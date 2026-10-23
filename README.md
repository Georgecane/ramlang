# RamLang

RamLang is a small C-inspired, statically typed systems-language experiment written in Rust. The compiler lowers source code to LLVM IR and uses LLVM/Clang to produce a native executable.

> **Development status:** an initial end-to-end compiler is implemented. This is an early language implementation, not yet a stable systems-language ABI or a complete C/C++ replacement.

## Current features

- Rust-based lexer, parser, compiler driver, and LLVM IR backend
- Native compilation through Clang (LLVM 15 or newer, using opaque pointers)
- Static types: `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`, `f64`, `bool`, `str`, `ptr`, and `void`
- Functions, typed parameters, return values, recursive calls, and `extern fn` declarations for C ABI interop
- Global scalar, boolean, null-pointer, and UTF-8 string constants, available across source files
- Static global storage with immutable `static` and mutable `static mut` variables
- Target-aware `size_of(Type)`, `align_of(Type)`, and `offset_of(Type, field)` layout queries
- LLVM bit intrinsics for population count, leading/trailing zero counts, byte swap, and bit reversal
- Runtime `assert(bool)` with a trap on failure, `panic(str)` for non-returning errors, `assume(bool)` lowered to `llvm.assume`, volatile memory access built-ins, and LLVM memory-order fences
- Immutable local variables with `let` and mutable locals with `let mut`
- Explicit casts with `as` between signed integer widths, integers and `f64`, integers and `bool`, `bool` and `f64`, and `str` / `ptr`
- Arithmetic (`+`, `-`, `*`, `/`, `%`), signed comparisons, bitwise (`&`, `|`, `^`, `~`) and shift (`<<`, `>>`) operations, unary `-` and `!`, and short-circuit `&&` / `||`
- `if` / `else`, `while`, C-style `for` loops, `break`, `continue`, nested loops and blocks, line and block comments
- UTF-8 string literals with `\\n`, `\\r`, `\\t`, `\\0`, `\\\\`, and `\\\"` escapes
- Built-in `print(value)` for integers, floating-point values, booleans, and strings
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

## Assertions and optimizer assumptions
`panic(message)` writes a string message to standard output through the current C `printf` runtime and then traps. It is treated as a non-returning operation by the function code generator.

```rust
fn fail(message: str) -> void {
    panic(message);
}

fn main() -> i32 {
    let ok: bool = true;
    if (ok) {
        print("program is healthy");
    } else {
        fail("the program entered an invalid state");
    }
    return 0;
}
```


Use `assert(condition)` to trap if a boolean condition is false. `assume(condition)` emits LLVM's `llvm.assume` intrinsic, which tells the optimizer the condition is true; violating that assumption can make the program's behavior undefined and should only be used when the condition is guaranteed.

```rust
fn main() -> i32 {
    let value: i32 = 42;
    assert(value == 42);
    assume(value > 0);
    print(value);
    return 0;
}
```

## Static global variables

Use `static name: Type = value;` for immutable global storage or `static mut name: Type = value;` for mutable global storage with process lifetime. Global variables can be read by any function and, when mutable, assigned or addressed with `&`. Static scalar, boolean, floating-point, and string initializers are supported. Raw pointer statics can also use an explicit integer address cast, such as `static mut UART_BASE: ptr = 1073741824 as ptr;`, for target-specific MMIO mappings.

```rust
static mut interrupt_count: i32 = 0;
static status_mask: u32 = 15;
static device_name: str = "device-ready";

fn main() -> i32 {
    interrupt_count = interrupt_count + 1;
    print(interrupt_count);
    print(status_mask);
    print(device_name);
    return 0;
}
```

An integer-initialized raw pointer is target-specific and unsafe; do not run or dereference such a pointer unless the address is valid on the selected machine.

Mutable statics are shared process-wide state. They are not automatically synchronized across threads or interrupt contexts, and taking a raw pointer bypasses assignment mutability checks. Complex initializers such as global struct literals and array literals are not supported yet.

## Global constants

Declare compile-time constants at file scope with `const`. Scalar constants are substituted into expressions as LLVM constants, and string constants are emitted as immutable UTF-8 data. Constants in one source file can be referenced from another file in the same compilation.

```rust
const ANSWER: i32 = 42;
const GREETING: str = "RamLang";

fn main() -> i32 {
    print(ANSWER);
    print(GREETING);
    return 0;
}
```

The initial implementation accepts literal values for global constants (numeric, boolean, string, or null). It does not yet evaluate arbitrary constant expressions or initialize global arrays/structs.

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

## Struct field offsets

Use `offset_of(Type, field)` (or `offsetof(Type, field)`) to query a field's byte offset within a named struct. It uses LLVM's layout for the active target, including padding inserted for field alignment.

```rust
struct Packet {
    tag: u8,
    value: i64,
    tail: u32,
}

fn main() -> i32 {
    print(offset_of(Packet, tag));
    print(offset_of(Packet, value));
    print(offset_of(Packet, tail));
    return 0;
}
```

Offsets depend on the selected target ABI and the struct's declaration order. RamLang does not yet provide packed structs or an explicit C-layout/packing annotation, so use this only where the language's current LLVM struct layout matches the ABI you need.

## Type alignment

Use `align_of(Type)` (or `alignof(Type)`) to query ABI alignment in bytes for the selected target. The compiler derives it from LLVM's data layout rules for scalar, fixed-array, pointer, and struct types.

```rust
struct Aligned {
    byte: u8,
    wide: i64,
}

fn main() -> i32 {
    print(align_of(i8));
    print(align_of(i32));
    print(align_of([i16; 4]));
    print(align_of(Aligned));
    return 0;
}
```

This is ABI alignment for the target selected by Clang, not a guarantee that an arbitrary address or manually allocated pointer is aligned. The compiler does not yet enforce typed-pointer alignment or validate raw pointer addresses.

## Type sizes

Use `size_of(Type)` (or `sizeof(Type)`) to obtain the size in bytes as an `i64`. LLVM determines struct and pointer sizes for the selected compilation target, and fixed-length arrays use the element size multiplied by the declared length.

```rust
struct Cell {
    value: i32,
}

fn main() -> i32 {
    let bytes: i64 = size_of(Cell);
    let memory: ptr = alloc(bytes);
    let mut cell: Cell = memory as Cell;
    cell.value = 42;
    print(cell.value);
    free(memory);
    return 0;
}
```

`size_of(void)` is invalid. Cross-compiling may change pointer and struct sizes; use this intrinsic instead of hardcoding ABI-dependent byte counts. The current allocator interface uses an `i64` size and targets a 64-bit host ABI in the provided examples.

## Bit-manipulation intrinsics

For integer widths 8, 16, 32, and 64, RamLang exposes `ctpop_N` (population count), `ctlz_N` (count leading zero bits), `cttz_N` (count trailing zero bits), and `bitreverse_N` (reverse all bits), where `N` is the width in bits. `bswap_N` swaps bytes for widths 16, 32, and 64. These lower to LLVM intrinsics so the selected backend can choose efficient target instructions.

```rust
fn main() -> i32 {
    print(ctpop_32(255));
    print(ctlz_32(1));
    print(cttz_32(8));
    print(bswap_32(16909060));
    print(bitreverse_32(1));
    return 0;
}
```

The count-leading/trailing-zero intrinsics pass LLVM's defined-zero mode, so a zero input returns the bit width rather than making the result undefined. An input's integer width must match the suffix; cast literals explicitly when a narrower width is intended.

## Memory-order fences

RamLang exposes `fence_acquire()`, `fence_release()`, `fence_acq_rel()`, and `fence_seq_cst()`, which lower to LLVM fence instructions with the corresponding ordering. They are intended for code that interoperates with atomic or synchronization mechanisms in native libraries.

```rust
fn main() -> i32 {
    let mut shared_value: u32 = 0;
    let address: ptr = &shared_value;
    fence_release();
    volatile_store_u32(address, 42 as u32);
    fence_seq_cst();
    let observed: u32 = volatile_load_u32(address);
    fence_acquire();
    print(observed);
    return 0;
}
```

Fences alone do not make non-atomic data-race access safe and do not replace atomic operations, locks, or target-specific device barriers. Pair the ordering primitives with a correct synchronization protocol; MMIO requirements are architecture and device specific.

## Memory-mapped I/O example

`examples/mmio_registers.ram` demonstrates a static raw-pointer base, `volatile_load_u32`, and `volatile_store_u32`. CI compiles it to LLVM IR only and checks the emitted pointer constant and volatile instructions; it does not execute the register accessors.

```sh
cargo run --release -- examples/mmio_registers.ram --emit-llvm mmio.ll
```

The example address is only a placeholder. Replace it with the address and access widths documented for your specific board/device, and add any platform-required barriers. Running a program that dereferences an invalid MMIO address may crash or damage hardware.

## Unaligned memory access

The `load_unaligned_*` and `store_unaligned_*` built-ins access raw memory with LLVM alignment `1`, avoiding an implicit requirement that the pointer be naturally aligned for the selected scalar type. The matching `volatile_load_unaligned_*` and `volatile_store_unaligned_*` variants combine alignment `1` with volatile semantics.

```rust
fn main() -> i32 {
    let memory: ptr = alloc(8);
    let unaligned_address: ptr = ptr_offset(memory, 1);
    store_unaligned_i32(unaligned_address, 42);
    print(load_unaligned_i32(unaligned_address));
    free(memory);
    return 0;
}
```

These operations still require a valid pointer to enough accessible bytes. They do not make invalid pointers safe or guarantee that an underlying device supports unaligned accesses. On architectures that fault or require special handling for unaligned MMIO, follow the hardware documentation instead.

## Volatile memory access

Use `volatile_load_i32(ptr)`, `volatile_store_i32(ptr, value)`, and the matching built-ins for `i8/i16/i64`, `u8/u16/u32/u64`, `f64`, `bool`, and `ptr` when reads and writes must remain observable to LLVM. These lower to LLVM's `load volatile` and `store volatile` instructions and are intended for memory-mapped device registers, hardware status words, and similar externally changing memory.

```rust
fn main() -> i32 {
    let mut register_shadow: u32 = 0;
    let register: ptr = &register_shadow;
    volatile_store_u32(register, 42 as u32);
    let value: u32 = volatile_load_u32(register);
    print(value);
    return 0;
}
```

Volatile access is not a synchronization primitive and does not provide atomicity, thread ordering, device-specific barriers, or safe access to arbitrary addresses. A real MMIO program must use a valid target-specific address, suitable access width/alignment, and any barriers required by the hardware.

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
