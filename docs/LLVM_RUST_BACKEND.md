# Rust-owned LLVM backend

## Current architecture

RamLang's lexer, parser, semantic checks, and IR lowering are implemented in Rust. The front-end currently constructs textual LLVM IR, so the migration preserves that proven lowering while moving LLVM's parsing, verification, target selection, and machine-code emission into the Rust process through Inkwell.

The backend lives in `src/llvm_backend.rs`:

1. Parse the generated IR into an LLVM module in an in-process LLVM context.
2. Run the LLVM verifier before emitting an artifact.
3. Configure an LLVM target machine with the requested target triple, CPU, target features, relocation mode, and optimization level.
4. Emit native object files and assembly directly through LLVM. Emit bitcode directly from the verified module.
5. For an executable, send the resulting object file to a platform linker driver. The driver no longer parses RamLang's generated IR or performs LLVM code generation.

The build uses Inkwell 0.11.x and supports LLVM 18.1.x through 23.1.x behind explicit Cargo features. CI pins LLVM 23.1.x; developers with another supported release can build with `--no-default-features --features llvm22` (substitute their installed major). This keeps the Rust binding matched to the installed LLVM library instead of accepting an arbitrary Clang version for every compilation.

## What has and has not been removed

The compiler backend no longer invokes Clang to parse IR and emit machine code. The final executable still uses a platform linker driver so system startup objects, libc, user-selected native libraries, and target sysroots can be linked correctly.

This is not yet a claim that all C interoperability is gone. Existing programs can explicitly declare `extern fn`, the runtime still exports a C-compatible bridge for the current compiler, and existing built-ins currently lower to familiar native runtime symbols such as `printf` and `malloc`. Keeping the stable platform ABI is distinct from requiring RamLang applications to be written in C.

## Migration roadmap

- Replace string-based IR construction incrementally with typed Inkwell builders and typed LLVM values, starting with function signatures, basic blocks, and primitive expressions.
- Define RamLang's own runtime ABI in Rust and route built-in printing, allocation, panic, and networking through that runtime instead of requiring C source code or libc function declarations in generated IR.
- Add first-class `Future<T>`, `async fn`, and `await` to the parser and type checker.
- Lower async functions into explicit resumable state machines with cancellation and structured task ownership. Do not lower `await` to an ordinary blocking function call and call that asynchronous.
- Make TCP/UDP and timers standard-library operations over the native future model. Preserve explicit unsafe/extern declarations for deliberate foreign-library interoperability.
- Add target matrix tests, LLVM verifier tests, negative type-checking tests, async suspension/cancellation tests, fuzzing, and cross-platform runtime tests before removing compatibility paths.

The existing textual IR unit tests remain useful as lowering tests during the transition. Native-object and executable integration tests additionally exercise the LLVM library and target-machine path.
