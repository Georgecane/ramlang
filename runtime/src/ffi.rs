//! C ABI bridge for RamLang's native compiler.
//!
//! This layer keeps compatibility APIs for direct handle-based use while native
//! async functions and text TCP/UDP requests use Rust-owned Future handles. The compiler's
//! current await bridge blocks the calling OS thread; resumable state-machine lowering remains
//! future work.

use std::{
    alloc::{Layout, alloc as alloc_bytes, dealloc as dealloc_bytes},
    ffi::{CStr, CString, c_char, c_void},
    io, ptr, slice,
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::runtime::{Builder, Runtime};

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
            }
        }
        if let Some((cleanup, pointer)) = value_to_release {
            if !pointer.is_null() {
                // SAFETY: the future's configured cleanup matches the pointer payload type.
                unsafe { cleanup(pointer) };
            }
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
        if let Some((cleanup, pointer)) = value_to_release {
            if !pointer.is_null() {
                // SAFETY: the future's configured cleanup matches its pointer payload type.
                unsafe { cleanup(pointer) };
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

/// Block until a future produces its i64 payload.
///
/// # Safety
/// A non-null handle must point to a live RamlangFuture and must not be closed concurrently.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ramlang_future_await_i64(future: *mut RamlangFuture) -> i64 {
    if future.is_null() {
        return fail_i64("future handle must not be null");
    }
    // SAFETY: pointer validity is required by this function's contract.
    let future = unsafe { &*future };
    let mut data = future.state.data.lock().unwrap_or_else(|e| e.into_inner());
    while data.result.is_none() {
        data = future
            .state
            .ready
            .wait(data)
            .unwrap_or_else(|e| e.into_inner());
    }
    if data.consumed {
        set_last_error("Future result has already been consumed");
        return -1;
    }
    data.consumed = true;
    match data.result.as_ref().expect("Future result checked above") {
        Ok(value) => {
            let value = *value;
            if future.state.pointer_result_cleanup.is_some() {
                data.pointer_result_transferred = true;
            }
            clear_last_error();
            value
        }
        Err(error) => {
            set_last_error(error);
            -1
        }
    }
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

    unsafe extern "C" fn return_context_address(context: *mut c_void) -> i64 {
        context as usize as i64
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
    fn native_sleep_future_completes_after_timer() {
        let future = ramlang_sleep_future(1);
        assert!(!future.is_null());
        assert_eq!(unsafe { ramlang_future_await_i64(future) }, 0);
        assert_eq!(unsafe { ramlang_future_is_ready(future) }, 1);
        unsafe { ramlang_future_close(future) };
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
