# RamLang async and networking roadmap

## Status

This change introduces `runtime/`, a standalone Rust async networking foundation. It does **not** claim that `async fn` or `.await` can be written in `.ram` files yet. The current compiler is a monolithic lexer/parser/LLVM backend; native async syntax requires coordinated language, type-checker, lowering, and runtime work.

## Design goals

- Preserve RamLang's systems-language character and C ABI interoperability.
- Make async the language-level abstraction, with structured concurrency rather than detached tasks by default.
- Keep the runtime replaceable: Tokio is the initial host runtime, not part of the language specification.
- Make network I/O non-blocking, cancellation-aware, bounded, and explicit about errors.
- Keep unsafe raw-pointer operations separate from safe high-level APIs.
- Avoid promising memory safety or data-race freedom until ownership, borrowing, and task-boundary constraints are implemented and checked.

## Proposed source syntax (design sketch, not implemented)

```ram
async fn fetch_status(host: str, port: u16) -> Result<str, NetError> {
    let connection = await Tcp.connect(host, port)?;
    await connection.write("STATUS\\n")?;
    let response = await connection.read_line(4096)?;
    return Ok(response);
}

async fn main() -> i32 {
    let first = spawn(fetch_status("127.0.0.1", 9000));
    let second = spawn(fetch_status("127.0.0.1", 9001));
    let results = await join(first, second);
    print(results);
    return 0;
}
```

This syntax is illustrative and is not accepted by the compiler yet.

## Compiler implementation sequence

1. Specify async function types, await precedence, error propagation, cancellation, task ownership, and async main.
2. Add `Future<T>`, `Result<T, E>`, async signatures, and await type checking.
3. Lower async functions into explicit state machines with poll/resume semantics. Initially reject locals borrowed across suspension points unless their lifetimes can be proven safe.
4. Define a stable internal task, wake, cancellation, timer, and I/O ABI. Keep Tokio behind a runtime adapter.
5. Add TCP, UDP, DNS, bounded framing, deadlines, and cancellation. Add TLS only with certificate validation and trust configuration.
6. Use structured concurrency: parent tasks own child tasks; cancellation and failures propagate predictably.
7. Establish ownership/borrowing or an equivalent enforceable lifetime model before describing high-level networking as memory-safe.
8. Add parser/type-checker negative tests, deterministic executor tests, localhost integration tests, fuzzing, and benchmarks.

## Runtime crate

`runtime/` provides async TCP connect and listener/accept helpers, length-prefixed frames with a caller-supplied maximum, bounded UDP datagrams, per-operation timeouts, and localhost tests for TCP/UDP round trips and oversized payload rejection. This is currently a Rust API, not yet callable from RamLang without an explicit C ABI bridge or compiler-generated runtime integration.

## Quality gates

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- Existing compiler tests and examples continue to pass.
- CI checks the supported Rust toolchain and Clang/LLVM versions.
- Tests cover cancellation, partial reads/writes, timeouts, malformed lengths, and concurrent clients.
