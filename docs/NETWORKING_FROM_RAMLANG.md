# Networking from RamLang

RamLang now has an initial language-level async fn / Future<T> / await path and native text networking built-ins. The compiler emits calls into a Rust-owned Tokio runtime, so ordinary .ram code can start TCP or UDP work and await its UTF-8 response without declaring an extern fn or writing C source.

This is the first native integration milestone, not the final async execution model. async fn invocations run on the runtime's blocking pool, the built-in network operations run as Tokio tasks, and the current await waits on the calling OS thread. The next compiler milestone is explicit suspension/state-machine lowering so awaiting one future yields rather than tying up an OS thread. Text built-ins require valid UTF-8 and return a runtime-owned NUL-terminated response string; release it with free(response as ptr). Binary TCP/UDP APIs remain available through the runtime ABI for protocols that are not text-based.

## Build the shared library

    cargo build --release -p ramlang-runtime

The shared library is written to target/release/libramlang_runtime.so on Linux. The C-compatible declarations are in runtime/include/ramlang_runtime.h; they are an implementation ABI, not a requirement to write RamLang programs in C.

## Native TCP and UDP requests in RamLang

tcp_request(address, payload, timeout_ms) and udp_request(address, payload, timeout_ms) are built-ins that return Future<str>. They accept UTF-8 text, use framed TCP or a single UDP datagram respectively, and resolve to a NUL-terminated UTF-8 reply:

    let tcp_response: str = await tcp_request("127.0.0.1:39874", "hello over RamLang", 3000);
    print(tcp_response);
    free(tcp_response as ptr);

    let udp_response: str = await udp_request("127.0.0.1:39875", "hello over UDP", 3000);
    print(udp_response);
    free(udp_response as ptr);

The caller owns each response and must free it exactly once. A network, timeout, allocation, or UTF-8 error makes await yield a null pointer for Future<str> and records a diagnostic in the runtime's last-error slot. The TCP protocol uses a four-byte big-endian length prefix. The UDP protocol sends and receives one datagram and has the standard 65,507-byte payload limit. The examples in examples/async/tcp_request.ram and examples/async/udp_request.ram are compiled and run against local echo servers in CI.


## Available C ABI

- ramlang_tcp_connect(address, timeout_ms): connect to a TCP endpoint and return an opaque handle, or null on failure.
- ramlang_tcp_write_frame(handle, data, length): write one frame and return zero on success, or -1 on failure.
- ramlang_tcp_read_frame(handle, output, capacity): read one frame into a caller-owned buffer and return its byte length, or -1 on failure.
- ramlang_tcp_close(handle): release a handle.
- ramlang_task_scope_new(): create a cancellation scope; track TCP/UDP request handles with ramlang_task_scope_track_tcp and ramlang_task_scope_track_udp.
- ramlang_task_scope_cancel(scope): cancel all unfinished requests tracked by that scope while keeping the scope alive.
- ramlang_task_scope_len(scope): report the number of unique tracked requests.
- ramlang_task_scope_close(scope): cancel unfinished tracked work and release the scope; request handles must still be closed separately.
- ramlang_tcp_request_start(address, data, length, timeout_ms): copy the inputs and asynchronously connect, write one frame, then read one response frame.
- ramlang_tcp_request_is_ready(handle): return 1 when complete, 0 while pending, or -1 on error.
- ramlang_tcp_request_wait(handle, wait_timeout_ms): return 1 when complete, 0 when the wait timeout expires, or -1 on error.
- ramlang_tcp_request_read_result(handle, output, capacity): copy a successful response into caller-owned memory, or return -1 and set the diagnostic on failure.
- ramlang_tcp_request_cancel(handle): cancel pending work.
- ramlang_tcp_request_close(handle): cancel unfinished work and release the handle.
- ramlang_last_error(): return the calling OS thread's latest diagnostic as a NUL-terminated string.
- ramlang_udp_bind(local_address, timeout_ms): bind a UDP endpoint and return an opaque handle.
- ramlang_udp_local_addr(handle, output, capacity): write the local address as a NUL-terminated string.
- ramlang_udp_send_to(handle, data, length, destination): send a datagram; payloads are capped at 65,507 bytes.
- ramlang_udp_recv_from(handle, output, capacity, peer_output, peer_capacity): receive one datagram and copy the sender address into the peer buffer.
- ramlang_udp_close(handle): release a UDP handle.
- ramlang_udp_request_start(destination, data, length, response_capacity, timeout_ms): start a UDP send/receive transaction on the shared background runtime.
- ramlang_udp_request_is_ready(handle), ramlang_udp_request_wait(handle, wait_timeout_ms): inspect completion without waiting indefinitely.
- ramlang_udp_request_read_result(handle, output, capacity): copy the returned datagram.
- ramlang_udp_request_cancel(handle), ramlang_udp_request_close(handle): cancel and clean up background work.

Frames use a four-byte unsigned length prefix in big-endian byte order, followed by the payload. The C ABI caps each TCP frame and read-buffer capacity at 16 MiB; reads also reject frames larger than the supplied buffer capacity. If a read fails, close that connection because a partial frame may already have been consumed. A timeout greater than zero is required when connecting; it also bounds each read/write operation on that connection.

Pass valid NUL-terminated UTF-8 endpoint addresses. Buffers passed to write/read must be valid for the declared length/capacity. Do not use or close a handle concurrently with another operation on that same handle. Always close successful connections. Network input should be treated as untrusted.

## Build a RamLang TCP client

The included example connects to a local echo server, sends the frame ping, reads the reply, then prints the received frame length and first byte:

    cargo build --release -p ramlang-runtime
    cargo run --release -- examples/network/tcp_client.ram \
      --link-search target/release --link-lib ramlang_runtime \
      -o target/tcp-client
    LD_LIBRARY_PATH="$PWD/target/release${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
      target/tcp-client

A server must be listening at 127.0.0.1:39871 and speak the same length-prefixed framing protocol. The example is exercised in CI against a local Tokio echo server. Use a local test endpoint first; do not connect programs to untrusted public hosts until the error handling and protocol behavior are appropriate for your application.




## Cancellation scopes

A request scope lets a .ram caller group its background TCP and UDP request handles and cancel the unfinished work together on an error path. Create a scope, start a request, track its handle, and keep the request handle alive until the result has been consumed or the request has been closed. Closing a scope cancels all pending tracked requests and wakes any threads waiting for their results; it does not free the request handles themselves. Always close the individual request handles after closing the scope. Repeatedly tracking the same handle is safe and does not add duplicate entries.


## Non-blocking TCP requests from RamLang

Each request handle represents one connection, one framed write and one framed response read. The request start call copies the address and payload before returning and queues the operation on a shared multithread Tokio runtime. Start multiple handles before waiting on any of them to let their network operations overlap. Each connect, write and read has the supplied per-operation timeout; request wait has its own independent timeout. A response remains stored until the handle is closed, so a too-small output buffer can be corrected and read again. Closing the handle cancels unfinished work.

The two-request example sends first and second on two independent TCP connections, then waits for both responses:

    cargo build --release -p ramlang-runtime
    cargo run --release -- examples/network/tcp_concurrent.ram \
      --link-search target/release --link-lib ramlang_runtime \
      -o target/tcp-concurrent
    LD_LIBRARY_PATH="$PWD/target/release:$LD_LIBRARY_PATH" target/tcp-concurrent

The local test server listens on 127.0.0.1:39873 and accepts two connections. The expected program output is 5, 102, 6, 115 on separate lines. The C ABI is non-blocking for request submission, but RamLang still lacks native async/await syntax; programs currently manage request handles explicitly.


## UDP example

The UDP bridge is also blocking at the call site and applies the endpoint timeout to send/receive operations. The receive API requires a payload buffer no larger than 65,507 bytes and a peer-address buffer; 64 bytes is sufficient for an IPv4 or IPv6 textual address.

    cargo build --release -p ramlang-runtime
    cargo run --release -- examples/network/udp_client.ram \
      --link-search target/release --link-lib ramlang_runtime \
      -o target/udp-client
    LD_LIBRARY_PATH="$PWD/target/release:$LD_LIBRARY_PATH" target/udp-client

The CI test starts a local UDP echo server at 127.0.0.1:39872. The client sends ping and prints the length and first byte of the reply. The C declarations for both TCP and UDP are in runtime/include/ramlang_runtime.h.



## Non-blocking UDP requests

The UDP request API starts a send and waits for one response on the shared Tokio runtime, returning a handle immediately. It resolves the destination, sends using a temporary ephemeral socket, and accepts a reply only from the selected destination socket address. Request payloads and response buffers are limited to 65,507 bytes. The network timeout is applied separately to DNS lookup, socket binding, sending, and receiving; request wait has its own timeout. Closing the handle cancels work that has not completed.

    cargo build --release -p ramlang-runtime
    cargo run --release -- examples/network/udp_request.ram \
      --link-search target/release --link-lib ramlang_runtime \
      -o target/udp-request
    LD_LIBRARY_PATH="$PWD/target/release:$LD_LIBRARY_PATH" target/udp-request

The example uses the local echo service at 127.0.0.1:39872 and prints the response length and first byte. As with the TCP request API, this is handle-based background concurrency, not language-native async/await syntax.


## Native timer Future

The built-in `sleep_ms(delay)` returns `Future<void>`. Its argument must be unsigned milliseconds. Timer tasks run on the Tokio runtime and are cancelled when their Future handle is closed.

    async fn finish_after_delay() -> i32 {
        await sleep_ms(25);
        return 42;
    }

    fn main() -> i32 {
        print(await finish_after_delay());
        return 0;
    }

In this initial implementation, `await` blocks the calling OS thread while the timer itself remains asynchronous on Tokio. Resumable compiler state-machine lowering is still the next milestone.

## Next async compiler milestones

- Lower await to suspend/resume state machines instead of blocking the caller's OS thread.
- Propagate cancellation from parent tasks into in-flight network operations and define drop semantics for unused futures.
- Generalize Future<T> beyond the currently supported scalar and text-pointer results, including binary buffers with explicit lengths.
- Add async DNS, server/listener APIs, and typed errors as standard-library surface area rather than requiring raw handle management.
- Add compile-fail tests for invalid await contexts, lifetime tests for returned buffers, and stress tests for cancellation, concurrency, malformed responses, and timeouts.
