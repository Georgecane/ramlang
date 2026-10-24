# Rust-owned LLVM backend

## Current architecture

RamLang's lexer, parser, semantic checks, and IR lowering are implemented in Rust. The front-end currently constructs textual LLVM IR, so the migration preserves that proven lowering while moving LLVM's parsing, verification, target selection, and machine-code emission into the Rust process through Inkwell.

The backend lives in `src/llvm_backend.rs`:

1. Parse the generated IR into an LLVM module in an in-process LLVM context.
2. Run the LLVM verifier before emitting an artifact.
3. Configure an LLVM target machine with the requested target triple, CPU, target features, relocation mode, and optimization level.
4. Emit native object files and assembly directly through LLVM. Emit bitcode directly from the verified module.
5. On native x86_64 GNU/Linux, link the object directly with LLVM lld and the host startup/runtime files. Other targets and custom sysroots can explicitly select a compatible platform linker driver. Neither linker path parses RamLang's generated IR or performs LLVM code generation.

The build uses Inkwell 0.11.x and supports LLVM 18.1.x through 23.1.x behind explicit Cargo features. CI pins LLVM 23.1.x; developers with another supported release can build with `--no-default-features --features llvm22` (substitute their installed major). This keeps the Rust binding matched to the installed LLVM library instead of accepting an arbitrary Clang version for every compilation.

## What has and has not been removed

The compiler backend no longer invokes Clang to parse IR and emit machine code. Native x86_64 GNU/Linux executable builds invoke `ld.lld` directly, with system startup objects and system libraries supplied explicitly. This avoids the Clang compiler driver for normal builds on that target while still using the operating system's startup/runtime and libc. Other targets or custom sysroots can opt into a platform linker driver through `RAMLANG_LINKER`; the compiler does not claim that its entire dependency tree or the OS ABI is free of C.

Default generated modules now route printing, allocation, byte-memory operations, string helpers, and the first async networking built-ins through project-owned Rust runtime symbols. The compiler no longer emits default references to libc's printf, malloc, memcpy, or related helpers. The platform linker and operating-system startup/runtime are still used to create an executable, and explicit extern fn declarations remain as optional foreign-function interoperability. This removes the requirement to write or compile C source for ordinary RamLang programs; it does not claim to remove every C ABI from the toolchain.

## Experimental freestanding path

Use `--freestanding` for the restricted compute-only subset:

```sh
cargo run --release -- examples/freestanding/compute.ram --freestanding -o target/compute
target/compute
readelf -l target/compute
readelf -d target/compute
nm -u target/compute
```

For native x86_64 GNU/Linux, this mode appends an ELF `_start` that calls the RamLang `main() -> i32` and exits with the Linux `exit` syscall, then invokes LLVM lld without CRT objects, a dynamic loader, libc, or the RamLang shared runtime. CI verifies the executable starts, contains no ELF interpreter or `DT_NEEDED` entries, and has no undefined symbols.

This mode is not enabled by default and rejects `extern fn`, `extern static`, async functions, runtime futures, and calls to runtime built-ins. It is the first freestanding slice, not a replacement for the hosted Rust/Tokio runtime. Other architectures and custom sysroots are not supported by this mode yet.

## Migration roadmap

- Replace string-based IR construction incrementally with typed Inkwell builders and typed LLVM values, starting with function signatures, basic blocks, and primitive expressions.
- Replace the remaining string-based LLVM IR construction incrementally with typed Inkwell builders and typed LLVM values.
- Complete the Rust-owned standard runtime for panic, bounds and allocation errors, and eliminate legacy libc assumptions from the generated module.
- Turn the initial Future<T>/async fn/await path into resumable state-machine lowering so await suspends without blocking an OS thread.
- Expand native networking from the initial UTF-8 TCP/UDP request helpers to listeners, binary-safe buffers with explicit lengths, cancellation propagation, timers, and typed errors.
- Preserve explicit unsafe/extern declarations for deliberate foreign-library interoperability while keeping ordinary code independent of C source.
- Add target matrix tests, LLVM verifier tests, negative type-checking tests, async suspension/cancellation tests, fuzzing, and cross-platform runtime tests before removing compatibility paths.

The existing textual IR unit tests remain useful as lowering tests during the transition. Native-object and executable integration tests additionally exercise the LLVM library and target-machine path.
