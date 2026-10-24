//! C ABI bridge for RamLang's native compiler.
//!
//! This layer keeps compatibility APIs for direct handle-based use while native
//! async functions and text TCP/UDP requests use Rust-owned Future handles. The compiler's
//! current await bridge blocks the calling OS thread; resumable state-machine lowering remains
//! future work.

use corosensei::{Coroutine, CoroutineResult, Yielder};
use std::{
    alloc::{Layout, alloc as alloc_bytes, dealloc as dealloc_bytes},
    cell::Cell,
    ffi::{CStr, CString, c_char, c_void},
    io, ptr, slice,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    runtime::{Builder, Runtime},
    sync::{Notify, mpsc::UnboundedSender},
    task::LocalSet,
};

use crate::{TcpConnection, TcpServer, UdpEndpoint};

const MAX_FFI_TCP_FRAME_SIZE: usize = 16 * 1024 * 1024;

static ASYNC_REQUEST_RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();

/// State shared by the caller's request handle and its background Tokio task.
struct AsyncRequestState {
    result: Mutex<Option<Result<Vec<u8>, String>>>,
    ready: Condvar,
    abort_handle: Mutex<Option<tokio::task::AbortHandle>>,
}

impl AsyncRequestState {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            ready: Condvar::new(),
            abort_handle: Mutex::new(None),
        }
    }

    fn complete(&self, result: Result<Vec<u8>, String>) {
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot.is_none() {
            *slot = Some(result);
            self.ready.notify_all();
        }
    }

    fn set_abort_handle(&self, handle: tokio::task::AbortHandle) {
        *self
            .abort_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(handle);
    }

    fn cancel(&self, protocol: &str) {
        self.complete(Err(format!("{protocol} request cancelled")));
        let handle = self
            .abort_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(handle) = handle.as_ref() {
            handle.abort();
        }
    }
}

/// A cancellation scope that owns the lifetime policy for explicitly tracked requests.
///
/// A scope does not free request handles. Closing it cancels unfinished tracked work;
/// the caller must still close each TCP/UDP request handle it created.
pub struct RamlangTaskScope {
    requests: Mutex<Vec<(Arc<AsyncRequestState>, &'static str)>>,
}

impl RamlangTaskScope {
    fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
        }
    }

    fn track(&self, state: Arc<AsyncRequestState>, protocol: &'static str) -> Result<(), String> {
        let mut requests = self
            .requests
            .lock()
            .map_err(|_| "task scope lock is poisoned".to_owned())?;
        if !requests
            .iter()
            .any(|(existing, _)| Arc::ptr_eq(existing, &state))
        {
            requests.push((state, protocol));
        }
        Ok(())
    }

    fn cancel_all(&self) -> Result<(), String> {
        let requests = self
            .requests
            .lock()
            .map_err(|_| "task scope lock is poisoned".to_owned())?
            .clone();
        for (state, protocol) in requests {
            state.cancel(protocol);
        }
        Ok(())
    }
}

/// Opaque handle for a background TCP request.
///
/// Requests run on a shared multithread Tokio runtime. Dropping the handle through
/// ramlang_tcp_request_close cancels an unfinished request.
pub struct RamlangTcpRequest {
    state: Arc<AsyncRequestState>,
}

fn async_request_runtime() -> Result<&'static Runtime, String> {
    match ASYNC_REQUEST_RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|error| format!("could not create async request runtime: {error}"))
    }) {
        Ok(runtime) => Ok(runtime),
        Err(error) => Err(error.clone()),
    }
}

thread_local! {
    static LAST_ERROR: std::cell::RefCell<CString> =
        std::cell::RefCell::new(CString::new("").expect("empty C string is valid"));
}

/// Opaque TCP connection handle for C and RamLang callers.
///
/// The connection and runtime are kept together because Tokio I/O resources require
/// their originating runtime to remain alive. Field order is intentional: the
/// connection is dropped before the runtime.
pub struct RamlangTcpConnection {
    connection: Mutex<TcpConnection>,
    runtime: Runtime,
}

/// Handle for a native RamLang TCP listener. Cloned accept tasks retain shared state.
pub struct RamlangNativeTcpListener {
    inner: Arc<NativeTcpListenerInner>,
}

struct NativeTcpListenerInner {
    server: TcpServer,
    closed: tokio::sync::watch::Sender<bool>,
}

/// Handle for a native RamLang TCP connection returned by tcp_accept.
pub struct RamlangNativeTcpConnection {
    inner: Arc<tokio::sync::Mutex<TcpConnection>>,
}

unsafe extern "C" fn cleanup_native_tcp_listener(pointer: *mut c_void) {
    // SAFETY: the future owns the listener handle until ownership is transferred by await.
    unsafe { ramlang_tcp_listener_close(pointer.cast::<RamlangNativeTcpListener>()) };
}

unsafe extern "C" fn cleanup_native_tcp_connection(pointer: *mut c_void) {
    // SAFETY: the future owns the connection handle until ownership is transferred by await.
    unsafe { ramlang_tcp_native_close(pointer.cast::<RamlangNativeTcpConnection>()) };
}

/// Opaque handle for a native asynchronous UDP endpoint.
pub struct RamlangNativeUdpEndpoint {
    inner: Arc<UdpEndpoint>,
}

/// One text datagram returned by udp_recv. The peer address is retained for udp_send.
pub struct RamlangNativeUdpPacket {
    payload: String,
    peer: String,
}

unsafe extern "C" fn cleanup_native_udp_endpoint(pointer: *mut c_void) {
    // SAFETY: the future owns the UDP endpoint handle until pointer ownership is transferred.
    unsafe { ramlang_udp_native_close(pointer.cast::<RamlangNativeUdpEndpoint>()) };
}

unsafe extern "C" fn cleanup_native_udp_packet(pointer: *mut c_void) {
    // SAFETY: the future owns the packet handle until pointer ownership is transferred.
    unsafe { ramlang_udp_packet_close(pointer.cast::<RamlangNativeUdpPacket>()) };
}

/// Close a native UDP endpoint handle.
///
/// # Safety
///
/// A non-null handle must have been returned by udp_bind and must not be closed concurrently
/// with an operation that uses the same handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_native_close(endpoint: *mut RamlangNativeUdpEndpoint) {
    if !endpoint.is_null() {
        // SAFETY: pointer validity is required by this function's caller contract.
        drop(unsafe { Box::from_raw(endpoint) });
    }
    clear_last_error();
}

/// Close a received native UDP packet handle.
///
/// # Safety
///
/// A non-null handle must have been returned by a successful udp_recv await and must not be
/// closed concurrently with udp_packet_data or udp_packet_peer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_packet_close(packet: *mut RamlangNativeUdpPacket) {
    if !packet.is_null() {
        // SAFETY: pointer validity is required by this function's caller contract.
        drop(unsafe { Box::from_raw(packet) });
    }
    clear_last_error();
}

/// Bind a native UDP endpoint. Await the returned Future<ptr> to get its handle.
///
/// # Safety
///
/// address must be a valid readable NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_bind_future(
    address: *const c_char,
    timeout_ms: i64,
) -> *mut RamlangFuture {
    let address = match read_c_string(address, "UDP bind address") {
        Ok(address) => address,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    if timeout_ms <= 0 {
        set_last_error("UDP bind timeout must be greater than zero");
        return ptr::null_mut();
    }
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    let state = Arc::new(FutureState::new(Some(cleanup_native_udp_endpoint)));
    let task_state = Arc::clone(&state);
    let operation_timeout = Duration::from_millis(timeout_ms as u64);
    let task = runtime.spawn(async move {
        let result = async {
            let endpoint = UdpEndpoint::bind(&address, operation_timeout)
                .await
                .map_err(|error| error.to_string())?;
            let endpoint = Box::new(RamlangNativeUdpEndpoint {
                inner: Arc::new(endpoint),
            });
            Ok::<i64, String>(Box::into_raw(endpoint) as usize as i64)
        }
        .await;
        task_state.complete_result(result);
    });
    state.set_abort_handle(task.abort_handle());
    drop(task);
    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

/// Return a newly allocated local socket address string for a native UDP endpoint.
///
/// The returned string is owned by the caller and must be released with ramlang_free.
///
/// # Safety
///
/// endpoint must be a live handle returned by udp_bind, and it must not be closed concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_native_addr(
    endpoint: *mut RamlangNativeUdpEndpoint,
) -> *mut c_char {
    if endpoint.is_null() {
        set_last_error("native UDP endpoint handle must not be null");
        return ptr::null_mut();
    }
    // SAFETY: pointer validity is required by this function's caller contract.
    let endpoint = unsafe { &*endpoint };
    match endpoint.inner.local_addr() {
        Ok(address) => match allocate_runtime_string(&address.to_string()) {
            Ok(output) => {
                clear_last_error();
                output
            }
            Err(error) => {
                set_last_error(&error);
                ptr::null_mut()
            }
        },
        Err(error) => {
            set_last_error(&error.to_string());
            ptr::null_mut()
        }
    }
}

/// Receive one bounded UTF-8 UDP datagram. Await the result to obtain Future<ptr> packet handle.
///
/// Datagrams larger than 65,507 bytes or invalid UTF-8 datagrams complete with an error.
///
/// # Safety
///
/// endpoint must be a live handle returned by udp_bind and must not be closed concurrently
/// with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_recv_future(
    endpoint: *mut RamlangNativeUdpEndpoint,
) -> *mut RamlangFuture {
    if endpoint.is_null() {
        set_last_error("native UDP endpoint handle must not be null");
        return ptr::null_mut();
    }
    // SAFETY: pointer validity is required by this function's caller contract.
    let inner = Arc::clone(&unsafe { &*endpoint }.inner);
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    let state = Arc::new(FutureState::new(Some(cleanup_native_udp_packet)));
    let task_state = Arc::clone(&state);
    let task = runtime.spawn(async move {
        let result: Result<i64, String> = async {
            let (payload, peer) = inner
                .recv_from(65_507)
                .await
                .map_err(|error| error.to_string())?;
            let payload = String::from_utf8(payload)
                .map_err(|error| format!("UDP datagram is not valid UTF-8: {error}"))?;
            let packet = Box::new(RamlangNativeUdpPacket {
                payload,
                peer: peer.to_string(),
            });
            Ok(Box::into_raw(packet) as usize as i64)
        }
        .await;
        task_state.complete_result(result);
    });
    state.set_abort_handle(task.abort_handle());
    drop(task);
    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

/// Return an owned copy of a native UDP packet's UTF-8 payload.
///
/// Release the returned string with free(value as ptr).
///
/// # Safety
///
/// packet must be a live handle returned by a successful udp_recv await, and it must not be
/// closed concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_packet_data(
    packet: *mut RamlangNativeUdpPacket,
) -> *mut c_char {
    if packet.is_null() {
        set_last_error("native UDP packet handle must not be null");
        return ptr::null_mut();
    }
    // SAFETY: pointer validity is required by this function's caller contract.
    match allocate_runtime_string(&unsafe { &*packet }.payload) {
        Ok(output) => {
            clear_last_error();
            output
        }
        Err(error) => {
            set_last_error(&error);
            ptr::null_mut()
        }
    }
}

/// Return an owned copy of a native UDP packet's sender address.
///
/// Release the returned string with free(value as ptr).
///
/// # Safety
///
/// packet must be a live handle returned by a successful udp_recv await, and it must not be
/// closed concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_packet_peer(
    packet: *mut RamlangNativeUdpPacket,
) -> *mut c_char {
    if packet.is_null() {
        set_last_error("native UDP packet handle must not be null");
        return ptr::null_mut();
    }
    // SAFETY: pointer validity is required by this function's caller contract.
    match allocate_runtime_string(&unsafe { &*packet }.peer) {
        Ok(output) => {
            clear_last_error();
            output
        }
        Err(error) => {
            set_last_error(&error);
            ptr::null_mut()
        }
    }
}

/// Send one UTF-8 datagram and return Future<i64> containing the number of bytes sent.
///
/// # Safety
///
/// endpoint must be a live native UDP handle; peer and payload must be readable NUL-terminated
/// UTF-8 strings. The endpoint must not be closed concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_send_future(
    endpoint: *mut RamlangNativeUdpEndpoint,
    peer: *const c_char,
    payload: *const c_char,
) -> *mut RamlangFuture {
    if endpoint.is_null() {
        set_last_error("native UDP endpoint handle must not be null");
        return ptr::null_mut();
    }
    let peer = match read_c_string(peer, "UDP destination address") {
        Ok(peer) => peer,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    let payload = match read_c_string(payload, "UDP payload") {
        Ok(payload) => payload,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    if payload.len() > 65_507 {
        set_last_error("UDP payload exceeds 65,507 bytes");
        return ptr::null_mut();
    }
    // SAFETY: pointer validity is required by this function's caller contract.
    let inner = Arc::clone(&unsafe { &*endpoint }.inner);
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    let state = Arc::new(FutureState::new(None));
    let task_state = Arc::clone(&state);
    let task = runtime.spawn(async move {
        let result = inner
            .send_to(payload.as_bytes(), &peer)
            .await
            .map(|length| length as i64)
            .map_err(|error| error.to_string());
        task_state.complete_result(result);
    });
    state.set_abort_handle(task.abort_handle());
    drop(task);
    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

/// Close a native TCP listener and signal any future accept operation to stop.
///
/// # Safety
///
/// A non-null pointer must have been returned by the runtime and must not be closed
/// concurrently with any operation using the same handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_listener_close(listener: *mut RamlangNativeTcpListener) {
    if !listener.is_null() {
        // SAFETY: pointer validity is required by this function's caller contract.
        let listener = unsafe { Box::from_raw(listener) };
        listener.inner.closed.send_replace(true);
        drop(listener);
    }
    clear_last_error();
}

/// Close a native TCP connection handle.
///
/// # Safety
///
/// A non-null pointer must have been returned by the runtime and must not be closed
/// concurrently with any operation using the same handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_native_close(connection: *mut RamlangNativeTcpConnection) {
    if !connection.is_null() {
        // SAFETY: pointer validity is required by this function's caller contract.
        drop(unsafe { Box::from_raw(connection) });
    }
    clear_last_error();
}

fn allocate_runtime_string(value: &str) -> Result<*mut c_char, String> {
    let allocation_size = value
        .len()
        .checked_add(1)
        .ok_or_else(|| "runtime string allocation size overflow".to_owned())?;
    let output = ramlang_alloc(
        u64::try_from(allocation_size)
            .map_err(|_| "runtime string is too large for this platform".to_owned())?,
    );
    if output.is_null() {
        return Err("unable to allocate a RamLang string".to_owned());
    }
    // SAFETY: output holds value.len() + 1 writable bytes from ramlang_alloc.
    unsafe {
        ptr::copy_nonoverlapping(value.as_ptr(), output.cast::<u8>(), value.len());
        *output.cast::<u8>().add(value.len()) = 0;
    }
    Ok(output.cast::<c_char>())
}

/// Start binding a native TCP listener. Await the returned Future<ptr> to get its handle.
///
/// # Safety
///
/// address must be a valid readable NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_listen_future(
    address: *const c_char,
    timeout_ms: i64,
) -> *mut RamlangFuture {
    let address = match read_c_string(address, "TCP listen address") {
        Ok(address) => address,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    if timeout_ms <= 0 {
        set_last_error("TCP listen timeout must be greater than zero");
        return ptr::null_mut();
    }
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    let state = Arc::new(FutureState::new(Some(cleanup_native_tcp_listener)));
    let task_state = Arc::clone(&state);
    let operation_timeout = Duration::from_millis(timeout_ms as u64);
    let task = runtime.spawn(async move {
        let result = async {
            let server = TcpServer::bind(&address, operation_timeout)
                .await
                .map_err(|error| error.to_string())?;
            let (closed, _receiver) = tokio::sync::watch::channel(false);
            let listener = Box::new(RamlangNativeTcpListener {
                inner: Arc::new(NativeTcpListenerInner { server, closed }),
            });
            Ok::<i64, String>(Box::into_raw(listener) as usize as i64)
        }
        .await;
        task_state.complete_result(result);
    });
    state.set_abort_handle(task.abort_handle());
    drop(task);
    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

/// Return a newly allocated NUL-terminated string containing a listener's local address.
///
/// The caller owns the returned string and must release it using ramlang_free.
///
/// # Safety
///
/// listener must be a live handle returned by ramlang_tcp_listen_future, and it must not
/// be closed concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_listener_addr(
    listener: *mut RamlangNativeTcpListener,
) -> *mut c_char {
    if listener.is_null() {
        set_last_error("TCP listener handle must not be null");
        return ptr::null_mut();
    }
    // SAFETY: pointer validity is required by this function's caller contract.
    let listener = unsafe { &*listener };
    match listener.inner.server.local_addr() {
        Ok(address) => match allocate_runtime_string(&address.to_string()) {
            Ok(output) => {
                clear_last_error();
                output
            }
            Err(error) => {
                set_last_error(&error);
                ptr::null_mut()
            }
        },
        Err(error) => {
            set_last_error(&error.to_string());
            ptr::null_mut()
        }
    }
}

/// Start accepting one incoming native TCP connection. Await the Future<ptr> for its handle.
///
/// # Safety
///
/// listener must be a live handle returned by ramlang_tcp_listen_future and must not be closed
/// concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_accept_future(
    listener: *mut RamlangNativeTcpListener,
) -> *mut RamlangFuture {
    if listener.is_null() {
        set_last_error("TCP listener handle must not be null");
        return ptr::null_mut();
    }
    // SAFETY: pointer validity is required by this function's caller contract.
    let inner = Arc::clone(&unsafe { &*listener }.inner);
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    let state = Arc::new(FutureState::new(Some(cleanup_native_tcp_connection)));
    let task_state = Arc::clone(&state);
    let task = runtime.spawn(async move {
        let mut closed = inner.closed.subscribe();
        let result = if *closed.borrow() {
            Err("TCP listener is closed".to_owned())
        } else {
            tokio::select! {
                _ = closed.changed() => Err("TCP listener was closed while accepting".to_owned()),
                accepted = inner.server.accept() => {
                    accepted.map(|(connection, _peer)| {
                        let connection = Box::new(RamlangNativeTcpConnection {
                            inner: Arc::new(tokio::sync::Mutex::new(connection)),
                        });
                        Box::into_raw(connection) as usize as i64
                    }).map_err(|error| error.to_string())
                }
            }
        };
        task_state.complete_result(result);
    });
    state.set_abort_handle(task.abort_handle());
    drop(task);
    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

/// Write one framed UTF-8 message to a native TCP connection, returning Future<void>.
///
/// Errors are reported through last_error when the future is awaited.
///
/// # Safety
///
/// connection must be a live native TCP connection handle and payload must be a valid
/// readable NUL-terminated UTF-8 string. The connection must not be closed concurrently
/// with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_write_future(
    connection: *mut RamlangNativeTcpConnection,
    payload: *const c_char,
) -> *mut RamlangFuture {
    if connection.is_null() {
        set_last_error("TCP connection handle must not be null");
        return ptr::null_mut();
    }
    let payload = match read_c_string(payload, "TCP payload") {
        Ok(payload) => payload,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    // SAFETY: pointer validity is required by this function's caller contract.
    let inner = Arc::clone(&unsafe { &*connection }.inner);
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    let state = Arc::new(FutureState::new(None));
    let task_state = Arc::clone(&state);
    let task = runtime.spawn(async move {
        let result = inner
            .lock()
            .await
            .write_frame(payload.as_bytes())
            .await
            .map(|()| 0)
            .map_err(|error| error.to_string());
        task_state.complete_result(result);
    });
    state.set_abort_handle(task.abort_handle());
    drop(task);
    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

/// Read one framed UTF-8 message from a native TCP connection, returning Future<str>.
///
/// The caller owns the awaited response string and must release it with free(response as ptr).
///
/// # Safety
///
/// connection must be a live native TCP connection handle and must not be closed concurrently
/// with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_read_future(
    connection: *mut RamlangNativeTcpConnection,
) -> *mut RamlangFuture {
    if connection.is_null() {
        set_last_error("TCP connection handle must not be null");
        return ptr::null_mut();
    }
    // SAFETY: pointer validity is required by this function's caller contract.
    let inner = Arc::clone(&unsafe { &*connection }.inner);
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    let state = Arc::new(FutureState::new(Some(ramlang_free)));
    let task_state = Arc::clone(&state);
    let task = runtime.spawn(async move {
        let result: Result<i64, String> = async {
            let payload = inner
                .lock()
                .await
                .read_frame(MAX_FFI_TCP_FRAME_SIZE)
                .await
                .map_err(|error| error.to_string())?;
            let text = String::from_utf8(payload)
                .map_err(|error| format!("TCP response is not valid UTF-8: {error}"))?;
            let output = allocate_runtime_string(&text)? as usize as i64;
            Ok(output)
        }
        .await;
        task_state.complete_result(result);
    });
    state.set_abort_handle(task.abort_handle());
    drop(task);
    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

fn set_last_error(message: &str) {
    let sanitized = message.replace('\0', "�");
    let message = CString::new(sanitized)
        .unwrap_or_else(|_| CString::new("unknown RamLang runtime error").expect("valid literal"));
    LAST_ERROR.with(|slot| *slot.borrow_mut() = message);
}

fn clear_last_error() {
    set_last_error("");
}

fn fail_i64(error: impl std::fmt::Display) -> i64 {
    set_last_error(&error.to_string());
    -1
}

fn fail_i32(error: impl std::fmt::Display) -> i32 {
    set_last_error(&error.to_string());
    -1
}

fn read_c_string(value: *const c_char, label: &str) -> Result<String, String> {
    if value.is_null() {
        return Err(format!("{label} must not be null"));
    }
    // SAFETY: the caller contract for the exported function requires a valid,
    // NUL-terminated C string.
    let value = unsafe { CStr::from_ptr(value) };
    value
        .to_str()
        .map(str::to_owned)
        .map_err(|_| format!("{label} must be valid UTF-8"))
}

fn build_runtime() -> io::Result<Runtime> {
    Builder::new_current_thread().enable_all().build()
}

/// Return the most recent error for the calling OS thread.
///
/// The returned pointer stays valid until the next RamLang runtime call that changes
/// the error on this same thread. Callers should copy it before making another call.
#[unsafe(no_mangle)]
pub extern "C" fn ramlang_last_error() -> *const c_char {
    LAST_ERROR.with(|slot| slot.borrow().as_ptr())
}

/// Open a TCP connection.
///
/// Returns an opaque handle on success, or null on failure. timeout_ms must be
/// greater than zero. Use ramlang_last_error() to inspect failures.
///
/// # Safety
///
/// address must point to a valid NUL-terminated UTF-8 C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_connect(
    address: *const c_char,
    timeout_ms: u64,
) -> *mut RamlangTcpConnection {
    let address = match read_c_string(address, "address") {
        Ok(address) => address,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    if timeout_ms == 0 {
        set_last_error("timeout_ms must be greater than zero");
        return ptr::null_mut();
    }

    let runtime = match build_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&format!("could not create Tokio runtime: {error}"));
            return ptr::null_mut();
        }
    };

    let connection = match runtime.block_on(TcpConnection::connect(
        &address,
        Duration::from_millis(timeout_ms),
    )) {
        Ok(connection) => connection,
        Err(error) => {
            set_last_error(&error.to_string());
            return ptr::null_mut();
        }
    };

    clear_last_error();
    Box::into_raw(Box::new(RamlangTcpConnection {
        connection: Mutex::new(connection),
        runtime,
    }))
}

/// Write one length-prefixed TCP frame. Returns zero on success and -1 on error.
///
/// The frame is limited to the protocol's u32 length prefix. Close the connection if
/// a write fails.
///
/// # Safety
///
/// connection must be a live handle returned by ramlang_tcp_connect. If length
/// is nonzero, data must point to at least length readable bytes. Do not close the
/// handle concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_write_frame(
    connection: *mut RamlangTcpConnection,
    data: *const u8,
    length: u64,
) -> i64 {
    if connection.is_null() {
        return fail_i64("connection must not be null");
    }
    let length = match usize::try_from(length) {
        Ok(length) => length,
        Err(_) => return fail_i64("frame length does not fit this platform"),
    };
    if length > MAX_FFI_TCP_FRAME_SIZE {
        return fail_i64("TCP frame exceeds the C ABI maximum of 16 MiB");
    }
    if length > 0 && data.is_null() {
        return fail_i64("data must not be null when length is nonzero");
    }
    let payload = if length == 0 {
        &[][..]
    } else {
        // SAFETY: validated by the caller contract and null check above.
        unsafe { slice::from_raw_parts(data, length) }
    };

    // SAFETY: pointer validity is part of this function's documented contract.
    let handle = unsafe { &*connection };
    let mut stream = match handle.connection.lock() {
        Ok(stream) => stream,
        Err(_) => return fail_i64("TCP connection lock is poisoned"),
    };
    match handle.runtime.block_on(stream.write_frame(payload)) {
        Ok(()) => {
            clear_last_error();
            0
        }
        Err(error) => fail_i64(error),
    }
}

/// Read a length-prefixed TCP frame into the caller's buffer.
///
/// Returns the number of payload bytes written, or -1 on error. A frame larger than
/// capacity is rejected. On read errors, close the connection because a partial
/// frame may already have been consumed.
///
/// # Safety
///
/// connection must be a live handle returned by ramlang_tcp_connect. If capacity
/// is nonzero, output must point to at least capacity writable bytes. Do not close
/// the handle concurrently with this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_read_frame(
    connection: *mut RamlangTcpConnection,
    output: *mut u8,
    capacity: u64,
) -> i64 {
    if connection.is_null() {
        return fail_i64("connection must not be null");
    }
    let capacity = match usize::try_from(capacity) {
        Ok(capacity) => capacity,
        Err(_) => return fail_i64("buffer capacity does not fit this platform"),
    };
    if capacity > MAX_FFI_TCP_FRAME_SIZE {
        return fail_i64("TCP frame buffer exceeds the C ABI maximum of 16 MiB");
    }
    if capacity > 0 && output.is_null() {
        return fail_i64("output must not be null when capacity is nonzero");
    }

    // SAFETY: pointer validity is part of this function's documented contract.
    let handle = unsafe { &*connection };
    let mut stream = match handle.connection.lock() {
        Ok(stream) => stream,
        Err(_) => return fail_i64("TCP connection lock is poisoned"),
    };
    match handle.runtime.block_on(stream.read_frame(capacity)) {
        Ok(payload) => {
            if !payload.is_empty() {
                // SAFETY: the payload length is at most capacity, checked by read_frame.
                unsafe { ptr::copy_nonoverlapping(payload.as_ptr(), output, payload.len()) };
            }
            clear_last_error();
            i64::try_from(payload.len()).unwrap_or_else(|_| fail_i64("frame length exceeds i64"))
        }
        Err(error) => fail_i64(error),
    }
}

/// Start a TCP request on the shared background runtime without waiting for completion.
///
/// The function connects to address, writes one length-prefixed frame, then reads one
/// length-prefixed response frame. It returns an opaque request handle immediately.
/// Connect, write, and read each use timeout_ms as their individual timeout.
///
/// Returns null on validation/runtime setup failure. Use ramlang_last_error() for errors.
///
/// # Safety
///
/// address must be a valid NUL-terminated UTF-8 C string. If length is nonzero, data must
/// point to at least length readable bytes. Input strings and bytes are copied before return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_request_start(
    address: *const c_char,
    data: *const u8,
    length: u64,
    timeout_ms: u64,
) -> *mut RamlangTcpRequest {
    let address = match read_c_string(address, "address") {
        Ok(address) => address,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    if timeout_ms == 0 {
        set_last_error("timeout_ms must be greater than zero");
        return ptr::null_mut();
    }
    let length = match usize::try_from(length) {
        Ok(length) => length,
        Err(_) => {
            set_last_error("TCP request length does not fit this platform");
            return ptr::null_mut();
        }
    };
    if length > MAX_FFI_TCP_FRAME_SIZE {
        set_last_error("TCP request frame exceeds the C ABI maximum of 16 MiB");
        return ptr::null_mut();
    }
    if length > 0 && data.is_null() {
        set_last_error("data must not be null when length is nonzero");
        return ptr::null_mut();
    }
    let payload = if length == 0 {
        Vec::new()
    } else {
        // SAFETY: validated by the caller contract and null check above.
        unsafe { slice::from_raw_parts(data, length) }.to_vec()
    };

    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };

    let state = Arc::new(AsyncRequestState::new());
    let worker_state = Arc::clone(&state);
    let task = runtime.spawn(async move {
        let result = async {
            let mut connection =
                TcpConnection::connect(&address, Duration::from_millis(timeout_ms)).await?;
            connection.write_frame(&payload).await?;
            connection.read_frame(MAX_FFI_TCP_FRAME_SIZE).await
        }
        .await
        .map_err(|error: io::Error| error.to_string());

        worker_state.complete(result);
    });
    state.set_abort_handle(task.abort_handle());
    drop(task);

    clear_last_error();
    Box::into_raw(Box::new(RamlangTcpRequest { state }))
}

/// Return 1 when the request completed, 0 while it is still running, or -1 on error.
///
/// Completion includes failed requests. Use ramlang_tcp_request_read_result() to
/// retrieve the response or inspect the network error.
///
/// # Safety
///
/// request must be a live handle returned by ramlang_tcp_request_start.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_request_is_ready(request: *mut RamlangTcpRequest) -> i32 {
    if request.is_null() {
        set_last_error("request must not be null");
        return -1;
    }
    // SAFETY: pointer validity is part of this function's documented contract.
    let request = unsafe { &*request };
    match request.state.result.lock() {
        Ok(result) => {
            let ready = result.is_some();
            clear_last_error();
            if ready { 1 } else { 0 }
        }
        Err(_) => {
            set_last_error("TCP request result lock is poisoned");
            -1
        }
    }
}

/// Wait up to wait_timeout_ms for a TCP request to complete.
///
/// Returns 1 if completed, 0 if the wait timed out, or -1 on error. A timeout of zero
/// performs a non-blocking readiness check.
///
/// # Safety
///
/// request must be a live handle returned by ramlang_tcp_request_start.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_request_wait(
    request: *mut RamlangTcpRequest,
    wait_timeout_ms: u64,
) -> i32 {
    if request.is_null() {
        set_last_error("request must not be null");
        return -1;
    }
    // SAFETY: pointer validity is part of this function's documented contract.
    let request = unsafe { &*request };
    let total_wait = Duration::from_millis(wait_timeout_ms);
    let start = Instant::now();
    let mut result = match request.state.result.lock() {
        Ok(result) => result,
        Err(_) => {
            set_last_error("TCP request result lock is poisoned");
            return -1;
        }
    };

    loop {
        if result.is_some() {
            clear_last_error();
            return 1;
        }

        let remaining = total_wait.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            clear_last_error();
            return 0;
        }

        match request.state.ready.wait_timeout(result, remaining) {
            Ok((next, _)) => result = next,
            Err(_) => {
                set_last_error("TCP request wait lock is poisoned");
                return -1;
            }
        }
    }
}

/// Copy a completed response frame into caller-owned memory.
///
/// Returns the response byte count, or -1 if the request failed, has not completed, or
/// the output buffer is too small. The stored response remains available for repeated
/// reads. The supplied capacity must be at most 16 MiB.
///
/// # Safety
///
/// request must be a live handle returned by ramlang_tcp_request_start. If capacity is
/// nonzero, output must point to at least capacity writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_request_read_result(
    request: *mut RamlangTcpRequest,
    output: *mut u8,
    capacity: u64,
) -> i64 {
    if request.is_null() {
        return fail_i64("request must not be null");
    }
    let capacity = match usize::try_from(capacity) {
        Ok(capacity) => capacity,
        Err(_) => return fail_i64("response buffer capacity does not fit this platform"),
    };
    if capacity > MAX_FFI_TCP_FRAME_SIZE {
        return fail_i64("response buffer exceeds the C ABI maximum of 16 MiB");
    }
    if capacity > 0 && output.is_null() {
        return fail_i64("output must not be null when capacity is nonzero");
    }

    // SAFETY: pointer validity is part of this function's documented contract.
    let request = unsafe { &*request };
    let result = match request.state.result.lock() {
        Ok(result) => result,
        Err(_) => return fail_i64("TCP request result lock is poisoned"),
    };
    let response = match result.as_ref() {
        Some(Ok(response)) => response,
        Some(Err(error)) => return fail_i64(error),
        None => return fail_i64("TCP request has not completed"),
    };
    if response.len() > capacity {
        return fail_i64(format!(
            "response needs {} bytes but output capacity is {}",
            response.len(),
            capacity
        ));
    }
    if !response.is_empty() {
        // SAFETY: response length is at most capacity, checked above.
        unsafe { ptr::copy_nonoverlapping(response.as_ptr(), output, response.len()) };
    }
    clear_last_error();
    response.len() as i64
}

/// Cancel a pending request. Returns zero on success, or -1 on error.
///
/// A request that has already completed keeps its original result.
///
/// # Safety
///
/// request must be a live handle returned by ramlang_tcp_request_start.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_request_cancel(request: *mut RamlangTcpRequest) -> i32 {
    if request.is_null() {
        set_last_error("request must not be null");
        return -1;
    }
    // SAFETY: pointer validity is part of this function's documented contract.
    unsafe { &*request }.state.cancel("TCP");
    clear_last_error();
    0
}

/// Cancel unfinished work and release the caller's request handle. Passing null is a no-op.
///
/// # Safety
///
/// A non-null handle must have been returned by ramlang_tcp_request_start and must not
/// be used again or closed concurrently after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_request_close(request: *mut RamlangTcpRequest) {
    if !request.is_null() {
        // SAFETY: pointer validity is part of this function's documented contract.
        let request = unsafe { Box::from_raw(request) };
        request.state.cancel("TCP");
        drop(request);
    }
    clear_last_error();
}

/// Opaque handle for a background UDP request.
///
/// The request resolves the destination, binds an ephemeral local UDP socket, sends one
/// datagram, and receives one reply from the resolved destination. Work runs on the shared
/// multithread Tokio runtime and can be cancelled through the handle.
pub struct RamlangUdpRequest {
    state: Arc<AsyncRequestState>,
}

/// Start a UDP request in the background without waiting for its response.
///
/// The destination is resolved before sending. The request receives one datagram from the
/// selected destination address; data and destination are copied before this function returns.
/// Both the request payload and response capacity are bounded by 65,507 bytes. timeout_ms
/// must be greater than zero and applies separately to DNS lookup, bind, send, and receive.
///
/// Returns a request handle immediately, or null if validation or runtime setup fails.
///
/// # Safety
///
/// destination must be a valid NUL-terminated UTF-8 C string. If length is nonzero, data
/// must point to at least length readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_request_start(
    destination: *const c_char,
    data: *const u8,
    length: u64,
    response_capacity: u64,
    timeout_ms: u64,
) -> *mut RamlangUdpRequest {
    let destination = match read_c_string(destination, "destination") {
        Ok(destination) => destination,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    if timeout_ms == 0 {
        set_last_error("timeout_ms must be greater than zero");
        return ptr::null_mut();
    }
    let length = match usize::try_from(length) {
        Ok(length) => length,
        Err(_) => {
            set_last_error("UDP request length does not fit this platform");
            return ptr::null_mut();
        }
    };
    let response_capacity = match usize::try_from(response_capacity) {
        Ok(capacity) => capacity,
        Err(_) => {
            set_last_error("UDP response capacity does not fit this platform");
            return ptr::null_mut();
        }
    };
    if length > MAX_FFI_UDP_PAYLOAD {
        set_last_error("UDP request payload exceeds 65,507 bytes");
        return ptr::null_mut();
    }
    if response_capacity > MAX_FFI_UDP_PAYLOAD {
        set_last_error("UDP response capacity exceeds 65,507 bytes");
        return ptr::null_mut();
    }
    if length > 0 && data.is_null() {
        set_last_error("data must not be null when length is nonzero");
        return ptr::null_mut();
    }

    let payload = if length == 0 {
        Vec::new()
    } else {
        // SAFETY: validated by the caller contract and null check above.
        unsafe { slice::from_raw_parts(data, length) }.to_vec()
    };
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };

    let state = Arc::new(AsyncRequestState::new());
    let worker_state = Arc::clone(&state);
    let task = runtime.spawn(async move {
        let result = async {
            let mut addresses = tokio::time::timeout(
                Duration::from_millis(timeout_ms),
                tokio::net::lookup_host(destination.as_str()),
            )
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "UDP DNS lookup timed out"))??;
            let target = addresses.next().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "UDP destination resolved to no addresses",
                )
            })?;
            let bind_address = if target.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            };
            let endpoint =
                UdpEndpoint::bind(bind_address, Duration::from_millis(timeout_ms)).await?;
            endpoint.send_to(&payload, &target.to_string()).await?;
            let (response, peer) = endpoint.recv_from(response_capacity).await?;
            if peer != target {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "UDP response came from an unexpected peer",
                ));
            }
            Ok::<Vec<u8>, io::Error>(response)
        }
        .await
        .map_err(|error| error.to_string());

        worker_state.complete(result);
    });
    state.set_abort_handle(task.abort_handle());
    drop(task);

    clear_last_error();
    Box::into_raw(Box::new(RamlangUdpRequest { state }))
}

/// Return 1 when a UDP request has completed, 0 while pending, or -1 on error.
///
/// # Safety
///
/// request must be a live handle returned by ramlang_udp_request_start.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_request_is_ready(request: *mut RamlangUdpRequest) -> i32 {
    if request.is_null() {
        set_last_error("request must not be null");
        return -1;
    }
    // SAFETY: pointer validity is part of the documented caller contract.
    let request = unsafe { &*request };
    match request.state.result.lock() {
        Ok(result) => {
            let ready = result.is_some();
            clear_last_error();
            if ready { 1 } else { 0 }
        }
        Err(_) => {
            set_last_error("UDP request result lock is poisoned");
            -1
        }
    }
}

/// Wait up to wait_timeout_ms for a UDP request to complete.
///
/// Returns 1 when complete, 0 if only the wait timed out, or -1 on error. A zero timeout
/// performs a non-blocking readiness check.
///
/// # Safety
///
/// request must be a live handle returned by ramlang_udp_request_start.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_request_wait(
    request: *mut RamlangUdpRequest,
    wait_timeout_ms: u64,
) -> i32 {
    if request.is_null() {
        set_last_error("request must not be null");
        return -1;
    }
    // SAFETY: pointer validity is part of the documented caller contract.
    let request = unsafe { &*request };
    let total_wait = Duration::from_millis(wait_timeout_ms);
    let start = Instant::now();
    let mut result = match request.state.result.lock() {
        Ok(result) => result,
        Err(_) => {
            set_last_error("UDP request result lock is poisoned");
            return -1;
        }
    };

    loop {
        if result.is_some() {
            clear_last_error();
            return 1;
        }
        let remaining = total_wait.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            clear_last_error();
            return 0;
        }
        match request.state.ready.wait_timeout(result, remaining) {
            Ok((next, _)) => result = next,
            Err(_) => {
                set_last_error("UDP request wait lock is poisoned");
                return -1;
            }
        }
    }
}

/// Copy the completed UDP response into caller-owned memory.
///
/// Returns the response length or -1 for pending requests, failed requests, or insufficient
/// capacity. The saved response can be read again after an insufficient-buffer error.
///
/// # Safety
///
/// request must be a live handle returned by ramlang_udp_request_start. If capacity is
/// nonzero, output must point to at least capacity writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_request_read_result(
    request: *mut RamlangUdpRequest,
    output: *mut u8,
    capacity: u64,
) -> i64 {
    if request.is_null() {
        return fail_i64("request must not be null");
    }
    let capacity = match usize::try_from(capacity) {
        Ok(capacity) => capacity,
        Err(_) => return fail_i64("UDP response buffer capacity does not fit this platform"),
    };
    if capacity > MAX_FFI_UDP_PAYLOAD {
        return fail_i64("UDP response buffer exceeds 65,507 bytes");
    }
    if capacity > 0 && output.is_null() {
        return fail_i64("output must not be null when capacity is nonzero");
    }
    // SAFETY: pointer validity is part of the documented caller contract.
    let request = unsafe { &*request };
    let result = match request.state.result.lock() {
        Ok(result) => result,
        Err(_) => return fail_i64("UDP request result lock is poisoned"),
    };
    let response = match result.as_ref() {
        Some(Ok(response)) => response,
        Some(Err(error)) => return fail_i64(error),
        None => return fail_i64("UDP request has not completed"),
    };
    if response.len() > capacity {
        return fail_i64(format!(
            "UDP response needs {} bytes but output capacity is {}",
            response.len(),
            capacity
        ));
    }
    if !response.is_empty() {
        // SAFETY: response length is at most capacity, checked above.
        unsafe { ptr::copy_nonoverlapping(response.as_ptr(), output, response.len()) };
    }
    clear_last_error();
    response.len() as i64
}

/// Cancel a pending UDP request. Returns zero on success or -1 for an invalid handle.
///
/// # Safety
///
/// request must be a live handle returned by ramlang_udp_request_start.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_request_cancel(request: *mut RamlangUdpRequest) -> i32 {
    if request.is_null() {
        set_last_error("request must not be null");
        return -1;
    }
    // SAFETY: pointer validity is part of the documented caller contract.
    unsafe { &*request }.state.cancel("UDP");
    clear_last_error();
    0
}

/// Cancel unfinished work and release a UDP request handle. Passing null is a no-op.
///
/// # Safety
///
/// A non-null handle must have been returned by ramlang_udp_request_start and must not
/// be used again or closed concurrently after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_request_close(request: *mut RamlangUdpRequest) {
    if !request.is_null() {
        // SAFETY: pointer validity is part of the documented caller contract.
        let request = unsafe { Box::from_raw(request) };
        request.state.cancel("UDP");
        drop(request);
    }
    clear_last_error();
}

/// Create an empty cancellation scope for background TCP and UDP requests.
///
/// # Safety
///
/// This function has no pointer preconditions.
#[unsafe(no_mangle)]
pub extern "C" fn ramlang_task_scope_new() -> *mut RamlangTaskScope {
    clear_last_error();
    Box::into_raw(Box::new(RamlangTaskScope::new()))
}

/// Track a TCP request so closing its scope cancels it if it is still pending.
///
/// A request can be tracked more than once without duplicate entries. The scope does not
/// own/free the opaque request handle; the caller must still close it.
///
/// # Safety
///
/// scope and request must be live handles returned by ramlang_task_scope_new and
/// ramlang_tcp_request_start, respectively, and must not be closed concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_task_scope_track_tcp(
    scope: *mut RamlangTaskScope,
    request: *mut RamlangTcpRequest,
) -> i32 {
    if scope.is_null() || request.is_null() {
        set_last_error("scope and request must not be null");
        return -1;
    }
    // SAFETY: pointer validity is covered by this function's caller contract.
    let scope = unsafe { &*scope };
    let request = unsafe { &*request };
    match scope.track(Arc::clone(&request.state), "TCP") {
        Ok(()) => {
            clear_last_error();
            0
        }
        Err(error) => {
            set_last_error(&error);
            -1
        }
    }
}

/// Track a UDP request so closing its scope cancels it if it is still pending.
///
/// The scope does not own/free the opaque request handle; the caller must still close it.
///
/// # Safety
///
/// scope and request must be live handles returned by ramlang_task_scope_new and
/// ramlang_udp_request_start, respectively, and must not be closed concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_task_scope_track_udp(
    scope: *mut RamlangTaskScope,
    request: *mut RamlangUdpRequest,
) -> i32 {
    if scope.is_null() || request.is_null() {
        set_last_error("scope and request must not be null");
        return -1;
    }
    // SAFETY: pointer validity is covered by this function's caller contract.
    let scope = unsafe { &*scope };
    let request = unsafe { &*request };
    match scope.track(Arc::clone(&request.state), "UDP") {
        Ok(()) => {
            clear_last_error();
            0
        }
        Err(error) => {
            set_last_error(&error);
            -1
        }
    }
}

/// Cancel all unfinished requests tracked by a scope. Returns zero on success or -1 on error.
///
/// The scope remains live after cancellation, and more requests may be tracked afterwards.
///
/// # Safety
///
/// scope must be a live handle returned by ramlang_task_scope_new and must not be closed
/// concurrently with this function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_task_scope_cancel(scope: *mut RamlangTaskScope) -> i32 {
    if scope.is_null() {
        set_last_error("scope must not be null");
        return -1;
    }
    // SAFETY: pointer validity is covered by this function's caller contract.
    match unsafe { &*scope }.cancel_all() {
        Ok(()) => {
            clear_last_error();
            0
        }
        Err(error) => {
            set_last_error(&error);
            -1
        }
    }
}

/// Return the number of unique request handles tracked by this scope.
///
/// # Safety
///
/// scope must be a live handle returned by ramlang_task_scope_new and must not be closed
/// concurrently with this function.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_task_scope_len(scope: *mut RamlangTaskScope) -> i64 {
    if scope.is_null() {
        return fail_i64("scope must not be null");
    }
    // SAFETY: pointer validity is covered by this function's caller contract.
    match unsafe { &*scope }.requests.lock() {
        Ok(requests) => {
            clear_last_error();
            requests.len() as i64
        }
        Err(_) => fail_i64("task scope lock is poisoned"),
    }
}

/// Close a cancellation scope, cancelling all unfinished tracked requests. Null is a no-op.
///
/// This does not free the request handles themselves; close each tracked request handle
/// after the scope is closed. A handle already completed keeps its original result.
///
/// # Safety
///
/// A non-null scope must have been returned by ramlang_task_scope_new and must not be
/// used again or closed concurrently after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_task_scope_close(scope: *mut RamlangTaskScope) {
    if !scope.is_null() {
        // SAFETY: pointer validity is covered by this function's caller contract.
        let scope = unsafe { Box::from_raw(scope) };
        let _ = scope.cancel_all();
        drop(scope);
    }
    clear_last_error();
}

/// Close a TCP handle. Passing null is a no-op.
///
/// # Safety
///
/// A non-null handle must have been returned by ramlang_tcp_connect and must not be
/// used again or closed concurrently after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_close(connection: *mut RamlangTcpConnection) {
    if !connection.is_null() {
        // SAFETY: pointer validity is part of this function's documented contract.
        drop(unsafe { Box::from_raw(connection) });
    }
    clear_last_error();
}

const MAX_FFI_UDP_PAYLOAD: usize = 65_507;

/// Opaque UDP endpoint handle for C and RamLang callers.
///
/// The endpoint is dropped before its runtime so Tokio's I/O registration remains alive
/// for the endpoint's full lifetime.
pub struct RamlangUdpEndpoint {
    endpoint: Mutex<UdpEndpoint>,
    runtime: Runtime,
}

/// Bind a local UDP endpoint. Returns null on failure.
///
/// The timeout must be greater than zero and applies to each send or receive operation.
/// Use ramlang_last_error() to inspect failures.
///
/// # Safety
///
/// local_address must point to a valid NUL-terminated UTF-8 C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_bind(
    local_address: *const c_char,
    timeout_ms: u64,
) -> *mut RamlangUdpEndpoint {
    let local_address = match read_c_string(local_address, "local_address") {
        Ok(address) => address,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    if timeout_ms == 0 {
        set_last_error("timeout_ms must be greater than zero");
        return ptr::null_mut();
    }

    let runtime = match build_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&format!("could not create Tokio runtime: {error}"));
            return ptr::null_mut();
        }
    };
    let endpoint = match runtime.block_on(UdpEndpoint::bind(
        &local_address,
        Duration::from_millis(timeout_ms),
    )) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            set_last_error(&error.to_string());
            return ptr::null_mut();
        }
    };

    clear_last_error();
    Box::into_raw(Box::new(RamlangUdpEndpoint {
        endpoint: Mutex::new(endpoint),
        runtime,
    }))
}

/// Write the bound endpoint address as a NUL-terminated string.
///
/// Returns its length excluding the NUL byte, or -1 on error. The output capacity must
/// include space for the trailing NUL.
///
/// # Safety
///
/// endpoint must be a live handle returned by ramlang_udp_bind. If capacity is nonzero,
/// output must point to at least capacity writable bytes. Do not close the handle while
/// another thread is using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_local_addr(
    endpoint: *mut RamlangUdpEndpoint,
    output: *mut c_char,
    capacity: u64,
) -> i64 {
    if endpoint.is_null() {
        return fail_i64("endpoint must not be null");
    }
    let capacity = match usize::try_from(capacity) {
        Ok(capacity) => capacity,
        Err(_) => return fail_i64("address buffer capacity does not fit this platform"),
    };
    if capacity == 0 || output.is_null() {
        return fail_i64("address output buffer must not be null and have nonzero capacity");
    }

    // SAFETY: pointer validity is part of this function's documented contract.
    let handle = unsafe { &*endpoint };
    let socket = match handle.endpoint.lock() {
        Ok(socket) => socket,
        Err(_) => return fail_i64("UDP endpoint lock is poisoned"),
    };
    let address = match socket.local_addr() {
        Ok(address) => address.to_string(),
        Err(error) => return fail_i64(error),
    };
    if address.len() + 1 > capacity {
        return fail_i64("address output buffer is too small");
    }

    // SAFETY: output has capacity bytes, checked above, and address contains no NUL.
    unsafe {
        ptr::copy_nonoverlapping(address.as_ptr(), output.cast::<u8>(), address.len());
        *output.add(address.len()) = 0;
    }
    clear_last_error();
    address.len() as i64
}

/// Send a single UDP datagram. Returns the sent byte count, or -1 on error.
///
/// Payloads are limited to 65,507 bytes, the maximum IPv4 UDP payload size.
///
/// # Safety
///
/// endpoint must be a live handle returned by ramlang_udp_bind. destination must be a
/// valid NUL-terminated UTF-8 C string. If length is nonzero, data must point to at
/// least length readable bytes. Do not close the handle while another thread is using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_send_to(
    endpoint: *mut RamlangUdpEndpoint,
    data: *const u8,
    length: u64,
    destination: *const c_char,
) -> i64 {
    if endpoint.is_null() {
        return fail_i64("endpoint must not be null");
    }
    let length = match usize::try_from(length) {
        Ok(length) => length,
        Err(_) => return fail_i64("datagram length does not fit this platform"),
    };
    if length > MAX_FFI_UDP_PAYLOAD {
        return fail_i64("UDP payload exceeds 65,507 bytes");
    }
    if length > 0 && data.is_null() {
        return fail_i64("data must not be null when length is nonzero");
    }
    let destination = match read_c_string(destination, "destination") {
        Ok(address) => address,
        Err(error) => {
            set_last_error(&error);
            return -1;
        }
    };
    let payload = if length == 0 {
        &[][..]
    } else {
        // SAFETY: validated by the caller contract and null check above.
        unsafe { slice::from_raw_parts(data, length) }
    };

    // SAFETY: pointer validity is part of this function's documented contract.
    let handle = unsafe { &*endpoint };
    let socket = match handle.endpoint.lock() {
        Ok(socket) => socket,
        Err(_) => return fail_i64("UDP endpoint lock is poisoned"),
    };
    match handle
        .runtime
        .block_on(socket.send_to(payload, &destination))
    {
        Ok(sent) => {
            clear_last_error();
            sent as i64
        }
        Err(error) => fail_i64(error),
    }
}

/// Receive one UDP datagram and write the sender address to peer_output.
///
/// Returns the received byte count, or -1 on error. capacity must be no greater than
/// 65,507 bytes. peer_capacity must be large enough for a NUL-terminated socket address
/// (64 bytes is sufficient for IPv4 and IPv6 textual addresses).
///
/// # Safety
///
/// endpoint must be a live handle returned by ramlang_udp_bind. If capacity is nonzero,
/// output must point to at least capacity writable bytes. peer_output must point to at
/// least peer_capacity writable bytes and peer_capacity must be nonzero. Do not close
/// the handle while another thread is using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_recv_from(
    endpoint: *mut RamlangUdpEndpoint,
    output: *mut u8,
    capacity: u64,
    peer_output: *mut c_char,
    peer_capacity: u64,
) -> i64 {
    if endpoint.is_null() {
        return fail_i64("endpoint must not be null");
    }
    let capacity = match usize::try_from(capacity) {
        Ok(capacity) => capacity,
        Err(_) => return fail_i64("datagram buffer capacity does not fit this platform"),
    };
    if capacity > MAX_FFI_UDP_PAYLOAD {
        return fail_i64("UDP buffer capacity exceeds 65,507 bytes");
    }
    if capacity > 0 && output.is_null() {
        return fail_i64("datagram output must not be null when capacity is nonzero");
    }
    let peer_capacity = match usize::try_from(peer_capacity) {
        Ok(capacity) => capacity,
        Err(_) => return fail_i64("peer address capacity does not fit this platform"),
    };
    if peer_capacity == 0 || peer_output.is_null() {
        return fail_i64("peer address output must not be null and have nonzero capacity");
    }

    // SAFETY: pointer validity is part of this function's documented contract.
    let handle = unsafe { &*endpoint };
    let socket = match handle.endpoint.lock() {
        Ok(socket) => socket,
        Err(_) => return fail_i64("UDP endpoint lock is poisoned"),
    };
    match handle.runtime.block_on(socket.recv_from(capacity)) {
        Ok((payload, peer)) => {
            let peer = peer.to_string();
            if peer.len() + 1 > peer_capacity {
                return fail_i64("peer address output buffer is too small");
            }
            if !payload.is_empty() {
                // SAFETY: payload length is at most capacity, enforced by recv_from.
                unsafe { ptr::copy_nonoverlapping(payload.as_ptr(), output, payload.len()) };
            }
            // SAFETY: peer address output has sufficient capacity, checked above.
            unsafe {
                ptr::copy_nonoverlapping(peer.as_ptr(), peer_output.cast::<u8>(), peer.len());
                *peer_output.add(peer.len()) = 0;
            }
            clear_last_error();
            payload.len() as i64
        }
        Err(error) => fail_i64(error),
    }
}

/// Close a UDP endpoint. Passing null is a no-op.
///
/// # Safety
///
/// A non-null endpoint must have been returned by ramlang_udp_bind and must not be
/// used again or closed concurrently after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_close(endpoint: *mut RamlangUdpEndpoint) {
    if !endpoint.is_null() {
        // SAFETY: pointer validity is part of this function's documented contract.
        drop(unsafe { Box::from_raw(endpoint) });
    }
    clear_last_error();
}

fn write_stdout_line(write_value: impl FnOnce(&mut dyn std::io::Write) -> std::io::Result<()>) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    if write_value(&mut output).is_ok() {
        let _ = output.write_all(b"\n");
    }
}

/// Print a signed 32-bit integer using the RamLang runtime, without a C standard-library call.
#[unsafe(no_mangle)]
pub extern "C" fn ramlang_print_i32(value: i32) {
    write_stdout_line(|output| write!(output, "{value}"));
}

/// Print a signed 64-bit integer using the RamLang runtime.
#[unsafe(no_mangle)]
pub extern "C" fn ramlang_print_i64(value: i64) {
    write_stdout_line(|output| write!(output, "{value}"));
}

/// Print an unsigned 32-bit integer using the RamLang runtime.
#[unsafe(no_mangle)]
pub extern "C" fn ramlang_print_u32(value: u32) {
    write_stdout_line(|output| write!(output, "{value}"));
}

/// Print an unsigned 64-bit integer using the RamLang runtime.
#[unsafe(no_mangle)]
pub extern "C" fn ramlang_print_u64(value: u64) {
    write_stdout_line(|output| write!(output, "{value}"));
}

fn trim_decimal_zeros(value: &str) -> String {
    let mut output = value.to_owned();
    if output.contains('.') {
        while output.ends_with('0') {
            output.pop();
        }
        if output.ends_with('.') {
            output.pop();
        }
    }
    output
}

/// Format like the C printf %g default: six significant digits, switching to scientific
/// notation when the rounded exponent is less than -4 or at least 6.
fn format_general_float(value: f64) -> String {
    if value.is_nan() {
        return if value.is_sign_negative() {
            "-nan"
        } else {
            "nan"
        }
        .to_owned();
    }
    if value.is_infinite() {
        return if value.is_sign_negative() {
            "-inf"
        } else {
            "inf"
        }
        .to_owned();
    }
    if value == 0.0 {
        return if value.is_sign_negative() { "-0" } else { "0" }.to_owned();
    }

    // Round first, then select fixed/scientific notation from the rounded exponent. This
    // handles values such as 999_999.9 correctly: six significant digits round to 1e+06.
    let scientific = format!("{value:.5e}");
    let (mantissa, exponent_text) = scientific
        .split_once('e')
        .expect("Rust scientific float formatting includes an exponent");
    let exponent = exponent_text
        .parse::<i32>()
        .expect("Rust scientific float exponent is an integer");

    if !(-4..6).contains(&exponent) {
        let mantissa = trim_decimal_zeros(mantissa);
        format!("{mantissa}e{exponent:+03}")
    } else {
        let decimals = (5 - exponent).max(0) as usize;
        trim_decimal_zeros(&format!("{value:.decimals$}"))
    }
}

/// Print a floating-point value using six significant digits, matching the former %g built-in.
#[unsafe(no_mangle)]
pub extern "C" fn ramlang_print_f64(value: f64) {
    write_stdout_line(|output| write!(output, "{}", format_general_float(value)));
}

/// Print a NUL-terminated byte string, followed by a newline.
///
/// # Safety
/// A non-null value must point to a readable NUL-terminated byte sequence.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_print_str(value: *const c_char) {
    if value.is_null() {
        set_last_error("print string must not be null");
        return;
    }
    // SAFETY: the pointer precondition is part of this function's caller contract.
    let bytes = unsafe { CStr::from_ptr(value) }.to_bytes();
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    if output.write_all(bytes).is_ok() {
        let _ = output.write_all(b"\n");
        clear_last_error();
    } else {
        set_last_error("failed to write string to stdout");
    }
}

/// Allocate runtime-owned memory for generated RamLang data frames.
///
/// The allocation is aligned to 16 bytes and must be released with ramlang_free.
/// Returns null and sets ramlang_last_error on allocation/layout failure.
#[unsafe(no_mangle)]
pub extern "C" fn ramlang_alloc(size: u64) -> *mut c_void {
    let Ok(payload_size) = usize::try_from(size) else {
        set_last_error("allocation size does not fit this platform");
        return ptr::null_mut();
    };
    let payload_size = payload_size.max(1);
    let Some(total_size) = payload_size.checked_add(16) else {
        set_last_error("allocation size overflow");
        return ptr::null_mut();
    };
    let Ok(layout) = Layout::from_size_align(total_size, 16) else {
        set_last_error("invalid runtime allocation layout");
        return ptr::null_mut();
    };
    // SAFETY: the layout is valid and the pointer is checked before being written.
    let base = unsafe { alloc_bytes(layout) };
    if base.is_null() {
        set_last_error("runtime allocation failed");
        return ptr::null_mut();
    }
    // Store the actual allocation payload size in the 16-byte header.
    // SAFETY: the allocation is at least 16 bytes, so a usize write is in bounds.
    unsafe { base.cast::<usize>().write(payload_size) };
    clear_last_error();
    // SAFETY: total_size includes this header.
    unsafe { base.add(16).cast::<c_void>() }
}

/// Release memory returned by ramlang_alloc. Null is a no-op.
///
/// # Safety
/// A non-null pointer must be a live allocation returned by ramlang_alloc and must not
/// be used again after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_free(pointer: *mut c_void) {
    if pointer.is_null() {
        clear_last_error();
        return;
    }
    // SAFETY: pointer validity and provenance are part of this function's contract.
    let base = unsafe { pointer.cast::<u8>().sub(16) };
    // SAFETY: ramlang_alloc initialized the size word in this allocation header.
    let payload_size = unsafe { base.cast::<usize>().read() };
    let Some(total_size) = payload_size.checked_add(16) else {
        set_last_error("runtime allocation header is invalid");
        return;
    };
    let Ok(layout) = Layout::from_size_align(total_size, 16) else {
        set_last_error("runtime allocation header is invalid");
        return;
    };
    // SAFETY: base and layout match the allocation created by ramlang_alloc.
    unsafe { dealloc_bytes(base, layout) };
    clear_last_error();
}

/// C-free RamLang allocator entry points used by generated LLVM modules.
///
/// These wrappers deliberately have project-specific names so the generated executable
/// doesn't interpose the process-wide libc malloc/free symbols.
#[unsafe(no_mangle)]
pub extern "C" fn ramlang_malloc(size: usize) -> *mut c_void {
    ramlang_alloc(size as u64)
}

#[unsafe(no_mangle)]
pub extern "C" fn ramlang_calloc(count: usize, size: usize) -> *mut c_void {
    let Some(total) = count.checked_mul(size) else {
        set_last_error("calloc size overflow");
        return ptr::null_mut();
    };
    let pointer = ramlang_alloc(total as u64);
    if !pointer.is_null() {
        // ramlang_alloc returns at least one writable byte for a zero-size allocation.
        // SAFETY: the allocation has max(total, 1) bytes available.
        unsafe { ptr::write_bytes(pointer.cast::<u8>(), 0, total.max(1)) };
    }
    pointer
}

/// Reallocate a pointer created by the RamLang allocator, preserving up to min(old, new) bytes.
///
/// # Safety
///
/// A non-null pointer must be a live allocation returned by ramlang_malloc/ramlang_alloc and
/// must not have been freed or passed to another reallocation. No thread may access the
/// allocation concurrently while it is being reallocated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_realloc(pointer: *mut c_void, new_size: usize) -> *mut c_void {
    if pointer.is_null() {
        return ramlang_malloc(new_size);
    }
    if new_size == 0 {
        // SAFETY: pointer validity is part of the realloc caller contract.
        unsafe { ramlang_free(pointer) };
        return ptr::null_mut();
    }
    // SAFETY: a non-null pointer must be a live ramlang allocator allocation.
    let old_base = unsafe { pointer.cast::<u8>().sub(16) };
    // SAFETY: ramlang_alloc stores its payload size at the start of the allocation header.
    let old_size = unsafe { old_base.cast::<usize>().read() };
    let replacement = ramlang_malloc(new_size);
    if replacement.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: both allocations are live and copy length is bounded by both capacities.
    unsafe {
        ptr::copy_nonoverlapping(
            pointer.cast::<u8>(),
            replacement.cast::<u8>(),
            old_size.min(new_size),
        );
        ramlang_free(pointer);
    }
    replacement
}

/// Copy count bytes from source to destination. The ranges must not overlap.
///
/// # Safety
///
/// When count is nonzero, source must be readable for count bytes and destination must be
/// writable for count bytes. The two ranges must not overlap. Null pointers are permitted
/// only when count is zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_memcpy(
    destination: *mut c_void,
    source: *const c_void,
    count: usize,
) -> *mut c_void {
    if count == 0 {
        return destination;
    }
    if destination.is_null() || source.is_null() {
        set_last_error("memcpy pointers must not be null when count is nonzero");
        return ptr::null_mut();
    }
    // SAFETY: both pointer ranges must be valid and non-overlapping for count bytes.
    unsafe {
        ptr::copy_nonoverlapping(source.cast::<u8>(), destination.cast::<u8>(), count);
    }
    clear_last_error();
    destination
}

/// Copy count bytes from source to destination; overlap is permitted.
///
/// # Safety
///
/// When count is nonzero, source must be readable for count bytes and destination must be
/// writable for count bytes. Null pointers are permitted only when count is zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_memmove(
    destination: *mut c_void,
    source: *const c_void,
    count: usize,
) -> *mut c_void {
    if count == 0 {
        return destination;
    }
    if destination.is_null() || source.is_null() {
        set_last_error("memmove pointers must not be null when count is nonzero");
        return ptr::null_mut();
    }
    // SAFETY: both pointer ranges must be valid for count bytes; overlap is permitted.
    unsafe { ptr::copy(source.cast::<u8>(), destination.cast::<u8>(), count) };
    clear_last_error();
    destination
}

/// Fill count destination bytes with the low eight bits of byte.
///
/// # Safety
///
/// When count is nonzero, destination must be writable for count bytes. A null pointer is
/// permitted only when count is zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_memset(
    destination: *mut c_void,
    byte: i32,
    count: usize,
) -> *mut c_void {
    if count == 0 {
        return destination;
    }
    if destination.is_null() {
        set_last_error("memset destination must not be null when count is nonzero");
        return ptr::null_mut();
    }
    // SAFETY: destination points to count writable bytes.
    unsafe { ptr::write_bytes(destination.cast::<u8>(), byte as u8, count) };
    clear_last_error();
    destination
}

/// Compare two byte ranges lexicographically.
///
/// # Safety
///
/// When count is nonzero, both pointers must be readable for count bytes. Null pointers are
/// permitted only when count is zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_memcmp(
    left: *const c_void,
    right: *const c_void,
    count: usize,
) -> i32 {
    if count == 0 {
        return 0;
    }
    if left.is_null() || right.is_null() {
        set_last_error("memcmp pointers must not be null when count is nonzero");
        return 0;
    }
    // SAFETY: both pointer ranges must be readable for count bytes.
    let left = unsafe { slice::from_raw_parts(left.cast::<u8>(), count) };
    // SAFETY: both pointer ranges must be readable for count bytes.
    let right = unsafe { slice::from_raw_parts(right.cast::<u8>(), count) };
    clear_last_error();
    for (a, b) in left.iter().zip(right) {
        if a != b {
            return i32::from(*a) - i32::from(*b);
        }
    }
    0
}

/// Return the byte length of a NUL-terminated C string.
///
/// # Safety
///
/// A non-null value must point to a readable NUL-terminated byte sequence.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_strlen(value: *const c_char) -> usize {
    if value.is_null() {
        set_last_error("strlen argument must not be null");
        return 0;
    }
    // SAFETY: the caller guarantees a readable NUL-terminated string.
    let length = unsafe { CStr::from_ptr(value) }.to_bytes().len();
    clear_last_error();
    length
}

fn compare_terminated_bytes(left: &[u8], right: &[u8], limit: usize) -> i32 {
    for index in 0..limit {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        if a != b {
            return i32::from(a) - i32::from(b);
        }
        if a == 0 {
            return 0;
        }
    }
    0
}

/// Compare two NUL-terminated byte strings lexicographically.
///
/// # Safety
///
/// Both pointers must point to readable NUL-terminated byte sequences.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_strcmp(left: *const c_char, right: *const c_char) -> i32 {
    if left.is_null() || right.is_null() {
        set_last_error("strcmp arguments must not be null");
        return 0;
    }
    // SAFETY: both arguments must be readable NUL-terminated strings.
    let left = unsafe { CStr::from_ptr(left) }.to_bytes();
    // SAFETY: both arguments must be readable NUL-terminated strings.
    let right = unsafe { CStr::from_ptr(right) }.to_bytes();
    clear_last_error();
    compare_terminated_bytes(left, right, usize::MAX)
}

/// Compare at most count bytes of two NUL-terminated byte strings.
///
/// # Safety
///
/// Both pointers must point to readable NUL-terminated byte sequences, even if count is
/// smaller than either string's length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_strncmp(
    left: *const c_char,
    right: *const c_char,
    count: usize,
) -> i32 {
    if count == 0 {
        return 0;
    }
    if left.is_null() || right.is_null() {
        set_last_error("strncmp arguments must not be null when count is nonzero");
        return 0;
    }
    // SAFETY: both arguments must be readable NUL-terminated strings.
    let left = unsafe { CStr::from_ptr(left) }.to_bytes();
    // SAFETY: both arguments must be readable NUL-terminated strings.
    let right = unsafe { CStr::from_ptr(right) }.to_bytes();
    clear_last_error();
    compare_terminated_bytes(left, right, count)
}

/// Find the first matching byte in a readable memory range.
///
/// # Safety
///
/// When count is nonzero, value must be readable for count bytes. A null pointer is permitted
/// only when count is zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_memchr(
    value: *const c_void,
    byte: i32,
    count: usize,
) -> *mut c_void {
    if count == 0 {
        return ptr::null_mut();
    }
    if value.is_null() {
        set_last_error("memchr buffer must not be null when count is nonzero");
        return ptr::null_mut();
    }
    // SAFETY: value points to count readable bytes.
    let bytes = unsafe { slice::from_raw_parts(value.cast::<u8>(), count) };
    let result = bytes
        .iter()
        .position(|candidate| *candidate == byte as u8)
        .map(|offset| {
            // SAFETY: offset is within the readable range and the pointer originated from value.
            unsafe { value.cast::<u8>().add(offset).cast::<c_void>().cast_mut() }
        })
        .unwrap_or(ptr::null_mut());
    clear_last_error();
    result
}

/// Binary-safe payload returned by native TCP/UDP byte-request futures.
///
/// The buffer is opaque to RamLang. Read its length and data pointer through the provided
/// runtime builtins, and release the handle exactly once with ramlang_byte_buffer_close.
pub struct RamlangByteBuffer {
    bytes: Vec<u8>,
}

/// Return the number of bytes in a binary response buffer.
///
/// # Safety
/// buffer must be null or a live handle returned by a RamLang byte-buffer future.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_byte_buffer_len(buffer: *const RamlangByteBuffer) -> i64 {
    if buffer.is_null() {
        return fail_i64("byte buffer must not be null");
    }
    // SAFETY: buffer validity is part of this function's caller contract.
    let length = unsafe { &*buffer }.bytes.len();
    match i64::try_from(length) {
        Ok(length) => {
            clear_last_error();
            length
        }
        Err(_) => fail_i64("byte buffer length does not fit i64"),
    }
}

/// Return a pointer to the first byte in a response buffer.
///
/// The pointer remains valid until ramlang_byte_buffer_close is called. For an empty buffer,
/// the returned pointer must not be dereferenced.
///
/// # Safety
/// buffer must be null or a live handle returned by a RamLang byte-buffer future.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_byte_buffer_data(buffer: *const RamlangByteBuffer) -> *mut c_void {
    if buffer.is_null() {
        set_last_error("byte buffer must not be null");
        return ptr::null_mut();
    }
    // SAFETY: buffer validity is part of this function's caller contract.
    let data = unsafe { &*buffer }
        .bytes
        .as_ptr()
        .cast_mut()
        .cast::<c_void>();
    clear_last_error();
    data
}

/// Release a binary response buffer. Passing null is a no-op.
///
/// # Safety
/// A non-null buffer must be a live handle returned by a RamLang byte-buffer future and must
/// not be used again after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_byte_buffer_close(buffer: *mut RamlangByteBuffer) {
    if !buffer.is_null() {
        // SAFETY: buffer validity is part of this function's caller contract.
        drop(unsafe { Box::from_raw(buffer) });
    }
    clear_last_error();
}

unsafe extern "C" fn cleanup_byte_buffer(pointer: *mut c_void) {
    // SAFETY: the future only registers this callback for RamlangByteBuffer results.
    unsafe { ramlang_byte_buffer_close(pointer.cast::<RamlangByteBuffer>()) };
}

/// Mutable state shared between a RamLang Future handle and its runtime task.
struct FutureResultSlot {
    result: Option<Result<i64, String>>,
    consumed: bool,
    closed: bool,
    pointer_result_transferred: bool,
    abort_handle: Option<tokio::task::AbortHandle>,
}

/// Result slot shared with a generated RamLang async-function wrapper.
///
/// Pointer-result futures produced by network built-ins own their result allocation until
/// await transfers that pointer to the caller. If the handle is dropped first, the runtime
/// releases that allocation. Each Future is one-shot: awaiting a consumed result is an error.
type FuturePointerCleanup = unsafe extern "C" fn(*mut c_void);

struct FutureState {
    data: Mutex<FutureResultSlot>,
    ready: Condvar,
    ready_notify: Notify,
    pointer_result_cleanup: Option<FuturePointerCleanup>,
}

impl FutureState {
    fn new(pointer_result_cleanup: Option<FuturePointerCleanup>) -> Self {
        Self {
            data: Mutex::new(FutureResultSlot {
                result: None,
                consumed: false,
                closed: false,
                pointer_result_transferred: false,
                abort_handle: None,
            }),
            ready: Condvar::new(),
            ready_notify: Notify::new(),
            pointer_result_cleanup,
        }
    }

    fn complete(&self, value: i64) {
        self.complete_result(Ok(value));
    }

    fn set_abort_handle(&self, handle: tokio::task::AbortHandle) {
        let abort_now = {
            let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
            if data.closed {
                true
            } else {
                data.abort_handle = Some(handle.clone());
                false
            }
        };
        if abort_now {
            handle.abort();
        }
    }

    fn complete_result(&self, value: Result<i64, String>) {
        let mut value_to_release = None;
        let mut published_result = false;
        {
            let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
            if data.result.is_some() {
                return;
            }
            if data.closed {
                if let Some(cleanup) = self.pointer_result_cleanup {
                    value_to_release = value
                        .ok()
                        .map(|encoded| (cleanup, encoded as usize as *mut c_void));
                }
            } else {
                data.result = Some(value);
                self.ready.notify_all();
                published_result = true;
            }
        }
        if published_result {
            self.ready_notify.notify_waiters();
        }
        if let Some((cleanup, pointer)) = value_to_release
            && !pointer.is_null()
        {
            // SAFETY: the future's configured cleanup matches the pointer payload type.
            unsafe { cleanup(pointer) };
        }
    }

    fn close_handle(&self) {
        let (abort_handle, value_to_release) = {
            let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
            data.closed = true;
            let result_to_release =
                if self.pointer_result_cleanup.is_some() && !data.pointer_result_transferred {
                    data.result.take().and_then(Result::ok).map(|encoded| {
                        (
                            self.pointer_result_cleanup.expect("cleanup checked above"),
                            encoded as usize as *mut c_void,
                        )
                    })
                } else {
                    None
                };
            (data.abort_handle.take(), result_to_release)
        };
        if let Some(handle) = abort_handle {
            handle.abort();
        }
        self.ready_notify.notify_waiters();
        if let Some((cleanup, pointer)) = value_to_release
            && !pointer.is_null()
        {
            // SAFETY: the future's configured cleanup matches the pointer payload type.
            unsafe { cleanup(pointer) };
        }
    }

    fn is_ready_or_closed(&self) -> bool {
        let data = self.data.lock().unwrap_or_else(|error| error.into_inner());
        data.result.is_some() || data.closed
    }

    fn is_closed(&self) -> bool {
        self.data
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closed
    }
}

type AsyncFunctionCallback = unsafe extern "C" fn(*mut c_void) -> i64;
type AsyncContextCleanupCallback = unsafe extern "C" fn(*mut c_void);
type AsyncYielder = Yielder<(), Arc<FutureState>>;
type AsyncCoroutine = Coroutine<(), Arc<FutureState>, i64>;

thread_local! {
    /// Non-null only while the coroutine executor is actively resuming a RamLang async stack.
    static CURRENT_YIELDER: Cell<*const AsyncYielder> = const { Cell::new(ptr::null()) };
}

struct AsyncContextCleanup {
    context_address: usize,
    cleanup: Option<AsyncContextCleanupCallback>,
}

impl Drop for AsyncContextCleanup {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup {
            // SAFETY: the caller supplied the cleanup function for this context, and this guard
            // owns the context's cleanup responsibility for exactly one completion/cancellation.
            unsafe { cleanup(self.context_address as *mut c_void) };
        }
    }
}

struct AsyncCoroutineJob {
    callback: AsyncFunctionCallback,
    context_address: usize,
    context_cleanup: AsyncContextCleanup,
    state: Arc<FutureState>,
}

/// Owns a coroutine and its argument frame. Drop must unwind the stack before freeing the
/// context, because generated LLVM frames can keep pointers into it across an await.
struct ActiveAsyncCoroutine {
    coroutine: Option<AsyncCoroutine>,
    context_cleanup: Option<AsyncContextCleanup>,
}

impl Drop for ActiveAsyncCoroutine {
    fn drop(&mut self) {
        drop(self.coroutine.take());
        drop(self.context_cleanup.take());
    }
}

static ASYNC_COROUTINE_EXECUTOR: OnceLock<Result<UnboundedSender<AsyncCoroutineJob>, String>> =
    OnceLock::new();

fn async_coroutine_sender() -> Result<&'static UnboundedSender<AsyncCoroutineJob>, String> {
    match ASYNC_COROUTINE_EXECUTOR.get_or_init(|| {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<AsyncCoroutineJob>();
        let (started_sender, started_receiver) =
            std::sync::mpsc::sync_channel::<Result<(), String>>(1);
        std::thread::Builder::new()
            .name("ramlang-async-executor".to_owned())
            .spawn(move || {
                let runtime = match Builder::new_current_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = started_sender.send(Err(format!(
                            "could not start RamLang coroutine runtime: {error}"
                        )));
                        return;
                    }
                };
                let local = LocalSet::new();
                if started_sender.send(Ok(())).is_err() {
                    return;
                }
                local.block_on(&runtime, async move {
                    while let Some(job) = receiver.recv().await {
                        let state = Arc::clone(&job.state);
                        let task = tokio::task::spawn_local(run_async_coroutine(job));
                        state.set_abort_handle(task.abort_handle());
                        drop(task);
                    }
                });
            })
            .map_err(|error| format!("could not start RamLang async executor thread: {error}"))?;
        started_receiver
            .recv()
            .map_err(|error| format!("RamLang async executor startup channel failed: {error}"))??;
        Ok(sender)
    }) {
        Ok(sender) => Ok(sender),
        Err(error) => Err(error.clone()),
    }
}

async fn wait_for_future_ready(state: Arc<FutureState>) {
    loop {
        let notified = state.ready_notify.notified();
        tokio::pin!(notified);
        let _ = notified.as_mut().enable();
        if state.is_ready_or_closed() {
            return;
        }
        notified.await;
    }
}

async fn run_async_coroutine(job: AsyncCoroutineJob) {
    let AsyncCoroutineJob {
        callback,
        context_address,
        context_cleanup,
        state,
    } = job;

    if state.is_closed() {
        drop(context_cleanup);
        return;
    }

    let yielder_address = Arc::new(AtomicUsize::new(0));
    let coroutine_yielder_address = Arc::clone(&yielder_address);
    let coroutine = Coroutine::new(move |yielder, ()| {
        coroutine_yielder_address.store(yielder as *const AsyncYielder as usize, Ordering::Release);
        let previous = CURRENT_YIELDER.with(|slot| {
            let previous = slot.get();
            slot.set(yielder as *const AsyncYielder);
            previous
        });
        // SAFETY: this callback/context pair is owned by the async job and remains valid until
        // the coroutine completes or its stack is unwound during cancellation.
        let result = unsafe { callback(context_address as *mut c_void) };
        CURRENT_YIELDER.with(|slot| slot.set(previous));
        result
    });

    let mut active = ActiveAsyncCoroutine {
        coroutine: Some(coroutine),
        context_cleanup: Some(context_cleanup),
    };
    let mut started = false;

    loop {
        if state.is_closed() {
            break;
        }
        let previous = CURRENT_YIELDER.with(|slot| {
            let previous = slot.get();
            if started {
                let yielder = yielder_address.load(Ordering::Acquire) as *const AsyncYielder;
                slot.set(yielder);
            }
            previous
        });
        let result = active
            .coroutine
            .as_mut()
            .expect("active async coroutine was already dropped")
            .resume(());
        CURRENT_YIELDER.with(|slot| slot.set(previous));
        started = true;

        match result {
            CoroutineResult::Yield(awaited_state) => wait_for_future_ready(awaited_state).await,
            CoroutineResult::Return(value) => {
                state.complete(value);
                break;
            }
        }
    }
}

/// Opaque future handle returned by native async-function calls in generated RamLang programs.
///
/// Async function wrappers execute on Tokio's blocking pool, so multiple functions can make
/// progress concurrently while callers retain ordinary typed Rust/LLVM call frames. Awaiting
/// this initial backend blocks the calling OS thread. A later compiler milestone will replace
/// this bridge with resumable state-machine lowering so await can yield without blocking.
pub struct RamlangFuture {
    state: Arc<FutureState>,
}

/// Schedule a generated RamLang async-function entry point in the runtime.
///
/// The callback must have the signature extern C fn(context: ptr) -> i64. Generated wrappers
/// own and release their context when they begin executing. This runtime never dereferences it.
/// Schedule an unsafe RamLang entry callback on the runtime's blocking pool.
///
/// # Safety
///
/// If callback is Some, context must satisfy the callback's safety contract and remain valid
/// until the callback begins. The callback must not unwind across the C ABI boundary.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_future_spawn_i64(
    callback: Option<unsafe extern "C" fn(*mut c_void) -> i64>,
    context: *mut c_void,
) -> *mut RamlangFuture {
    let Some(callback) = callback else {
        set_last_error("async callback must not be null");
        return ptr::null_mut();
    };
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };

    let state = Arc::new(FutureState::new(None));
    let task_state = Arc::clone(&state);
    let context_address = context as usize;
    // Detach this task rather than aborting it on Future drop. Generated wrappers release
    // their heap argument frame when invoked, so abort-before-start would leak that frame.
    runtime.spawn_blocking(move || {
        // Convert through usize so the task closure is Send. The generated callback wrapper
        // owns the opaque context and guarantees its lifetime until invocation.
        let value = unsafe { callback(context_address as *mut c_void) };
        task_state.complete(value);
    });
    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

/// Spawn a RamLang async-function callback on the cooperative coroutine executor.
///
/// Unlike ramlang_future_spawn_i64, this entry owns the context and invokes its cleanup
/// exactly once after completion or cancellation. The coroutine executor suspends native
/// await calls instead of blocking a runtime worker thread.
///
/// # Safety
/// callback must be a valid non-unwinding C ABI function. context must remain valid until
/// cleanup runs. cleanup, when present, must release context and must not unwind.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_future_spawn_owned_i64(
    callback: Option<AsyncFunctionCallback>,
    context: *mut c_void,
    cleanup: Option<AsyncContextCleanupCallback>,
) -> *mut RamlangFuture {
    let context_cleanup = AsyncContextCleanup {
        context_address: context as usize,
        cleanup,
    };
    let Some(callback) = callback else {
        drop(context_cleanup);
        set_last_error("async callback must not be null");
        return ptr::null_mut();
    };
    let sender = match async_coroutine_sender() {
        Ok(sender) => sender,
        Err(error) => {
            drop(context_cleanup);
            set_last_error(&error);
            return ptr::null_mut();
        }
    };

    let state = Arc::new(FutureState::new(None));
    let job = AsyncCoroutineJob {
        callback,
        context_address: context as usize,
        context_cleanup,
        state: Arc::clone(&state),
    };
    if let Err(error) = sender.send(job) {
        drop(error.0);
        set_last_error("RamLang coroutine executor has stopped");
        return ptr::null_mut();
    }

    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

/// Start a framed TCP text request as a native future.
///
/// The input strings are copied before returning. The network work runs on the Tokio runtime
/// and the returned future resolves to a Rust-runtime-owned, NUL-terminated UTF-8 response.
/// The caller owns that response allocation and must release it with ramlang_free.
/// Transport errors and non-UTF-8 responses make await return -1 and set ramlang_last_error.
/// Start a native TCP text request using valid C strings.
///
/// # Safety
///
/// address and payload must point to readable NUL-terminated UTF-8 C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_request_future(
    address: *const c_char,
    payload: *const c_char,
    timeout_ms: i64,
) -> *mut RamlangFuture {
    let address = match read_c_string(address, "TCP destination") {
        Ok(value) => value,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    let payload = match read_c_string(payload, "TCP payload") {
        Ok(value) => value,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    if timeout_ms <= 0 {
        set_last_error("TCP timeout must be greater than zero");
        return ptr::null_mut();
    }
    if payload.len() > MAX_FFI_TCP_FRAME_SIZE {
        set_last_error("TCP request payload exceeds 16 MiB");
        return ptr::null_mut();
    }
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };

    let state = Arc::new(FutureState::new(Some(ramlang_free)));
    let task_state = Arc::clone(&state);
    let operation_timeout = Duration::from_millis(timeout_ms as u64);
    let task = runtime.spawn(async move {
        let result: Result<i64, String> = async {
            let mut connection = TcpConnection::connect(&address, operation_timeout)
                .await
                .map_err(|e| e.to_string())?;
            connection
                .write_frame(payload.as_bytes())
                .await
                .map_err(|e| e.to_string())?;
            let response = connection
                .read_frame(MAX_FFI_TCP_FRAME_SIZE)
                .await
                .map_err(|e| e.to_string())?;
            let response = String::from_utf8(response)
                .map_err(|error| format!("TCP text response is not valid UTF-8: {error}"))?;
            let allocation_size = response
                .len()
                .checked_add(1)
                .ok_or_else(|| "TCP response size overflow".to_owned())?;
            let output = ramlang_alloc(
                u64::try_from(allocation_size)
                    .map_err(|_| "TCP response size does not fit this platform".to_owned())?,
            );
            if output.is_null() {
                return Err("unable to allocate TCP response string".to_owned());
            }
            // SAFETY: the allocation is response length plus one byte, as requested above.
            unsafe {
                ptr::copy_nonoverlapping(response.as_ptr(), output.cast::<u8>(), response.len());
                *output.cast::<u8>().add(response.len()) = 0;
            }
            Ok(output as usize as i64)
        }
        .await;
        task_state.complete_result(result);
    });
    state.set_abort_handle(task.abort_handle());
    drop(task);

    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

/// Start a native UDP text request that resolves to a Future<str> in generated RamLang.
///
/// The request binds an ephemeral socket, sends one UTF-8 datagram to the resolved address,
/// and accepts one reply only from that exact peer. The response must be UTF-8. The future
/// result is a Rust-runtime-owned NUL-terminated string that the caller must release using
/// ramlang_free. Network and decoding errors are surfaced through ramlang_last_error when
/// the future is awaited.
/// Start a native UDP text request using valid C strings.
///
/// # Safety
///
/// address and payload must point to readable NUL-terminated UTF-8 C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_request_future(
    address: *const c_char,
    payload: *const c_char,
    timeout_ms: i64,
) -> *mut RamlangFuture {
    let address = match read_c_string(address, "UDP destination") {
        Ok(value) => value,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    let payload = match read_c_string(payload, "UDP payload") {
        Ok(value) => value,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    if timeout_ms <= 0 {
        set_last_error("UDP timeout must be greater than zero");
        return ptr::null_mut();
    }
    if payload.len() > 65_507 {
        set_last_error("UDP request payload exceeds 65,507 bytes");
        return ptr::null_mut();
    }
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };

    let state = Arc::new(FutureState::new(Some(ramlang_free)));
    let task_state = Arc::clone(&state);
    let operation_timeout = Duration::from_millis(timeout_ms as u64);
    runtime.spawn(async move {
        let result: Result<i64, String> = async {
            let mut addresses =
                tokio::time::timeout(operation_timeout, tokio::net::lookup_host(address.as_str()))
                    .await
                    .map_err(|_| "UDP DNS lookup timed out".to_owned())?
                    .map_err(|error| error.to_string())?;
            let target = addresses
                .next()
                .ok_or_else(|| "UDP destination resolved to no addresses".to_owned())?;
            let bind_address = if target.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            };
            let endpoint = UdpEndpoint::bind(bind_address, operation_timeout)
                .await
                .map_err(|error| error.to_string())?;
            endpoint
                .send_to(payload.as_bytes(), &target.to_string())
                .await
                .map_err(|error| error.to_string())?;
            let (response, peer) = endpoint
                .recv_from(65_507)
                .await
                .map_err(|error| error.to_string())?;
            if peer != target {
                return Err("UDP response came from an unexpected peer".to_owned());
            }
            let response = String::from_utf8(response)
                .map_err(|error| format!("UDP text response is not valid UTF-8: {error}"))?;
            let allocation_size = response
                .len()
                .checked_add(1)
                .ok_or_else(|| "UDP response size overflow".to_owned())?;
            let output = ramlang_alloc(
                u64::try_from(allocation_size)
                    .map_err(|_| "UDP response size does not fit this platform".to_owned())?,
            );
            if output.is_null() {
                return Err("unable to allocate UDP response string".to_owned());
            }
            // SAFETY: the allocation is response length plus one byte, as requested above.
            unsafe {
                ptr::copy_nonoverlapping(response.as_ptr(), output.cast::<u8>(), response.len());
                *output.cast::<u8>().add(response.len()) = 0;
            }
            Ok(output as usize as i64)
        }
        .await;
        task_state.complete_result(result);
    });

    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

/// Start a binary-safe TCP frame transaction as a native Future<ptr>.
///
/// Unlike tcp_request_future, this API accepts arbitrary bytes (including NUL and invalid
/// UTF-8) and resolves to an opaque RamlangByteBuffer handle. The returned buffer must be
/// released with ramlang_byte_buffer_close. Input data is copied before this function returns.
///
/// # Safety
/// address must be a valid NUL-terminated UTF-8 string. If length is positive, data must point
/// to at least length readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_tcp_request_bytes_future(
    address: *const c_char,
    data: *const u8,
    length: i64,
    timeout_ms: i64,
) -> *mut RamlangFuture {
    let address = match read_c_string(address, "TCP destination") {
        Ok(value) => value,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    if timeout_ms <= 0 {
        set_last_error("TCP timeout must be greater than zero");
        return ptr::null_mut();
    }
    let length = match usize::try_from(length) {
        Ok(length) => length,
        Err(_) => {
            set_last_error("TCP byte request length must be non-negative");
            return ptr::null_mut();
        }
    };
    if length > MAX_FFI_TCP_FRAME_SIZE {
        set_last_error("TCP byte request payload exceeds 16 MiB");
        return ptr::null_mut();
    }
    if length > 0 && data.is_null() {
        set_last_error("TCP byte request data must not be null when length is nonzero");
        return ptr::null_mut();
    }
    let payload = if length == 0 {
        Vec::new()
    } else {
        // SAFETY: caller promises a readable region of the declared length.
        unsafe { slice::from_raw_parts(data, length) }.to_vec()
    };
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };

    let state = Arc::new(FutureState::new(Some(cleanup_byte_buffer)));
    let task_state = Arc::clone(&state);
    let operation_timeout = Duration::from_millis(timeout_ms as u64);
    let task = runtime.spawn(async move {
        let result: Result<i64, String> = async {
            let mut connection = TcpConnection::connect(&address, operation_timeout)
                .await
                .map_err(|error| error.to_string())?;
            connection
                .write_frame(&payload)
                .await
                .map_err(|error| error.to_string())?;
            let bytes = connection
                .read_frame(MAX_FFI_TCP_FRAME_SIZE)
                .await
                .map_err(|error| error.to_string())?;
            let buffer = Box::into_raw(Box::new(RamlangByteBuffer { bytes }));
            Ok(buffer as usize as i64)
        }
        .await;
        task_state.complete_result(result);
    });
    state.set_abort_handle(task.abort_handle());
    drop(task);
    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

/// Start a binary-safe UDP send/receive transaction as a native Future<ptr>.
///
/// The response is accepted only from the resolved destination address. Payloads and returned
/// datagrams are bounded to the maximum supported UDP payload, 65,507 bytes. The returned
/// handle must be released with ramlang_byte_buffer_close.
///
/// # Safety
/// destination must be a valid NUL-terminated UTF-8 string. If length is positive, data must
/// point to at least length readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_udp_request_bytes_future(
    destination: *const c_char,
    data: *const u8,
    length: i64,
    timeout_ms: i64,
) -> *mut RamlangFuture {
    let destination = match read_c_string(destination, "UDP destination") {
        Ok(value) => value,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    if timeout_ms <= 0 {
        set_last_error("UDP timeout must be greater than zero");
        return ptr::null_mut();
    }
    let length = match usize::try_from(length) {
        Ok(length) => length,
        Err(_) => {
            set_last_error("UDP byte request length must be non-negative");
            return ptr::null_mut();
        }
    };
    if length > 65_507 {
        set_last_error("UDP byte request payload exceeds 65,507 bytes");
        return ptr::null_mut();
    }
    if length > 0 && data.is_null() {
        set_last_error("UDP byte request data must not be null when length is nonzero");
        return ptr::null_mut();
    }
    let payload = if length == 0 {
        Vec::new()
    } else {
        // SAFETY: caller promises a readable region of the declared length.
        unsafe { slice::from_raw_parts(data, length) }.to_vec()
    };
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };

    let state = Arc::new(FutureState::new(Some(cleanup_byte_buffer)));
    let task_state = Arc::clone(&state);
    let operation_timeout = Duration::from_millis(timeout_ms as u64);
    let task = runtime.spawn(async move {
        let result: Result<i64, String> = async {
            let mut addresses = tokio::time::timeout(
                operation_timeout,
                tokio::net::lookup_host(destination.as_str()),
            )
            .await
            .map_err(|_| "UDP DNS lookup timed out".to_owned())?
            .map_err(|error| error.to_string())?;
            let target = addresses
                .next()
                .ok_or_else(|| "UDP destination resolved to no addresses".to_owned())?;
            let bind_address = if target.is_ipv4() {
                "0.0.0.0:0"
            } else {
                "[::]:0"
            };
            let endpoint = UdpEndpoint::bind(bind_address, operation_timeout)
                .await
                .map_err(|error| error.to_string())?;
            endpoint
                .send_to(&payload, &target.to_string())
                .await
                .map_err(|error| error.to_string())?;
            let (bytes, peer) = endpoint
                .recv_from(65_507)
                .await
                .map_err(|error| error.to_string())?;
            if peer != target {
                return Err("UDP response came from an unexpected peer".to_owned());
            }
            let buffer = Box::into_raw(Box::new(RamlangByteBuffer { bytes }));
            Ok(buffer as usize as i64)
        }
        .await;
        task_state.complete_result(result);
    });
    state.set_abort_handle(task.abort_handle());
    drop(task);
    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

/// Create a future that completes after the requested number of milliseconds.
///
/// The timer runs on the shared Tokio runtime and is cancelled if its Future handle is closed
/// before completion. Delay uses an unsigned type, so negative timeouts are rejected by the
/// RamLang type checker before reaching this function.
#[unsafe(no_mangle)]
pub extern "C" fn ramlang_sleep_future(delay_ms: u64) -> *mut RamlangFuture {
    let runtime = match async_request_runtime() {
        Ok(runtime) => runtime,
        Err(error) => {
            set_last_error(&error);
            return ptr::null_mut();
        }
    };
    let state = Arc::new(FutureState::new(None));
    let task_state = Arc::clone(&state);
    let task = runtime.spawn(async move {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        task_state.complete(0);
    });
    state.set_abort_handle(task.abort_handle());
    drop(task);
    clear_last_error();
    Box::into_raw(Box::new(RamlangFuture { state }))
}

struct OwnedFutureHandle(*mut RamlangFuture);

impl Drop for OwnedFutureHandle {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        // SAFETY: this guard consumes the owned Future handle exactly once.
        let future = unsafe { Box::from_raw(self.0) };
        future.state.close_handle();
        drop(future);
    }
}

/// Block until a future produces its i64 payload.
///
/// # Safety
/// A non-null handle must point to a live RamlangFuture and must not be closed concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_future_await_i64(future: *mut RamlangFuture) -> i64 {
    if future.is_null() {
        return fail_i64("future handle must not be null");
    }
    // Clone shared state so the native handle reference itself isn't held across suspension.
    // SAFETY: pointer validity is required by this function's caller contract.
    let state = unsafe { &*future }.state.clone();
    let mut data = state.data.lock().unwrap_or_else(|e| e.into_inner());

    loop {
        while data.result.is_none() {
            if data.closed {
                set_last_error("Future was closed before completion");
                return -1;
            }

            let yielder = CURRENT_YIELDER.with(Cell::get);
            if yielder.is_null() {
                data = state.ready.wait(data).unwrap_or_else(|e| e.into_inner());
            } else {
                drop(data);
                // SAFETY: CURRENT_YIELDER is set only while the coroutine executor is resuming
                // this exact native stack, and the executor keeps the coroutine pinned to its
                // dedicated thread until the yielder returns.
                unsafe { (&*yielder).suspend(Arc::clone(&state)) };
                data = state.data.lock().unwrap_or_else(|e| e.into_inner());
            }
        }

        if data.consumed {
            set_last_error("Future result has already been consumed");
            return -1;
        }
        data.consumed = true;
        match data.result.as_ref().expect("Future result checked above") {
            Ok(value) => {
                let value = *value;
                if state.pointer_result_cleanup.is_some() {
                    data.pointer_result_transferred = true;
                }
                clear_last_error();
                return value;
            }
            Err(error) => {
                set_last_error(error);
                return -1;
            }
        }
    }
}

/// Await a Future and take ownership of its handle. If this coroutine is cancelled while
/// suspended, the handle is closed as the coroutine stack unwinds.
///
/// # Safety
/// future must be null or a live handle owned by this await operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_future_await_owned_i64(future: *mut RamlangFuture) -> i64 {
    let _owned_future = OwnedFutureHandle(future);
    // SAFETY: forwarded caller contract; _owned_future closes the handle after return/unwind.
    unsafe { ramlang_future_await_i64(future) }
}
/// Query completion without waiting: 1 ready, 0 pending, -1 invalid handle.
///
/// # Safety
/// A non-null handle must point to a live RamlangFuture and must not be closed concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_future_is_ready(future: *mut RamlangFuture) -> i32 {
    if future.is_null() {
        return fail_i32("future handle must not be null");
    }
    // SAFETY: pointer validity is required by this function's contract.
    let future = unsafe { &*future };
    let ready = future
        .state
        .data
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .result
        .is_some();
    clear_last_error();
    i32::from(ready)
}

/// Release the caller's future handle. Passing null is a no-op.
///
/// Started work is not cancelled by closing the handle; the result state remains alive until
/// the runtime task completes.
/// Release the caller's future handle. Passing null is a no-op.
///
/// Native network and timer futures cancel pending work when their handle closes. A future
/// wrapping a user-declared async function detaches its worker instead, because that wrapper
/// owns the argument-frame cleanup and may already be executing.
///
/// # Safety
///
/// A non-null future must be a live handle returned by a RamLang future API. It must not be
/// used again or closed concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_future_close(future: *mut RamlangFuture) {
    if !future.is_null() {
        // SAFETY: pointer validity is required by this function's contract.
        let future = unsafe { Box::from_raw(future) };
        future.state.close_handle();
        drop(future);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{TcpListener, UdpSocket},
        thread,
    };

    #[test]
    fn rust_allocator_is_aligned_and_frees_allocations() {
        let allocation = ramlang_alloc(37);
        assert!(!allocation.is_null());
        assert_eq!(allocation as usize % 16, 0);

        let bytes = b"ramlang-owned-allocation";
        // SAFETY: the allocation contains 37 writable bytes and bytes fits the allocation.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), allocation.cast::<u8>(), bytes.len());
            assert_eq!(
                slice::from_raw_parts(allocation.cast::<u8>(), bytes.len()),
                bytes
            );
            ramlang_free(allocation);
        }
    }

    #[test]
    fn rust_calloc_zeroes_memory_and_realloc_preserves_contents() {
        let allocation = ramlang_calloc(16, 1);
        assert!(!allocation.is_null());
        // SAFETY: calloc allocated 16 bytes and initializes each byte to zero.
        unsafe {
            assert_eq!(slice::from_raw_parts(allocation.cast::<u8>(), 16), &[0; 16]);
            ptr::copy_nonoverlapping(b"ramlang".as_ptr(), allocation.cast::<u8>(), 7);
        }

        // SAFETY: allocation came from the RamLang allocator and is still live.
        let grown = unsafe { ramlang_realloc(allocation, 64) };
        assert!(!grown.is_null());
        // SAFETY: realloc returned a live 64-byte allocation, preserving the old prefix.
        unsafe {
            assert_eq!(slice::from_raw_parts(grown.cast::<u8>(), 7), b"ramlang");
            ramlang_free(grown);
        }
    }

    #[test]
    fn rust_memory_and_string_builtins_match_expected_semantics() {
        let source = CString::new("hello").unwrap();
        let longer = CString::new("hello world").unwrap();
        // SAFETY: CString storage is NUL-terminated and remains live for the calls.
        assert_eq!(unsafe { ramlang_strlen(source.as_ptr()) }, 5);
        assert_eq!(
            unsafe { ramlang_strcmp(source.as_ptr(), longer.as_ptr()) }.signum(),
            -1
        );
        assert_eq!(
            unsafe { ramlang_strncmp(source.as_ptr(), longer.as_ptr(), 5) },
            0
        );

        let source_bytes = b"network";
        let mut copied = [0_u8; 7];
        // SAFETY: the source and destination ranges are valid and non-overlapping.
        unsafe {
            ramlang_memcpy(
                copied.as_mut_ptr().cast::<c_void>(),
                source_bytes.as_ptr().cast::<c_void>(),
                source_bytes.len(),
            );
        }
        assert_eq!(&copied, source_bytes);
        // SAFETY: both pointers are valid for the copied range.
        assert_eq!(
            unsafe {
                ramlang_memcmp(
                    copied.as_ptr().cast::<c_void>(),
                    source_bytes.as_ptr().cast::<c_void>(),
                    copied.len(),
                )
            },
            0
        );
        // SAFETY: copied points to seven valid readable bytes.
        let found =
            unsafe { ramlang_memchr(copied.as_ptr().cast::<c_void>(), b'w' as i32, copied.len()) };
        assert_eq!(found.cast::<u8>(), copied.as_mut_ptr().wrapping_add(3));
    }

    unsafe extern "C" fn return_context_address(context: *mut c_void) -> i64 {
        context as usize as i64
    }

    unsafe extern "C" fn await_child_future(context: *mut c_void) -> i64 {
        // SAFETY: the context is a Box containing one live Future handle.
        let child = unsafe { *context.cast::<*mut RamlangFuture>() };
        // SAFETY: child remains live until the parent context cleanup runs after its callback.
        unsafe { ramlang_future_await_i64(child) }
    }

    unsafe extern "C" fn cleanup_child_future_context(context: *mut c_void) {
        // SAFETY: the executor owns this Box and invokes cleanup exactly once.
        let child_slot = unsafe { Box::from_raw(context.cast::<*mut RamlangFuture>()) };
        if !child_slot.is_null() {
            // SAFETY: cleanup is responsible for releasing the owned Future handle.
            unsafe { ramlang_future_close(*child_slot) };
        }
    }

    #[test]
    fn closing_pending_native_tcp_future_cancels_network_io() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (received_tx, received_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut header = [0_u8; 4];
            stream.read_exact(&mut header).unwrap();
            let payload_length = u32::from_be_bytes(header) as usize;
            let mut payload = vec![0_u8; payload_length];
            stream.read_exact(&mut payload).unwrap();
            received_tx.send(()).unwrap();

            let mut extra = [0_u8; 1];
            stream.read(&mut extra).unwrap()
        });

        let address = CString::new(address.to_string()).unwrap();
        let payload = CString::new("cancel pending TCP future").unwrap();
        // SAFETY: address and payload are valid NUL-terminated strings copied on entry.
        let future =
            unsafe { ramlang_tcp_request_future(address.as_ptr(), payload.as_ptr(), 5_000) };
        assert!(!future.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });
        received_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        // SAFETY: the future is live and not being awaited or closed concurrently.
        unsafe { ramlang_future_close(future) };
        assert_eq!(server.join().unwrap(), 0);
    }

    #[test]
    fn floating_point_output_uses_six_significant_digits() {
        assert_eq!(format_general_float(3.875), "3.875");
        assert_eq!(format_general_float(0.012500000186264515), "0.0125");
        assert_eq!(format_general_float(0.0001), "0.0001");
        assert_eq!(format_general_float(0.00001), "1e-05");
        assert_eq!(format_general_float(100_000.0), "100000");
        assert_eq!(format_general_float(1_000_000.0), "1e+06");
        assert_eq!(format_general_float(123_456.7), "123457");
        assert_eq!(format_general_float(f64::INFINITY), "inf");
        assert_eq!(format_general_float(f64::NEG_INFINITY), "-inf");
        assert_eq!(format_general_float(-0.0), "-0");
    }

    #[test]
    fn native_tcp_bytes_future_round_trips_embedded_nul_and_invalid_utf8() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let payload = vec![0_u8, 255, 128, 65, 10];
        let server_payload = payload.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut header = [0_u8; 4];
            stream.read_exact(&mut header).unwrap();
            let length = u32::from_be_bytes(header) as usize;
            let mut received = vec![0_u8; length];
            stream.read_exact(&mut received).unwrap();
            assert_eq!(received, server_payload);
            stream.write_all(&header).unwrap();
            stream.write_all(&received).unwrap();
        });

        let address = CString::new(address.to_string()).unwrap();
        // SAFETY: the address is NUL-terminated and payload's bytes are readable for its length.
        let future = unsafe {
            ramlang_tcp_request_bytes_future(
                address.as_ptr(),
                payload.as_ptr(),
                payload.len() as i64,
                2_000,
            )
        };
        assert!(!future.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });
        // SAFETY: the future is live and is not being consumed or closed concurrently.
        let encoded = unsafe { ramlang_future_await_i64(future) };
        assert_ne!(encoded, -1, "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });
        let buffer = encoded as u64 as usize as *mut RamlangByteBuffer;
        assert!(!buffer.is_null());
        // SAFETY: buffer is a live byte-buffer handle returned by the future.
        assert_eq!(
            unsafe { ramlang_byte_buffer_len(buffer) },
            payload.len() as i64
        );
        // SAFETY: the returned data pointer stays valid until byte_buffer_close.
        let data = unsafe { ramlang_byte_buffer_data(buffer) }.cast::<u8>();
        // SAFETY: the buffer length and data pointer describe the same live allocation.
        assert_eq!(
            unsafe { slice::from_raw_parts(data, payload.len()) },
            payload
        );

        // SAFETY: both handles are live and no concurrent use is in progress.
        unsafe {
            ramlang_byte_buffer_close(buffer);
            ramlang_future_close(future);
        }
        server.join().unwrap();
    }

    #[test]
    fn native_udp_bytes_future_round_trips_embedded_nul_and_invalid_utf8() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let address = server.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut bytes = [0_u8; 128];
            let (length, peer) = server.recv_from(&mut bytes).unwrap();
            assert_eq!(&bytes[..length], &[0, 255, 128, 65, 10]);
            server.send_to(&bytes[..length], peer).unwrap();
        });

        let address = CString::new(address.to_string()).unwrap();
        let payload = [0_u8, 255, 128, 65, 10];
        // SAFETY: the address is NUL-terminated and payload's bytes are readable for its length.
        let future = unsafe {
            ramlang_udp_request_bytes_future(
                address.as_ptr(),
                payload.as_ptr(),
                payload.len() as i64,
                2_000,
            )
        };
        assert!(!future.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });
        // SAFETY: the future is live and is not being consumed or closed concurrently.
        let encoded = unsafe { ramlang_future_await_i64(future) };
        assert_ne!(encoded, -1, "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });
        let buffer = encoded as u64 as usize as *mut RamlangByteBuffer;
        assert!(!buffer.is_null());
        // SAFETY: buffer is a live byte-buffer handle returned by the future.
        assert_eq!(
            unsafe { ramlang_byte_buffer_len(buffer) },
            payload.len() as i64
        );
        // SAFETY: the returned data pointer stays valid until byte_buffer_close.
        let data = unsafe { ramlang_byte_buffer_data(buffer) }.cast::<u8>();
        // SAFETY: the buffer length and data pointer describe the same live allocation.
        assert_eq!(
            unsafe { slice::from_raw_parts(data, payload.len()) },
            payload
        );

        // SAFETY: both handles are live and no concurrent use is in progress.
        unsafe {
            ramlang_byte_buffer_close(buffer);
            ramlang_future_close(future);
        }
        server.join().unwrap();
    }

    #[test]
    fn native_sleep_future_completes_after_timer() {
        let future = ramlang_sleep_future(1);
        assert!(!future.is_null());
        assert_eq!(unsafe { ramlang_future_await_i64(future) }, 0);
        assert_eq!(unsafe { ramlang_future_is_ready(future) }, 1);
        unsafe { ramlang_future_close(future) };
    }

    #[test]
    fn awaiting_pending_child_yields_executor_to_other_async_functions() {
        let child = ramlang_sleep_future(600);
        assert!(!child.is_null());
        let context = Box::into_raw(Box::new(child)).cast::<c_void>();
        // SAFETY: the callback and cleanup agree on the boxed child-handle context.
        let parent = unsafe {
            ramlang_future_spawn_owned_i64(
                Some(await_child_future),
                context,
                Some(cleanup_child_future_context),
            )
        };
        assert!(!parent.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });

        // This sibling completes immediately. A blocking await inside parent would occupy
        // the executor thread and prevent this callback from running until the timer finishes.
        // SAFETY: the callback only encodes the value as an integer and never dereferences it.
        let sibling = unsafe {
            ramlang_future_spawn_owned_i64(Some(return_context_address), 7_usize as *mut c_void, None)
        };
        assert!(!sibling.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });

        let deadline = Instant::now() + Duration::from_millis(300);
        while unsafe { ramlang_future_is_ready(sibling) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        let sibling_ready = unsafe { ramlang_future_is_ready(sibling) } == 1;
        let parent_still_pending = unsafe { ramlang_future_is_ready(parent) } == 0;
        if !sibling_ready || !parent_still_pending {
            // SAFETY: neither Future is being awaited concurrently by another thread.
            unsafe {
                ramlang_future_close(sibling);
                ramlang_future_close(parent);
            }
            assert!(
                sibling_ready && parent_still_pending,
                "cooperative executor failed to run the sibling while the parent awaited a timer"
            );
        }

        // SAFETY: these handles are live and no other caller is consuming either result.
        assert_eq!(unsafe { ramlang_future_await_i64(sibling) }, 7);
        assert_eq!(unsafe { ramlang_future_await_i64(parent) }, 0);
        // SAFETY: both completed handles are now released by their owner.
        unsafe {
            ramlang_future_close(sibling);
            ramlang_future_close(parent);
        }
    }

    #[test]
    fn native_future_runs_callback_and_await_returns_typed_payload_bits() {
        // No dereference is performed; this integer is used solely as opaque test data.
        let context = 42_usize as *mut c_void;
        let future = unsafe { ramlang_future_spawn_i64(Some(return_context_address), context) };
        assert!(!future.is_null());

        let value = unsafe { ramlang_future_await_i64(future) };
        assert_eq!(value, 42);
        assert_eq!(unsafe { ramlang_future_is_ready(future) }, 1);
        unsafe { ramlang_future_close(future) };
    }

    #[test]
    fn c_abi_tcp_frame_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut header = [0_u8; 4];
            stream.read_exact(&mut header).unwrap();
            let length = u32::from_be_bytes(header) as usize;
            let mut payload = vec![0_u8; length];
            stream.read_exact(&mut payload).unwrap();
            stream.write_all(&header).unwrap();
            stream.write_all(&payload).unwrap();
        });

        let address = CString::new(address.to_string()).unwrap();
        // SAFETY: address is a live NUL-terminated CString.
        let connection = unsafe { ramlang_tcp_connect(address.as_ptr(), 2_000) };
        assert!(!connection.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });

        let payload = b"ramlang";
        // SAFETY: the handle is live and payload points to payload.len() readable bytes.
        assert_eq!(
            unsafe { ramlang_tcp_write_frame(connection, payload.as_ptr(), payload.len() as u64) },
            0,
            "{}",
            unsafe { CStr::from_ptr(ramlang_last_error()).to_string_lossy() }
        );

        let mut output = [0_u8; 32];
        // SAFETY: the handle is live and output has 32 writable bytes.
        assert_eq!(
            unsafe { ramlang_tcp_read_frame(connection, output.as_mut_ptr(), output.len() as u64) },
            payload.len() as i64,
            "{}",
            unsafe { CStr::from_ptr(ramlang_last_error()).to_string_lossy() }
        );
        assert_eq!(&output[..payload.len()], payload);

        // SAFETY: connection has not been closed or otherwise freed.
        unsafe { ramlang_tcp_close(connection) };
        server.join().unwrap();
    }

    #[test]
    fn c_abi_async_tcp_requests_run_concurrently() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            // Accept both connections before replying. This verifies that starting the
            // first request does not block the caller waiting for its response.
            let mut connections = Vec::new();
            for _ in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                connections.push(stream);
            }

            let handlers = connections
                .into_iter()
                .map(|mut stream| {
                    thread::spawn(move || {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut header = [0_u8; 4];
                        stream.read_exact(&mut header).unwrap();
                        let length = u32::from_be_bytes(header) as usize;
                        let mut payload = vec![0_u8; length];
                        stream.read_exact(&mut payload).unwrap();
                        thread::sleep(Duration::from_millis(20));
                        stream.write_all(&header).unwrap();
                        stream.write_all(&payload).unwrap();
                    })
                })
                .collect::<Vec<_>>();
            for handler in handlers {
                handler.join().unwrap();
            }
        });

        let address = CString::new(server_address.to_string()).unwrap();
        let first_payload = b"first";
        let second_payload = b"second";
        // SAFETY: address and both payload slices remain valid for the calls; inputs are
        // copied synchronously before the API returns.
        let first = unsafe {
            ramlang_tcp_request_start(
                address.as_ptr(),
                first_payload.as_ptr(),
                first_payload.len() as u64,
                2_000,
            )
        };
        let second = unsafe {
            ramlang_tcp_request_start(
                address.as_ptr(),
                second_payload.as_ptr(),
                second_payload.len() as u64,
                2_000,
            )
        };
        assert!(!first.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });
        assert!(!second.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });

        // SAFETY: both handles are live and each wait timeout is expressed in milliseconds.
        assert_eq!(unsafe { ramlang_tcp_request_wait(first, 2_000) }, 1);
        assert_eq!(unsafe { ramlang_tcp_request_wait(second, 2_000) }, 1);

        let mut first_output = [0_u8; 64];
        let mut second_output = [0_u8; 64];
        // SAFETY: both handles are live and the output buffers have 64 writable bytes.
        let first_length =
            unsafe { ramlang_tcp_request_read_result(first, first_output.as_mut_ptr(), 64) };
        let second_length =
            unsafe { ramlang_tcp_request_read_result(second, second_output.as_mut_ptr(), 64) };
        assert_eq!(first_length, first_payload.len() as i64);
        assert_eq!(second_length, second_payload.len() as i64);
        assert_eq!(&first_output[..first_length as usize], first_payload);
        assert_eq!(&second_output[..second_length as usize], second_payload);

        // SAFETY: each handle is live and has not been closed.
        unsafe {
            ramlang_tcp_request_close(first);
            ramlang_tcp_request_close(second);
        }
        server.join().unwrap();
    }

    #[test]
    fn c_abi_task_scope_cancels_tcp_and_udp_requests_together() {
        let tcp_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp_address = tcp_listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let tcp_ready = ready_tx.clone();
        let tcp_server = thread::spawn(move || {
            let (mut stream, _) = tcp_listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut header = [0_u8; 4];
            stream.read_exact(&mut header).unwrap();
            let length = u32::from_be_bytes(header) as usize;
            let mut payload = vec![0_u8; length];
            stream.read_exact(&mut payload).unwrap();
            tcp_ready.send("tcp").unwrap();
            thread::sleep(Duration::from_millis(200));
        });

        let udp_server = UdpSocket::bind("127.0.0.1:0").unwrap();
        udp_server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let udp_address = udp_server.local_addr().unwrap();
        let udp_ready = ready_tx.clone();
        let udp_server = thread::spawn(move || {
            let mut payload = [0_u8; 64];
            let _ = udp_server.recv_from(&mut payload).unwrap();
            udp_ready.send("udp").unwrap();
            thread::sleep(Duration::from_millis(200));
        });
        drop(ready_tx);

        let tcp_address = CString::new(tcp_address.to_string()).unwrap();
        let tcp_payload = b"tcp scope";
        // SAFETY: pointers are valid and inputs are copied before the calls return.
        let tcp_request = unsafe {
            ramlang_tcp_request_start(
                tcp_address.as_ptr(),
                tcp_payload.as_ptr(),
                tcp_payload.len() as u64,
                5_000,
            )
        };

        let udp_address = CString::new(udp_address.to_string()).unwrap();
        let udp_payload = b"udp scope";
        // SAFETY: pointers are valid and inputs are copied before the calls return.
        let udp_request = unsafe {
            ramlang_udp_request_start(
                udp_address.as_ptr(),
                udp_payload.as_ptr(),
                udp_payload.len() as u64,
                64,
                5_000,
            )
        };
        assert!(!tcp_request.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });
        assert!(!udp_request.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });

        let scope = ramlang_task_scope_new();
        assert!(!scope.is_null());
        // SAFETY: these are live handles and none are closed concurrently.
        assert_eq!(
            unsafe { ramlang_task_scope_track_tcp(scope, tcp_request) },
            0
        );
        assert_eq!(
            unsafe { ramlang_task_scope_track_udp(scope, udp_request) },
            0
        );
        assert_eq!(unsafe { ramlang_task_scope_len(scope) }, 2);

        let mut seen = [
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            ready_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        ];
        seen.sort_unstable();
        assert_eq!(seen, ["tcp", "udp"]);

        // SAFETY: scope is live and not being closed concurrently.
        assert_eq!(unsafe { ramlang_task_scope_cancel(scope) }, 0);
        assert_eq!(unsafe { ramlang_tcp_request_wait(tcp_request, 500) }, 1);
        assert_eq!(unsafe { ramlang_udp_request_wait(udp_request, 500) }, 1);

        let mut output = [0_u8; 64];
        // SAFETY: each handle is live and output has 64 writable bytes.
        assert_eq!(
            unsafe { ramlang_tcp_request_read_result(tcp_request, output.as_mut_ptr(), 64) },
            -1
        );
        assert_eq!(
            unsafe { CStr::from_ptr(ramlang_last_error()) }.to_string_lossy(),
            "TCP request cancelled"
        );
        assert_eq!(
            unsafe { ramlang_udp_request_read_result(udp_request, output.as_mut_ptr(), 64) },
            -1
        );
        assert_eq!(
            unsafe { CStr::from_ptr(ramlang_last_error()) }.to_string_lossy(),
            "UDP request cancelled"
        );

        // SAFETY: these handles are live and all usage has completed.
        unsafe {
            ramlang_task_scope_close(scope);
            ramlang_tcp_request_close(tcp_request);
            ramlang_udp_request_close(udp_request);
        }
        tcp_server.join().unwrap();
        udp_server.join().unwrap();
    }

    #[test]
    fn c_abi_task_scope_cancels_tracked_tcp_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_address = listener.local_addr().unwrap();
        let (received_tx, received_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut header = [0_u8; 4];
            stream.read_exact(&mut header).unwrap();
            let length = u32::from_be_bytes(header) as usize;
            let mut payload = vec![0_u8; length];
            stream.read_exact(&mut payload).unwrap();
            received_tx.send(()).unwrap();
            thread::sleep(Duration::from_millis(200));
        });

        let address = CString::new(server_address.to_string()).unwrap();
        let payload = b"scope cancellation";
        // SAFETY: address and payload are valid and copied synchronously.
        let request = unsafe {
            ramlang_tcp_request_start(
                address.as_ptr(),
                payload.as_ptr(),
                payload.len() as u64,
                5_000,
            )
        };
        assert!(!request.is_null());

        let scope = ramlang_task_scope_new();
        assert!(!scope.is_null());
        // SAFETY: both request and scope are live handles and not used concurrently.
        assert_eq!(unsafe { ramlang_task_scope_track_tcp(scope, request) }, 0);
        assert_eq!(unsafe { ramlang_task_scope_track_tcp(scope, request) }, 0);
        assert_eq!(unsafe { ramlang_task_scope_len(scope) }, 1);
        received_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        // SAFETY: the scope is live and not being closed concurrently.
        assert_eq!(unsafe { ramlang_task_scope_cancel(scope) }, 0);
        assert_eq!(unsafe { ramlang_tcp_request_wait(request, 500) }, 1);

        let mut output = [0_u8; 64];
        // SAFETY: request is live and output has 64 writable bytes.
        assert_eq!(
            unsafe { ramlang_tcp_request_read_result(request, output.as_mut_ptr(), 64) },
            -1
        );
        assert_eq!(
            unsafe { CStr::from_ptr(ramlang_last_error()) }.to_string_lossy(),
            "TCP request cancelled"
        );

        // SAFETY: both handles are live and have not been closed.
        unsafe {
            ramlang_task_scope_close(scope);
            ramlang_tcp_request_close(request);
        }
        server.join().unwrap();
    }

    #[test]
    fn c_abi_async_tcp_request_cancel_unblocks_waiters() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server_address = listener.local_addr().unwrap();
        let (received_tx, received_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut header = [0_u8; 4];
            stream.read_exact(&mut header).unwrap();
            let length = u32::from_be_bytes(header) as usize;
            let mut payload = vec![0_u8; length];
            stream.read_exact(&mut payload).unwrap();
            received_tx.send(()).unwrap();
            // Keep the socket open with no response so the client's read remains pending.
            thread::sleep(Duration::from_millis(250));
        });

        let address = CString::new(server_address.to_string()).unwrap();
        let payload = b"cancel me";
        // SAFETY: address and payload are valid and copied synchronously.
        let request = unsafe {
            ramlang_tcp_request_start(
                address.as_ptr(),
                payload.as_ptr(),
                payload.len() as u64,
                5_000,
            )
        };
        assert!(!request.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });
        received_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        // SAFETY: request is live and has not been closed.
        assert_eq!(unsafe { ramlang_tcp_request_cancel(request) }, 0);
        assert_eq!(unsafe { ramlang_tcp_request_wait(request, 500) }, 1);

        let mut output = [0_u8; 64];
        // SAFETY: request is live and output has 64 writable bytes.
        assert_eq!(
            unsafe { ramlang_tcp_request_read_result(request, output.as_mut_ptr(), 64) },
            -1
        );
        assert_eq!(
            unsafe { CStr::from_ptr(ramlang_last_error()) }.to_string_lossy(),
            "TCP request cancelled"
        );

        // SAFETY: request is live and has not been closed.
        unsafe { ramlang_tcp_request_close(request) };
        server.join().unwrap();
    }

    #[test]
    fn c_abi_async_udp_request_round_trip() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let address = server.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut payload = [0_u8; 128];
            let (length, peer) = server.recv_from(&mut payload).unwrap();
            server.send_to(&payload[..length], peer).unwrap();
        });

        let destination = CString::new(address.to_string()).unwrap();
        let payload = b"async udp";
        // SAFETY: the destination and payload are valid and copied before the call returns.
        let request = unsafe {
            ramlang_udp_request_start(
                destination.as_ptr(),
                payload.as_ptr(),
                payload.len() as u64,
                128,
                2_000,
            )
        };
        assert!(!request.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });

        // SAFETY: the request is live and has not been closed.
        assert_eq!(unsafe { ramlang_udp_request_wait(request, 2_000) }, 1);
        let mut output = [0_u8; 128];
        // SAFETY: the request is live and output has 128 writable bytes.
        let length = unsafe { ramlang_udp_request_read_result(request, output.as_mut_ptr(), 128) };
        assert_eq!(length, payload.len() as i64);
        assert_eq!(&output[..length as usize], payload);

        // SAFETY: the request is live and has not been closed.
        unsafe { ramlang_udp_request_close(request) };
        server.join().unwrap();
    }

    #[test]
    fn c_abi_async_udp_request_cancel_unblocks_waiters() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let address = server.local_addr().unwrap();
        let (received_tx, received_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let mut payload = [0_u8; 64];
            let (_, _) = server.recv_from(&mut payload).unwrap();
            received_tx.send(()).unwrap();
            thread::sleep(Duration::from_millis(250));
        });

        let destination = CString::new(address.to_string()).unwrap();
        let payload = b"cancel udp";
        // SAFETY: destination and payload are valid and copied before return.
        let request = unsafe {
            ramlang_udp_request_start(
                destination.as_ptr(),
                payload.as_ptr(),
                payload.len() as u64,
                64,
                5_000,
            )
        };
        assert!(!request.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });
        received_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        // SAFETY: request is live and has not been closed.
        assert_eq!(unsafe { ramlang_udp_request_cancel(request) }, 0);
        assert_eq!(unsafe { ramlang_udp_request_wait(request, 500) }, 1);
        let mut output = [0_u8; 64];
        // SAFETY: request is live and output has 64 writable bytes.
        assert_eq!(
            unsafe { ramlang_udp_request_read_result(request, output.as_mut_ptr(), 64) },
            -1
        );
        assert_eq!(
            unsafe { CStr::from_ptr(ramlang_last_error()) }.to_string_lossy(),
            "UDP request cancelled"
        );
        // SAFETY: request is live and has not been closed.
        unsafe { ramlang_udp_request_close(request) };
        server.join().unwrap();
    }

    #[test]
    fn c_abi_udp_datagram_round_trip() {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let server_address = server.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut payload = [0_u8; 64];
            let (length, peer) = server.recv_from(&mut payload).unwrap();
            server.send_to(&payload[..length], peer).unwrap();
        });

        let local_address = CString::new("127.0.0.1:0").unwrap();
        // SAFETY: local_address is a live NUL-terminated CString.
        let endpoint = unsafe { ramlang_udp_bind(local_address.as_ptr(), 2_000) };
        assert!(!endpoint.is_null(), "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });

        let mut local_output = [0 as c_char; 64];
        // SAFETY: the endpoint is live and local_output has 64 writable bytes.
        let local_length =
            unsafe { ramlang_udp_local_addr(endpoint, local_output.as_mut_ptr(), 64) };
        assert!(local_length > 0, "{}", unsafe {
            CStr::from_ptr(ramlang_last_error()).to_string_lossy()
        });
        assert!(
            unsafe { CStr::from_ptr(local_output.as_ptr()) }
                .to_string_lossy()
                .starts_with("127.0.0.1:")
        );

        let destination = CString::new(server_address.to_string()).unwrap();
        let payload = b"ramlang udp";
        // SAFETY: endpoint and destination are live, and payload has payload.len() bytes.
        assert_eq!(
            unsafe {
                ramlang_udp_send_to(
                    endpoint,
                    payload.as_ptr(),
                    payload.len() as u64,
                    destination.as_ptr(),
                )
            },
            payload.len() as i64,
            "{}",
            unsafe { CStr::from_ptr(ramlang_last_error()).to_string_lossy() }
        );

        let mut output = [0_u8; 64];
        let mut peer_output = [0 as c_char; 64];
        // SAFETY: endpoint is live; output and peer_output have the declared capacities.
        assert_eq!(
            unsafe {
                ramlang_udp_recv_from(
                    endpoint,
                    output.as_mut_ptr(),
                    output.len() as u64,
                    peer_output.as_mut_ptr(),
                    peer_output.len() as u64,
                )
            },
            payload.len() as i64,
            "{}",
            unsafe { CStr::from_ptr(ramlang_last_error()).to_string_lossy() }
        );
        assert_eq!(&output[..payload.len()], payload);
        assert_eq!(
            unsafe { CStr::from_ptr(peer_output.as_ptr()) }.to_string_lossy(),
            server_address.to_string()
        );

        // SAFETY: endpoint has not been closed or otherwise freed.
        unsafe { ramlang_udp_close(endpoint) };
        server.join().unwrap();
    }

    #[test]
    fn c_abi_rejects_null_arguments_without_panicking() {
        // SAFETY: null is explicitly handled by the API.
        assert!(unsafe { ramlang_tcp_connect(ptr::null(), 1_000) }.is_null());
        assert_eq!(
            unsafe { ramlang_tcp_write_frame(ptr::null_mut(), ptr::null(), 0) },
            -1
        );
    }
}
