# Ramlang Rust-native redesign RFC

Status: proposed architecture and migration contract; not a claim that the current compiler already implements it.

## 1. Repository baseline (verified)

The current default branch is a Rust 2024 Cargo workspace with two packages: `ramlang` and `ramlang-runtime`. The compiler driver is `src/main.rs`; it contains the lexer and parser and builds textual LLVM IR. `src/llvm_backend.rs` parses and verifies that IR using Inkwell, then emits object, assembly, or bitcode through LLVM's in-process target machine. The runtime is Rust, uses Tokio and corosensei, and exposes an FFI surface. The current feature set includes scalar types, functions, aliases, enums, globals/statics, layout queries, atomics, inline assembly, and an initial async/networking API.

This is a useful foundation, but it does **not** meet the strict self-contained Rust-toolchain goal yet:

- The compiler's default feature is `llvm18`; Inkwell is configured for LLVM 18 and Cargo features support LLVM 18–23. Building requires matching native LLVM development libraries and `llvm-config`.
- CI installs Clang, LLVM development packages, and LLD. The documented native x86_64 Linux linker path invokes `ld.lld`; other targets may use `RAMLANG_LINKER` (with `RAMLANG_CLANG` as a legacy alias). LLD is a separate native executable, not a Rust crate.
- The backend emits LLVM IR as text before parsing it back through LLVM. This is a workable transition but duplicates representation and gives generated IR correctness a critical role.
- The runtime crate depends on Tokio and corosensei, both Rust crates. Tokio is a reasonable interim async runtime; it is not a reason to keep the current blocking await bridge.
- The runtime declares `rlib` and `cdylib`, and the project generates a C-compatible header for ABI interop. This does not mean the implementation is written in C, but a cdylib/header should be optional interoperability artifacts rather than a build prerequisite.
- The networking guide explicitly says the current `await` bridge blocks the calling OS thread. Async functions run on a blocking pool; true compiler-generated suspension/resumption is still future work.
- `rust-toolchain.toml` pins Rust 1.99.1; the workspace uses edition 2024. The checked-in CI runs Cargo tests and release builds and exercises networking examples.
- The repository README reports unit tests for code generation and semantic errors. A full test inventory and all platform configurations have not been independently executed as part of this RFC.

No Python, JavaScript, C, C++, Java, or Go source is required to implement the current compiler front end based on the files inspected. However, the native LLVM/LLD build requirements mean it is not currently a Cargo-only toolchain in the strict sense requested here. Do not infer that an absent `package.json` or `pyproject.toml` proves every transitive build script is free of external tools.

## 2. Vision and non-goals

Ramlang should be a native systems language with a small, auditable compiler and explicit control over resource costs. It should offer memory-safe defaults, low-level escape hatches with visible unsafe boundaries, and first-class networking/concurrency without pretending that a language design alone proves safety or performance.

Goals:

1. Rust-only implementation code and a reproducible Cargo build; no required LLVM installation, C compiler, linker driver, scripting runtime, or foreign package manager.
2. Static types, predictable layouts where explicitly requested, explicit integer overflow policy, and checked conversions.
3. Ownership and borrowing for safe references, deterministic destruction, and a narrowly scoped `unsafe` mechanism for raw pointers, inline assembly, and OS ABI operations.
4. Async functions that compile to resumable state machines, cancellation-aware I/O, structured task scopes, bounded buffers, and explicit backpressure.
5. A clear native ABI, documented target support, useful diagnostics, formatter, package manager/build commands, and tests that can run without network access.
6. Reproducible builds and dependency auditing; no unsupported benchmark or security claims.

Non-goals for the initial migration: immediate C/C++ ABI parity, a fully optimizing LLVM-equivalent backend, a moving garbage collector, unrestricted implicit data races, a stable ABI before semantics stabilize, and support for every target at once.

## 3. Semantic design

### Types and memory

Keep the existing fixed-width scalar types, functions, aliases, enums, and useful layout built-ins where they can be specified precisely. Introduce an explicit type-checking phase and a typed, backend-independent IR before replacing code generation.

- Values are owned by default. Assignment and function passing follow a documented move/copy rule; primitive scalars and explicitly `Copy` types copy, owned aggregates move unless cloned.
- Borrowed references are `&T` and `&mut T`; safe code cannot retain references beyond the owner or create aliased mutable access. Lifetimes should be inferred locally where possible and explicit only where inference cannot decide.
- Raw pointers are `*const T` and `*mut T`; dereference, pointer arithmetic, foreign calls, and inline assembly require an `unsafe` block. Raw pointers are not silently treated as references.
- Arrays and structs have defined layout rules. Default aggregate layout is compiler-controlled; `repr(C)` and `repr(packed)` are explicit ABI/layout commitments. Packed-field references must be rejected when they would be unaligned.
- Integer overflow policy is explicit: checked operations return a result/flag, wrapping operations are named, and debug/release behavior must not silently differ. Division by zero and invalid shifts are defined errors/traps, never accidental backend undefined behavior.
- Strings are UTF-8 slices with explicit length, not implicitly NUL-terminated C strings. C strings are an interop type with validation/conversion.

### Errors

Use `Result<T, E>` for recoverable failures, `Option<T>` for absence, and a documented panic/abort path for violated invariants. Network and filesystem operations return typed errors; no nullable pointer plus hidden thread-local last-error slot in the safe API. The FFI layer may translate errors into status codes and explicit output parameters.

### Async and concurrency

`async fn` produces a compiler-owned future state machine; `await` suspends the current task and does not block an OS thread. Start with a single-thread executor for a small semantic surface, then add a work-stealing or multi-thread executor only with documented Send/Sync-like rules. Tasks are structured: child tasks belong to a scope, cancellation is propagated, and shutdown semantics are explicit. Network APIs expose timeouts, cancellation, bounded frame sizes, and backpressure. No implicit detached tasks.

### Unsafe and OS interfaces

The safe standard library should wrap OS handles with RAII and typed ownership. Platform-specific syscalls and socket APIs may require Rust crates or small Rust `unsafe` modules; those modules must document invariants and isolate ABI declarations. Platform SDKs and system ABI differences are unavoidable, but they must not require project implementation code in another language. Inline assembly is target-specific and explicitly unsafe. Safe APIs must not expose arbitrary pointer handles as nominally safe resources.

### Compatibility

Declare the language pre-1.0 until ownership, async cancellation, ABI, and package semantics are specified. Preserve source syntax and behavior when it is well-defined; add a compatibility report for intentional changes. Introduce a language edition or explicit migration diagnostics before breaking changes. Do not promise stable binary compatibility until the ABI and layout rules are tested across targets.

## 4. Target architecture

```text
ramcomp (Rust CLI)
  source loader / module graph
  lexer -> parser -> AST
  name resolution -> type checking -> borrow/ownership checking
  typed HIR -> MIR (control-flow graph, explicit drops and error edges)
      |                         |
      |                         +--> interpreter (reference semantics / tests)
      +--> Cranelift backend (native code; Rust crates)
                  |
             object files
                  |
          Rust-native linker path / platform linker adapter

ramlang-runtime (Rust)
  allocator + strings/bytes + panic/error reporting
  executor + task scopes + cancellation
  TCP/UDP + timers + OS handle wrappers
  optional, explicitly requested foreign ABI shim
```

The first backend candidate is Cranelift via Rust crates: it avoids requiring LLVM headers, `llvm-config`, Clang, or an LLVM installation to build Ramlang. It trades away some LLVM target coverage, maturity, optimization breadth, and compatibility with existing LLVM-only features such as arbitrary LLVM IR intrinsics and inline-assembly constraints. Audit the selected Cranelift versions and build scripts before adopting them; pin versions and prove clean builds in CI. Keep a small interpreter for semantics and tests. Do not generate C or Rust source as an intermediate language.

For native executable linking, distinguish object emission from final executable linking. A Rust crate cannot magically eliminate OS ABI, startup, or linker requirements on every platform. First support a documented host target using a Rust implementation of the required object/link steps where feasible, or an explicitly bundled Rust-native linker crate; verify it actually supports the desired target and relocations. If a platform system linker is required, label that target as not meeting the fully self-contained profile rather than silently calling it Rust-native. Freestanding Linux syscall support remains target-specific and must be isolated.

### Dependency policy

- Allowed: audited Rust crates with source distributed through Cargo, no required non-Rust build/runtime toolchain, and pinned/reproducible versions.
- Disallowed for the required build path: LLVM/Clang toolchains, `llvm-config`, external C/C++ compilation, generated C/C++ glue, Node/npm, Python/pip, Java/JVM, Go, or a foreign package manager.
- OS-provided kernel interfaces, system libraries, and target ABI conventions may be unavoidable at runtime; document each per target. Optional C ABI output is allowed but must not require a C compiler to build Ramlang.
- CI must include a clean build with only the pinned Rust toolchain and Cargo cache, plus an offline test pass. Audit `build.rs` for all direct and transitive dependencies.

## 5. Migration milestones and acceptance criteria

### M0 — Baseline and language contract
- Record current syntax, built-ins, generated artifacts, and test coverage.
- Add executable conformance tests for existing examples and semantic errors.
- Acceptance: current baseline builds and tests on the documented LLVM setup; unsupported behavior is documented.

### M1 — Rust-native semantic core
- Extract lexer/parser/AST, diagnostics with source spans, name resolution, and type checking from the compiler driver into Rust modules/crates.
- Add a typed HIR/MIR and interpreter for the currently supported scalar/control-flow subset.
- Acceptance: golden tests compare interpreter output and diagnostics; no external compiler is invoked by the interpreter path.

### M2 — Native backend without LLVM
- Prototype Cranelift for the same typed IR; retain the LLVM backend behind a temporary migration feature only.
- Implement object emission and a host executable-linking strategy, with explicit target capabilities.
- Acceptance: clean Cargo build with LLVM and Clang absent; hello-world and arithmetic/control-flow tests run; output matches interpreter; no generated C/C++.

### M3 — Ownership, memory and ABI
- Specify ownership, borrowing, drop order, raw pointers, aggregate layout, checked arithmetic and FFI.
- Implement borrow checking incrementally and reject unsafe aliasing before expanding the standard library.
- Acceptance: compile-fail tests cover use-after-move, dangling borrow, conflicting mutable borrow, unaligned packed-field access, and invalid unsafe operations; layout tests validate documented ABI cases.

### M4 — Cooperative async and networking
- Lower async functions to explicit state machines in MIR; implement task wakeups and cancellation-safe cleanup.
- Replace nullable handles/last-error semantics in safe APIs with typed `Result` values; retain bounded framing and binary-safe byte APIs.
- Acceptance: a pending await does not block the executor thread; cancellation closes owned sockets; stress tests exercise concurrency, timeouts, partial reads/writes, frame limits, and backpressure.

### M5 — Developer workflow and release gates
- Add `ram fmt`, `ram check`, `ram test`, `ram build`, package manifests/lockfile, deterministic diagnostics, and editor/LSP protocol support.
- Acceptance: clean/offline CI, reproducible lockfile, formatter and lint gates, cross-target smoke tests, and documented release/support matrix.

## 6. Required verification

During the transition, run and report separately:

```sh
rustc --version
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo build --workspace --release
```

The existing baseline additionally needs its configured LLVM development libraries and LLD. These commands are acceptance criteria, not results asserted by this RFC. For the Rust-native milestone, repeat build and test in a clean environment where LLVM, Clang, `llvm-config`, and external language runtimes are unavailable. Inspect `cargo tree`, build scripts, and CI logs for transitive toolchain requirements.

## 7. Known risks

- Cranelift does not automatically replace every LLVM feature or target. Inline assembly, intrinsics, object formats, relocation models, and optimization behavior require explicit capability decisions.
- A compiler with a borrow checker is a major implementation effort. Until it exists and is tested, Ramlang must not claim Rust-like memory safety.
- Async cancellation and resource destruction interact; state-machine lowering must preserve drops on every exit path.
- System linking, libc, dynamic loaders, and OS APIs differ by target. “Rust-only implementation” is achievable; “no system tools or libraries on every target” is not a credible universal promise.
- Current raw pointers, FFI conventions, and runtime error slots cannot simply be rebranded as safe abstractions. Each needs a specified ownership and failure contract.


## Implementation update (2026-10-24)

The first code milestone now exists in `crates/ramlang-core`, a dependency-free Rust crate added to the Cargo workspace. It defines a small typed register IR, verifies register types, call signatures and branch targets, and includes a budgeted reference interpreter with checked i64 arithmetic, function calls, branches, and explicit runtime errors.

This is a semantic-core prototype, not yet connected to the existing `.ram` parser/compiler. The current `ramcomp` binary still uses the legacy LLVM backend; the project does not yet satisfy the no-LLVM build requirement. The crate's unit tests are included in workspace default members so the existing CI's `cargo test --all-targets` exercises the new core. The next implementation milestone is to add source spans and control-flow IR, then migrate a small subset of the existing parser/type checker into this backend-neutral pipeline.
