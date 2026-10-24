# ramlang-core

`ramlang-core` is the first backend-neutral semantic-core milestone for Ramlang. It has no third-party dependencies and builds as ordinary Rust/Cargo code.

## Run the reference interpreter

On the pinned Rust toolchain:

```sh
cargo run -p ramlang-core --bin ramrun -- examples/hello.ram
```

Expected output:

```text
49
```

An instruction budget can be set with `--fuel`:

```sh
cargo run -p ramlang-core --bin ramrun -- examples/hello.ram --fuel 100000
```

## Supported source subset

The initial interpreter accepts function declarations, scalar integer and boolean parameters and return values, integer arithmetic, comparisons, boolean `&&` / `||` (short circuit), local `let` bindings, `let mut`, assignment to mutable locals, `if` / `else`, `while`, function calls, `return`, line/block comments, and `print(integer_or_bool)`.

For this early reference runner, source integer annotations such as `i8`, `i32`, `i64`, and unsigned fixed-width integer types are normalized to an internal checked `i64`. This is a deliberate limitation, not target-accurate width semantics. Strings, structs, arrays, pointers, extern declarations, async functions, networking built-ins, imports, casts, and low-level operations are not supported in this subset and produce diagnostics or are not yet represented.

This runner is independent of LLVM, but it does not replace `ramcomp`: the existing compiler still requires LLVM and the current native linker path. The next migration stage is to share parsing/type-checking infrastructure, expand typed IR semantics, and compile this IR to native code.
