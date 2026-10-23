# RamLang

RamLang is a small C-inspired, statically typed systems-language experiment written in Rust. The compiler lowers source code to LLVM IR and uses LLVM/Clang to produce a native executable.

> **Development status:** an initial end-to-end compiler is implemented. This is an early language implementation, not yet a stable systems-language ABI or a complete C/C++ replacement.

## Current features

- Rust-based lexer, parser, compiler driver, and LLVM IR backend
- Native compilation through Clang (LLVM 15 or newer, using opaque pointers)
- Static types: `i32`, `i64`, `f64`, `bool`, and `void`
- Functions, typed parameters, return values, recursive calls
- Immutable local variables with `let` and mutable locals with `let mut`
- Arithmetic, comparisons, unary `-` and `!`
- `if` / `else`, `while`, nested blocks, line and block comments
- Built-in `print(value)` for integer, floating-point, and boolean values
- Unit tests for code generation and common semantic errors

## Requirements

- Rust stable (edition 2024; Rust 1.85 or newer)
- LLVM/Clang 15 or newer for producing native executables
- A native C toolchain/runtime provided by the operating system

On Arch Linux / CachyOS, install the required tools with:

```sh
sudo pacman -S --needed rust clang llvm
```

If Clang is not named `clang` or is outside `PATH`, set `RAMLANG_CLANG` to its executable path.

## Build and test

```sh
cargo build --release
cargo test
```

## Example

Create `hello.ram`:

```rust
fn square(x: i32) -> i32 {
    return x * x;
}

fn main() -> i32 {
    let answer: i32 = square(7);
    print(answer);
    return 0;
}
```

Compile and run it:

```sh
cargo run --release -- hello.ram -o hello
./hello
```

Expected output:

```text
49
```

## Compiler options

```sh
# Parse and type-check without building a binary
ramcomp hello.ram --check

# Emit LLVM IR to hello.ll
ramcomp hello.ram --emit-llvm hello.ll

# Compile to a native executable
ramcomp hello.ram -o hello
```

When using Cargo before installing the binary, replace `ramcomp` with `cargo run --release -- <source.ram> ...`.

## Syntax overview

```rust
fn add(a: i32, b: i32) -> i32 {
    return a + b;
}

fn main() -> i32 {
    let fixed: i32 = add(20, 22);
    let mut counter: i32 = 0;

    while (counter < 3) {
        print(fixed + counter);
        counter = counter + 1;
    }

    if (fixed == 42) {
        print(true);
    } else {
        print(false);
    }

    return 0;
}
```

## Current limitations

This is an early, intentionally small compiler. Strings, user-defined structs, pointers/references, arrays, modules, imports, external function declarations, generics, memory allocation APIs, and a formal ownership model are not implemented yet. Integer division uses signed LLVM division. There is no stable language specification or cross-platform target configuration yet.

The compiler emits opaque-pointer LLVM IR, so the current native path targets LLVM/Clang 15+. LLVM IR is an implementation boundary, not a promise that every future RamLang feature or target is already supported.

## License

GPL-3.0. See [LICENSE](LICENSE).
