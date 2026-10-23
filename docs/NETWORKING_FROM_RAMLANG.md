# Networking from RamLang

RamLang currently compiles ordinary synchronous functions to native code. Its compiler does not yet implement language-native async functions or await expressions. To make networking available to current .ram programs, the runtime provides a C ABI bridge backed by Tokio.

The bridge is **blocking at the RamLang call site**: a call to connect, write, or read waits until that operation completes or reaches its configured timeout. Tokio handles the socket I/O internally, but these functions are not a substitute for native async/await. Native async syntax and compiler state-machine lowering remain future compiler work.

## Build the shared library

    cargo build --release -p ramlang-runtime

The shared library is written to target/release/libramlang_runtime.so on Linux. The C declarations are in runtime/include/ramlang_runtime.h.

## Available C ABI

- ramlang_tcp_connect(address, timeout_ms): connect to a TCP endpoint and return an opaque handle, or null on failure.
- ramlang_tcp_write_frame(handle, data, length): write one frame and return zero on success, or -1 on failure.
- ramlang_tcp_read_frame(handle, output, capacity): read one frame into a caller-owned buffer and return its byte length, or -1 on failure.
- ramlang_tcp_close(handle): release a handle.
- ramlang_last_error(): return the calling OS thread's latest diagnostic as a NUL-terminated string.
- ramlang_udp_bind(local_address, timeout_ms): bind a UDP endpoint and return an opaque handle.
- ramlang_udp_local_addr(handle, output, capacity): write the local address as a NUL-terminated string.
- ramlang_udp_send_to(handle, data, length, destination): send a datagram; payloads are capped at 65,507 bytes.
- ramlang_udp_recv_from(handle, output, capacity, peer_output, peer_capacity): receive one datagram and copy the sender address into the peer buffer.
- ramlang_udp_close(handle): release a UDP handle.

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


## UDP example

The UDP bridge is also blocking at the call site and applies the endpoint timeout to send/receive operations. The receive API requires a payload buffer no larger than 65,507 bytes and a peer-address buffer; 64 bytes is sufficient for an IPv4 or IPv6 textual address.

    cargo build --release -p ramlang-runtime
    cargo run --release -- examples/network/udp_client.ram \
      --link-search target/release --link-lib ramlang_runtime \
      -o target/udp-client
    LD_LIBRARY_PATH="$PWD/target/release:$LD_LIBRARY_PATH" target/udp-client

The CI test starts a local UDP echo server at 127.0.0.1:39872. The client sends ping and prints the length and first byte of the reply. The C declarations for both TCP and UDP are in runtime/include/ramlang_runtime.h.


## Roadmap to native async

The next compiler work must introduce async-function declarations and async function types, add an await expression with type checking, then lower suspension points into a state machine. The runtime ABI will need task scheduling, wake-up, cancellation and structured-concurrency semantics before RamLang can claim native asynchronous execution.
