# Networking from RamLang

RamLang currently compiles ordinary synchronous functions to native code. Its compiler does not yet implement language-native async functions or await expressions. The Tokio-backed runtime exposes networking through a C ABI so current .ram programs can use the APIs through extern fn declarations.

The connection-oriented TCP functions and UDP functions are blocking at the RamLang call site. In addition, the TCP request-handle API starts a full connect/write/read transaction on a shared multithread Tokio runtime and returns immediately; a RamLang program can start multiple requests before waiting for their results. This provides non-blocking network concurrency through explicit handles, but it is not a substitute for native async/await or compiler state-machine lowering.

## Build the shared library

    cargo build --release -p ramlang-runtime

The shared library is written to target/release/libramlang_runtime.so on Linux. The C declarations are in runtime/include/ramlang_runtime.h.

## Available C ABI

- ramlang_tcp_connect(address, timeout_ms): connect to a TCP endpoint and return an opaque handle, or null on failure.
- ramlang_tcp_write_frame(handle, data, length): write one frame and return zero on success, or -1 on failure.
- ramlang_tcp_read_frame(handle, output, capacity): read one frame into a caller-owned buffer and return its byte length, or -1 on failure.
- ramlang_tcp_close(handle): release a handle.
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


## Roadmap to native async

The next compiler work must introduce async-function declarations and async function types, add an await expression with type checking, then lower suspension points into a state machine. The runtime ABI will need task scheduling, wake-up, cancellation and structured-concurrency semantics before RamLang can claim native asynchronous execution.
