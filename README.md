# RamLang

RamLang is a small C-inspired, statically typed systems-language experiment written in Rust. The compiler lowers source code to LLVM IR and uses LLVM/Clang to produce a native executable.

> **Development status:** an initial end-to-end compiler is implemented. This is an early language implementation, not yet a stable systems-language ABI or a complete C/C++ replacement.

## Current features

- Rust-based lexer, parser, compiler driver, and LLVM IR backend
- Native compilation through Clang (LLVM 15 or newer, using opaque pointers)
- Static types: `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`, `f64`, `bool`, `str`, `ptr`, and `void`
- Functions, typed parameters, return values, recursive calls, and `extern fn` declarations for C ABI interop
- Immutable local variables with `let` and mutable locals with `let mut`
- Explicit casts with `as` between signed integer widths, integers and `f64`, integers and `bool`, `bool` and `f64`, and `str` / `ptr`
- Arithmetic (`+`, `-`, `*`, `/`, `%`), signed comparisons, bitwise (`&`, `|`, `^`, `~`) and shift (`<<`, `>>`) operations, unary `-` and `!`, and short-circuit `&&` / `||`
- `if` / `else`, `while`, C-style `for` loops, `break`, `continue`, nested loops and blocks, line and block comments
- UTF-8 string literals with `\\n`, `\\r`, `\\t`, `\\0`, `\\\\`, and `\\\"` escapes\n- Built-in `print(value)` for integers, floating-point values, booleans, and strings
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

## Unsigned integers

Unsigned integer types cover values from zero through their full bit-width maximum. Arithmetic uses the same LLVM integer representation as the matching signed type, but division, remainder, ordering comparisons, right shifts, integer-to-float conversions, and widening casts follow unsigned semantics.

```rust
fn main() -> i32 {
    let word: u32 = 4294967295;
    let wide: u64 = 18446744073709551615;
    print(word);
    print(wide);
    print(word / 5);
    print(wide >> 63);
    return 0;
}
```

A direct negative literal cannot initialize an unsigned variable; use an explicit cast when wrapping behavior is intentional. Arithmetic is low-level and does not insert overflow checks.

## Explicit casts

RamLang requires explicit conversions between different numeric types. Casts lower to LLVM conversion instructions; narrowing integer casts truncate bits, while widening signed integers sign-extend. Floating-point to integer casts truncate toward zero for values representable by the destination type. As with other low-level languages, casts do not perform run-time range checks.

```rust
fn main() -> i32 {
    let fraction: f64 = 258.75;
    let number: i32 = fraction as i32;
    let byte: i8 = number as i8;
    let widened: i64 = byte as i64;
    let flag: bool = byte as bool;
    print(number);
    print(widened);
    print(flag);
    return 0;
}
```

`bool` converts to integers as 0 or 1; integer zero converts to `false` and any nonzero value converts to `true`. Casts to `void` and floating-point/pointer casts are rejected. Explicit integer/pointer casts lower to LLVM `inttoptr` and `ptrtoint`; they are inherently unsafe, target-dependent, and can create invalid or misaligned addresses. Unsigned integer types `u8`, `u16`, `u32`, and `u64` use zero extension and unsigned comparisons/division/right-shift lowering; casts from unsigned integers to `f64` use unsigned conversion instructions.

## Strings

String literals are stored as immutable UTF-8 constants in the generated LLVM module. The `str` type is passed as a NUL-terminated pointer when calling the current C runtime. Supported escape sequences are `\\n`, `\\r`, `\\t`, `\\0`, `\\\\`, and `\\\"`.

```rust
fn main() -> i32 {
    let greeting: str = "Hello, LLVM!";
    print(greeting);
    print("UTF-8 is supported: café");
    return 0;
}
```

String values currently follow C-string behavior: embedded NUL bytes terminate output, strings are immutable, and there is no string concatenation, slicing, or managed lifetime system yet.

## C ABI interoperability
### Linking external native libraries

Use `--link-lib` (or `-l`) to link a library, `--link-search` (or `-L`) to add a library search directory, and `--link-arg` to pass an additional Clang argument:

```sh
ramcomp examples/ffi_math.ram -o math-demo --target x86_64-unknown-linux-gnu --opt-level 2 --link-search /usr/lib/x86_64-linux-gnu --link-lib m
./math-demo
```

The example calls `sqrt` from the system math library. `--target` forwards a target triple to Clang, and `--opt-level` accepts `0`, `1`, `2`, `3`, `s`, or `z` (default `2`). Cross-compiling may require a matching linker, sysroot, and target libraries. Target, optimization, and linker options apply only to native compilation and are rejected with `--check` and `--emit-llvm`.


External functions can be declared using `extern fn`. Calls use the platform's default C ABI, and the final native link must be able to resolve the referenced symbol.

```rust
extern fn abs(value: i32) -> i32;

fn main() -> i32 {
    print(abs(-42));
    return 0;
}
```

This example links against the host C runtime and prints `42`. At present, FFI uses the supported scalar and raw-pointer types; variadic declarations and per-library link metadata inside source files are not implemented. Pass library search paths and link flags on the compiler command line.

### Explicit address conversion

`ptr` may be converted to an integer type with `as`, and integer values may be converted back to `ptr`. Use this only for low-level code where target pointer width and address validity are understood:

```rust
fn main() -> i32 {
    let memory: ptr = alloc(4);
    let address: u64 = memory as u64;
    let alias: ptr = address as ptr;
    store_i32(alias, 42);
    print(load_i32(memory));
    free(memory);
    return 0;
}
```

Do not dereference arbitrary integer-derived addresses. On targets with pointers wider than the chosen integer type, an integer round-trip may lose information; prefer the target's native pointer-width integer type when adding platform abstractions.

## Named structs

Declare a structure with named fields, construct a value using a field initializer, and read or update fields with dot syntax. Struct values are pointer-backed in this initial implementation; struct literals allocate their storage in the function's stack frame. Use `let mut` when fields must be reassigned.

```rust
struct Point {
    x: i32,
    y: i32,
}

fn main() -> i32 {
    let mut point: Point = Point { x: 20, y: 22 };
    print(point.x + point.y);
    point.x = 30;
    print(point.x);
    return 0;
}
```

Struct declarations may be placed in a different `.ram` file from the functions that use them. Pass all files to one `ramcomp` invocation. Current limitations: structs do not have constructors or methods, field assignment supports a direct local struct variable, and struct storage follows pointer/stack lifetime rules rather than automatic ownership.

## Fixed-length arrays

Use `[element_type; length]` for a fixed-length view of a memory region, with indexed reads and writes. Every access emits a runtime bounds check and branches to `llvm.trap` if the index is outside the declared length. The compiler does not own the memory: allocate enough aligned storage for all elements, and free heap storage exactly once.

```rust
fn main() -> i32 {
    let values: [i32; 4] = alloc(16) as [i32; 4];
    let mut index: i32 = 0;
    while (index < 4) {
        values[index] = index * 10;
        index = index + 1;
    }
    print(values[3]);
    free(values as ptr);
    return 0;
}
```

Array types support scalar and pointer elements, including explicit-length literals such as `let values: [i32; 4] = [10, 20, 30, 40];`. Array literals use stack storage and must not outlive the function call. An array type is a pointer plus a declared length, so the bounds check protects the declared index range; it cannot prove that the underlying allocation actually contains that much memory. The programmer must keep the allocation size and array type consistent.

## Address-of

The unary `&` operator yields a raw pointer to a local variable, an array element, or a struct field. This allows the existing `load_*` and `store_*` operations and C ABI calls to work with stack storage without allocating separate heap memory.

```rust
fn main() -> i32 {
    let mut value: i32 = 41;
    let address: ptr = &value;
    store_i32(address, value + 1);
    print(value);
    return 0;
}
```

The pointer is non-owning and follows the lifetime of its underlying storage. Do not return or retain a pointer to a local after its stack frame ends. Taking the address of a temporary expression is rejected.

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

## Multiple source files

Pass multiple `.ram` files to one compiler invocation. All functions from the inputs are combined into one LLVM module, so a function in one file can call a function declared in another input file. Every input file must contain at least one function; modules, namespaces, and import declarations are not yet implemented.

```sh
ramcomp examples/multi_file/main.ram examples/multi_file/math.ram -o program
./program
```

The compiler can also emit intermediate native artifacts without linking:

```sh
# LLVM IR
ramcomp examples/hello.ram --emit-llvm output.ll

# Object file
ramcomp examples/hello.ram --emit-obj -o output.o

# Target assembly
ramcomp examples/hello.ram --emit-asm -o output.s
```

The `--emit-obj` and `--emit-asm` modes still use Clang's backend. You can then link the object file with another native object or library, or assemble/link the `.s` output with the system toolchain.

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

This is an early, intentionally small compiler. Raw pointers and manual heap allocation are supported through `ptr`, `null`, `alloc`, `free`, `ptr_offset`, `load_i8`/`load_i16`/`load_i32`/`load_i64`, `load_u8`/`load_u16`/`load_u32`/`load_u64`, `load_f64`/`load_bool`/`load_ptr`, and matching `store_*` intrinsics. These operations are unsafe by design: the programmer is responsible for allocation size, alignment, bounds, lifetime, and avoiding use-after-free or double-free. Typed pointer safety, nested arrays, modules/import declarations, variadic foreign calls, generics, constructors/methods, and a formal ownership model are not implemented yet. Signed division uses LLVM `sdiv`; unsigned division uses `udiv`. There is no stable language specification or cross-platform target configuration yet.

The compiler emits opaque-pointer LLVM IR, so the current native path targets LLVM/Clang 15+. LLVM IR is an implementation boundary, not a promise that every future RamLang feature or target is already supported.

## License

GPL-3.0. See [LICENSE](LICENSE).
