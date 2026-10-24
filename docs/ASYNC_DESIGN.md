# RamLang async and networking architecture

## Current implementation

RamLang has an initial source-level async path and a Rust-owned runtime:

- `async fn name(...) -> T` declares an async function.
- `Future<T>` is a handle type; `await future` consumes the result once.
- `future_is_ready(future)` checks completion without waiting.
- `async fn main() -> i32` is supported through a generated native entry-point wrapper.
- `sleep_ms`, `tcp_request`, and `udp_request` return typed futures.
- Native server APIs provide TCP listen/accept/read/write and UDP bind/receive/send operations. Text APIs require UTF-8; binary-safe `tcp_read_bytes`/`tcp_write_bytes` and `udp_recv_bytes`/`udp_packet_bytes`/`udp_send_bytes` preserve arbitrary bytes. TCP uses four-byte big-endian length-prefixed frames, while UDP preserves datagram boundaries.
- Futures can return scalar values, `str`, or `ptr`; strings returned by the runtime are owned and must be freed exactly once. Binary-safe `tcp_request_bytes` and `udp_request_bytes` return an opaque `ByteBuffer` pointer with explicit length/data access and deterministic close semantics.

The runtime is implemented in Rust on Tokio. The LLVM backend is also driven in-process from Rust through Inkwell: generated IR is parsed and verified, then LLVM emits target objects, assembly, or bitcode. Native x86_64 GNU/Linux executables link with LLVM lld directly; an explicitly selected platform linker remains available for custom targets and sysroots. Native code generation never requires Clang to parse or compile RamLang IR.

The C-compatible functions under `runtime/src/ffi.rs` are an implementation ABI between generated LLVM and Rust. RamLang programs do not need to write C or declare those functions; the compiler lowers the built-in names directly to the runtime symbols. Explicit `extern fn` remains an opt-in interoperability feature for calling foreign libraries.

## Critical limitation: await still blocks

This is not yet a fully cooperative async execution model. Async function bodies run on Tokio's blocking pool. The current `await` operation blocks its calling operating-system thread until the future resolves. Network operations themselves run as Tokio tasks and can progress independently, but a large number of pending RamLang tasks can consume blocking-pool threads.

The next major compiler milestone is to lower async functions into resumable state machines. Each async function should expose a poll/resume entry point; `await` should save the program counter and live values, register a wakeup, return pending, and resume only when the awaited future is ready. Merely scheduling the current function body on another thread is not sufficient to call the model non-blocking.

## Design goals

- Make async a first-class language abstraction, not an API that requires users to manage opaque handles.
- Keep Tokio behind a stable Rust runtime adapter rather than treating Tokio as part of the language specification.
- Make networking bounded, timeout-aware, cancellation-aware, and explicit about ownership.
- Keep unsafe pointer operations separate from high-level text/network APIs.
- Do not promise memory safety or data-race freedom until task boundaries and values live across suspension points have enforceable ownership/lifetime rules.
- Retain deliberate `extern fn` interoperability without requiring a C compiler for normal RamLang source programs.

## Roadmap

1. **Cooperative async lowering.** Build a typed async intermediate representation with explicit suspension points and liveness analysis. Spill values live across `await` into a task frame, implement poll/resume states, and connect runtime wakeups to the scheduler. Preserve the current blocking backend temporarily as a compatibility/debug mode.
2. **Typed error propagation.** Introduce a first-class `Result<T, E>` and standard propagation/control-flow semantics. Replace the current `null`/thread-local-last-error convention for high-level APIs with typed errors while retaining last-error diagnostics at low-level ABI boundaries.
3. **Structured concurrency.** Add language-level `spawn`, `join`/join sets, cancellation scopes, and deterministic parent-child lifetime rules. A parent should not accidentally leak detached tasks.
4. **Safe network types.** Replace untyped `ptr` handles in safe code with opaque `TcpListener`, `TcpStream`, `UdpSocket`, and packet types. Restrict raw-pointer escapes to explicitly unsafe code.
5. **Standard library expansion.** Add async DNS resolution, stream and datagram utilities, timers, cancellation, buffered I/O, and protocol parsing. Add TLS only with certificate validation and explicit trust configuration.
6. **Expand direct linker support.** Native x86_64 GNU/Linux executables now link directly with LLVM lld rather than the Clang compiler driver. Add/test native link backends for more ELF and non-ELF targets, and keep system startup/runtime linkage clearly distinct from requiring C source or C APIs in RamLang code.
7. **Reliability and performance.** Add negative type-checking tests, deterministic executor tests, stress tests for thousands of tasks, timeout/cancellation race tests, fuzzing for parser/IR/network framing, benchmark baselines, and cross-target CI.
8. **Developer tooling.** Add language-server support, formatting, package/dependency management, diagnostics with source spans, module privacy, and semantic-versioned standard-library interfaces.

## Runtime and CI

The `runtime/` crate includes TCP/UDP networking, bounded framed TCP reads, UDP payload bounds, asynchronous request handles, cancellation scopes, and `TaskGroup<T>` for Rust-side structured task ownership. RamLang examples and CI cover typed futures, async main, concurrent async functions, local TCP/UDP requests, and native TCP/UDP echo servers.

## Quality gates

- Rust formatting check, runtime Clippy with warnings denied, and workspace unit tests.
- LLVM parsing and verification before every emitted target artifact.
- Native object, assembly, and bitcode emission tests.
- Executable integration tests for async functions, async main, concurrent futures, and local TCP/UDP client/server paths.
- Memory/ownership tests for allocator, pointer-valued futures, close/cancel races, and strings returned across task boundaries.
- No claim of fully non-blocking awaits until state-machine lowering and scheduler-wakeup tests pass.
