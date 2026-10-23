//! C ABI bridge for RamLang's native compiler.
//!
//! This layer intentionally exposes blocking functions so current RamLang programs can
//! call the networking runtime through existing extern fn declarations. Native
//! async fn / await integration remains a separate compiler milestone.

use std::{
    ffi::{CStr, CString, c_char},
    io, ptr, slice,
    sync::Mutex,
    time::Duration,
};
use tokio::runtime::{Builder, Runtime};

use crate::{TcpConnection, UdpEndpoint};

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
