# ramlang-jit

`ramlang-jit` is a native-code execution path for a restricted subset of Ramlang. It compiles the backend-neutral `ramlang-core` IR to host machine code through the Rust Cranelift crates and executes it in-process. It does not use LLVM, Clang, a C source generator, or an external linker.

## Try it

```sh
cargo run -p ramlang-jit --bin ramjit -- examples/hello.ram
```

Expected output:

```text
49
```

The JIT subset supports i64-normalized integer values, booleans, functions/calls, conditionals, loops, short-circuit boolean logic, integer arithmetic/comparisons, and numeric/boolean `print`. It applies an instruction-fuel guard and checks signed integer overflow and invalid division before executing the dangerous operation.

The source parser accepts fixed-width integer annotations for migration convenience, but this stage normalizes them all to checked i64. That is not the final language's integer-width semantics. String values and low-level operations are currently interpreter-only; `ramjit` reports a clear error for programs that require string values. Use `ramrun` for the supported string subset.

The output is JIT machine code executed in the current process, not an ahead-of-time executable artifact. A later stage can reuse this backend-neutral IR for an object-file backend and define platform linking separately.
