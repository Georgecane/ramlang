//! C ABI bridge for RamLang's native compiler.
//!
//! This layer intentionally exposes blocking functions so current RamLang programs can
//! call the networking runtime through existing extern fn declarations. Native
//! async fn / await integration remains a separate compiler milestone.

use std::{
    ffi::{CStr, CString, c_char},
    io, ptr, slice,
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::runtime::{Builder, Runtime};

use crate::{TcpConnection, UdpEndpoint};

const MAX_FFI_TCP_FRAME_SIZE: usize = 16 * 1024 * 1024;

static ASYNC_REQUEST_RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();

/// State shared by the caller's request handle and its background Tokio task.
struct AsyncTcpRequestState {
    result: Mutex<Option<Result<Vec<u8>, String>>>,
    ready: Condvar,
    abort_handle: Mutex<Option<tokio::task::AbortHandle>>,
}

impl AsyncTcpRequestState {
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

    fn cancel(&self) {
        self.complete(Err("TCP request cancelled".to_owned()));
        let handle = self
            .abort_handle
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(handle) = handle.as_ref() {
            handle.abort();
        }
    }
}

/// Opaque handle for a background TCP request.
///
/// Requests run on a shared multithread Tokio runtime. Dropping the handle through
/// ramlang_tcp_request_close cancels an unfinished request.
pub struct RamlangTcpRequest {
    state: Arc<AsyncTcpRequestState>,
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

    let state = Arc::new(AsyncTcpRequestState::new());
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
    unsafe { &*request }.state.cancel();
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
        request.state.cancel();
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
    state: Arc<AsyncTcpRequestState>,
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

    let state = Arc::new(AsyncTcpRequestState::new());
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
                io::Error::new(io::ErrorKind::NotFound, "UDP destination resolved to no addresses")
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
    unsafe { &*request }.state.cancel();
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
        request.state.cancel();
        drop(request);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{TcpListener, UdpSocket},
        thread,
    };

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
        let length =
            unsafe { ramlang_udp_request_read_result(request, output.as_mut_ptr(), 128) };
        assert_eq!(length, payload.len() as i64);
        assert_eq!(&output[..length as usize], payload);

        // SAFETY: the request is live and has not been closed.
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
