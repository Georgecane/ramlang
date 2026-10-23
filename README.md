# RamLang

RamLang is a small C-inspired, statically typed systems-language experiment written in Rust. The compiler lowers source code to LLVM IR and uses LLVM/Clang to produce a native executable.

> **Development status:** an initial end-to-end compiler is implemented. This is an early language implementation, not yet a stable systems-language ABI or a complete C/C++ replacement.

## Current features

- Rust-based lexer, parser, compiler driver, and LLVM IR backend
- Native compilation through Clang (LLVM 15 or newer, using opaque pointers)
- Static types: `i8`, `i32`, `i64`, `f64`, `bool`, `ptr`, and `void`
- Functions, typed parameters, return values, recursive calls, and `extern fn` declarations for C ABI interop
- Immutable local variables with `let` and mutable locals with `let mut`
- Arithmetic (`+`, `-`, `*`, `/`, `%`), comparisons, unary `-` and `!`, and short-circuit `&&` / `||`
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

## C ABI interoperability

External functions can be declared using `extern fn`. Calls use the platform's default C ABI, and the final native link must be able to resolve the referenced symbol.

```rust
extern fn abs(value: i32) -> i32;

fn main() -> i32 {
    print(abs(-42));
    return 0;
}
```

This example links against the host C runtime and prints `42`. At present, FFI uses the supported scalar and raw-pointer types; variadic declarations, string literals, and per-library link flags are not implemented.

## Explicit memory management

Raw memory operations are intentionally explicit. This example allocates four bytes, stores an integer, reads it back, and frees the allocation:

```rust
fn main() -> i32 {
    let p: ptr = alloc(8);
    store_i32(p, 7);
    let q: ptr = ptr_offset(p, 4);
    store_i32(q, 42);
    print(load_i32(q));
    free(p);
    return 0;
}
```

The caller must allocate enough correctly aligned memory for each load/store operation and must not use a pointer after freeing it. The first implementation targets host platforms with a 64-bit `size_t` for the current `alloc(byte_count: i64)` ABI.

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

This is an early, intentionally small compiler. Raw pointers and manual heap allocation are supported through `ptr`, `null`, `alloc`, `free`, `ptr_offset`, `load_i8`/`load_i32`/`load_i64`/`load_f64`/`load_bool`/`load_ptr`, and matching `store_*` intrinsics. These operations are unsafe by design: the programmer is responsible for allocation size, alignment, bounds, lifetime, and avoiding use-after-free or double-free. Typed pointer safety, arrays, user-defined structs, strings, modules, imports, variadic foreign calls, generics, and a formal ownership model are not implemented yet. Integer division uses signed LLVM division. There is no stable language specification or cross-platform target configuration yet.

The compiler emits opaque-pointer LLVM IR, so the current native path targets LLVM/Clang 15+. LLVM IR is an implementation boundary, not a promise that every future RamLang feature or target is already supported.

## License

GPL-3.0. See [LICENSE](LICENSE).
