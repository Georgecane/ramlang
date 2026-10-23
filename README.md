# RamLang

RamLang is a small C-inspired, statically typed systems-language experiment written in Rust. The compiler lowers source code to LLVM IR and uses LLVM/Clang to produce a native executable.

> **Development status:** an initial end-to-end compiler is implemented. This is an early language implementation, not yet a stable systems-language ABI or a complete C/C++ replacement.

## Current features

- Rust-based lexer, parser, compiler driver, and LLVM IR backend
- Native compilation through Clang (LLVM 15 or newer, using opaque pointers), with LLVM IR, object, assembly, and bitcode output modes
- Static types: `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`, `f64`, `bool`, `str`, `ptr`, and `void`
- Functions, typed parameters, return values, recursive calls, and `extern fn` declarations for C ABI interop
- Type aliases for scalar, array, struct, and enum types, including chained aliases
- Simple enums with automatic or explicit integer discriminants, including cross-file type resolution
- `extern static` declarations for C-defined scalar and raw-pointer globals
- Variadic `extern fn` calls with C default argument promotions for extra scalar arguments
- Global scalar, boolean, null-pointer, and UTF-8 string constants, available across source files
- Static global storage with immutable `static` and mutable `static mut` variables
- Target-aware `size_of(Type)`, `align_of(Type)`, and `offset_of(Type, field)` layout queries, including `packed struct` layouts
- Target-ABI-aware `size_t` calls for allocation, string, and memory routines, with checked 64-to-32-bit byte-count conversion
- LLVM bit intrinsics for population count, leading/trailing zero counts, byte swap, and bit reversal
- Checked signed and unsigned integer add, subtract, and multiply with overflow flags, plus saturating add/subtract
- Typed LLVM inline assembly built-ins for target-specific instructions
- Typed atomic integer loads, stores, exchanges, and read-modify-write operations with explicit memory ordering
- Runtime `assert(bool)` with a trap on failure, `panic(str)` for non-returning errors, `assume(bool)` lowered to `llvm.assume`, volatile memory access built-ins, and LLVM memory-order fences
- Immutable local variables with `let` and mutable locals with `let mut`
- Explicit casts with `as` between signed integer widths, integers and `f64`, integers and `bool`, `bool` and `f64`, and `str` / `ptr`
- Arithmetic (`+`, `-`, `*`, `/`, `%`), signed comparisons, bitwise (`&`, `|`, `^`, `~`) and shift (`<<`, `>>`) operations, unary `-` and `!`, and short-circuit `&&` / `||`
- `if` / `else`, `while`, C-style `for` loops, `break`, `continue`, nested loops and blocks, line and block comments
- UTF-8 string literals with `\\n`, `\\r`, `\\t`, `\\0`, `\\\\`, and `\\\"` escapes
- Built-in `print(value)` for integers, floating-point values, booleans, and strings
- C-compatible string length/comparison and byte-search routines (`strlen`, `strcmp`, `strncmp`, `memchr`)
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

## String and byte-search routines

The compiler exposes `strlen(str)` for the byte length of a NUL-terminated string, `strcmp(left, right)` and `strncmp(left, right, byte_count)` for lexicographic comparison, and `memchr(buffer, byte, byte_count)` to find the first occurrence of a byte in a buffer.

```rust
fn main() -> i32 {
    print(strlen("RamLang"));
    print(strcmp("RamLang", "RamLang"));
    print(strncmp("RamLang", "Ram", 3));

    let found: ptr = memchr("RamLang", 76, 7);
    print(load_u8(found));
    return 0;
}
```

`strlen` excludes the terminating NUL and returns a byte count, not a Unicode character count. The compare functions return negative, zero, or positive values; only the sign is portable. `memchr` returns `null` when the byte is not found, so check its result before dereferencing it. Buffer lengths and pointer validity remain the caller's responsibility.

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

## LLVM bitcode output

Use `--emit-bc` to compile the generated LLVM IR into a `.bc` bitcode file without linking a native executable. You can feed that file into later Clang/LLVM stages or link it with other bitcode and native objects.

```sh
ramcomp examples/hello.ram --emit-bc -o hello.bc
clang hello.bc -o hello
./hello
```

The bitcode is generated by the configured Clang toolchain, so it is tied to the LLVM bitcode format/version compatibility of the LLVM tools that consume it. `--target` and `--opt-level` can be used with `--emit-bc`; linker flags cannot, because this mode does not link.

## LLVM inline assembly

The `asm_i8`, `asm_i16`, `asm_i32`, `asm_i64`, unsigned-width variants, `asm_f64`, `asm_bool`, `asm_ptr`, and `asm_void` built-ins emit side-effecting LLVM inline assembly. The assembly template and constraint string must be string literals; any following arguments become typed LLVM input operands. Clang validates the constraints and target instruction syntax during native code generation.

```rust
fn main() -> i32 {
    let value: i32 = asm_i32("mov $1, $0", "=r,r", 42);
    asm_void("nop", "");
    print(value);
    return 0;
}
```

Constraints follow LLVM's inline-assembly convention: output constraints come first, then input constraints, with clobbers such as `~{memory}` included in the constraint string when appropriate. The result type and output constraints must agree. These built-ins are target-specific and unsafe: preserve the platform ABI, register rules, stack alignment, and required clobbers, and select the correct `--target`. No instruction-set emulation or portability layer is added by RamLang.

## Target CPU, features, and sysroot

The backend driver accepts explicit LLVM CPU and feature controls for native code generation:

```sh
ramcomp examples/hello.ram \
  --target x86_64-unknown-linux-gnu \
  --target-cpu x86-64 \
  --target-feature +sse2 \
  --sysroot /path/to/sysroot \
  --opt-level 3 \
  -o hello
```

Repeat `--target-feature` to enable or disable multiple target features, for example `--target-feature +avx2 --target-feature -fma`. These values are forwarded to Clang's LLVM backend; feature names and CPU availability depend on the selected target. `--sysroot` is passed to Clang for locating target headers, libraries, and runtime components. A matching linker and target runtime may still be required for cross-compilation.

Target CPU, target feature, and sysroot options require backend code generation and therefore cannot be combined with `--check` or text-only `--emit-llvm`. They can be used with native executable, object, assembly, or bitcode output modes.

## C ABI interoperability
### Linking external native libraries

Use `--link-lib` (or `-l`) to link a library, `--link-search` (or `-L`) to add a library search directory, and `--link-arg` to pass an additional Clang argument:

```sh
ramcomp examples/ffi_math.ram -o math-demo --target x86_64-unknown-linux-gnu --opt-level 2 --link-search /usr/lib/x86_64-linux-gnu --link-lib m
./math-demo
```

The example calls `sqrt` from the system math library. `--target` selects a compilation target, and `--opt-level` accepts `0`, `1`, `2`, `3`, `s`, or `z` (default `2`). Supported common 32-bit and 64-bit triples drive target-sized `size_t` declarations and are embedded in emitted LLVM IR; `--target` can be combined with `--check` and `--emit-llvm` to validate or inspect ABI-specific lowering. Optimization levels are rejected with `--check` and `--emit-llvm`, because those modes do not invoke code-generation optimization. Cross-compiling may require a matching linker, sysroot, and target libraries. Linker options are rejected in check and emit-only modes.


External functions can be declared using `extern fn`. Calls use the platform's default C ABI, and the final native link must be able to resolve the referenced symbol.

```rust
extern fn abs(value: i32) -> i32;

fn main() -> i32 {
    print(abs(-42));
    return 0;
}
```

This example links against the host C runtime and prints `42`. At present, FFI uses the supported scalar and raw-pointer types; variadic declarations and per-library link metadata inside source files are not implemented. Pass library search paths and link flags on the compiler command line.

### Variadic C ABI functions

Declare an external C function with a trailing `...` in its parameter list. Calls must provide all fixed parameters, then may pass additional supported scalar/pointer arguments. For the extra arguments, RamLang applies C default argument promotions: `bool`, `i8`, `u8`, `i16`, and `u16` are promoted to `i32`; `f64` is passed as C `double`.

```rust
extern fn variadic_sum(base: i32, ...) -> i32;

fn main() -> i32 {
    let small: i8 = -5;
    let medium: u16 = 250;
    let enabled: bool = true;
    let result: i32 = variadic_sum(10, small, medium, enabled, 2.0);
    print(result);
    return result - 258;
}
```

The companion C function is in `examples/variadic_ffi.c`, and CI links it against the RamLang object file. Variadic calls cannot safely infer types from a C format string, and arrays/structs passed by value through `...` are rejected. Make sure the declared fixed parameters and every extra argument exactly match the foreign function's ABI contract.

### External global variables

Use `extern static mut NAME: Type;` for a writable C global or `extern static NAME: Type;` for a read-only reference. These declarations are emitted as LLVM external globals and resolved by the final linker.

```rust
extern static mut C_COUNTER: i32;
extern static C_LIMIT: u32;

fn main() -> i32 {
    C_COUNTER = C_COUNTER + 2;
    print(C_COUNTER);
    print(C_LIMIT);
    return 0;
}
```

The corresponding C definitions live in `examples/extern_globals.c`; CI compiles RamLang to an object file, links it with the C object, and checks the result. External aggregate globals such as inline C structs are not implemented yet. Ensure the declared type, mutability, symbol name, and target ABI match the C definition.

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

Global numeric and boolean constants support compile-time arithmetic, bitwise operations, shifts, unary operators, comparisons, and references to earlier constants. Integer expressions are overflow-checked against the declared type, division by zero is rejected, and floating-point constant results must remain finite. Forward references, string concatenation, and global array/struct initializers are not implemented yet.

## Enums

RamLang supports simple enums with automatic or explicit signed 32-bit discriminants. Use `EnumName.Variant` to refer to a variant, declare locals and parameters with the enum type, compare values, and cast to or from integer types when you need the numeric discriminant.

```rust
enum Color {
    Red,
    Green = 4,
    Blue,
}

fn color_code(color: Color) -> i32 {
    return color as i32;
}

fn main() -> i32 {
    let color: Color = Color.Green;
    print(color);
    print(color == Color.Green);
    print(size_of(Color));
    return color_code(Color.Blue) - 5;
}
```

Variants without an explicit value increment from the preceding discriminant, so `Blue` above has value `5`. Enums lower to their integer discriminants (currently `i32`) rather than tagged payload unions. Payload variants and enum constructors are not implemented yet.

## Match statements

Use `match (value)` to select a block from enum variants or scalar values. An enum match without a `_` fallback must cover every distinct discriminant; boolean matches without a fallback must include both `true` and `false`. Integer matches require a `_` fallback because their value domain is open-ended. Constant enum, integer, and boolean patterns use LLVM `switch`; dynamic patterns use an ordered comparison chain.

```rust
enum Color {
    Red,
    Green,
    Blue,
}

fn main() -> i32 {
    let color: Color = Color.Green;
    match (color) {
        Color.Red => { print(10); },
        Color.Green => { print(20); },
        Color.Blue => { print(30); },
    }
    return 0;
}
```

A wildcard can handle unmatched values in integer matches:

```rust
match (status_code) {
    200 => { print(1); },
    404 => { print(0); },
    _ => { print(-1); },
}
```

Match statements select blocks. Match expressions instead return a value: every reachable arm must produce the same type, and integer matches require a fallback. A diverging arm such as `panic("invalid input")` is allowed because it does not contribute a value to the result's LLVM `phi` node.

```rust
fn decode(code: i32) -> i32 {
    return match (code) {
        0 => 42,
        200 => 7,
        _ => panic("unsupported code"),
    };
}

fn main() -> i32 {
    print(decode(0));
    print(decode(200));
    return 0;
}
```

Statement matches and value-producing matches both lower constant enum, integer, and boolean patterns to an LLVM `switch`; value-producing matches merge their results with a `phi` node. If any pattern is dynamic, the compiler retains ordered comparison semantics. The lowered default edge for an exhaustive enum statement match traps if the underlying integer contains an invalid discriminant.

## Type aliases

Use `type Name = ExistingType;` to introduce another name for an existing type. Aliases are resolved to their canonical type before LLVM generation and may refer to scalar types, arrays, structs, enums, or other aliases. Chained aliases and aliases declared in imported source files are supported; circular aliases, duplicate aliases, and collisions with existing type declarations are errors.

```rust
type Scalar = i32;
type Number = Scalar;
type Samples = [i32; 3];

struct Vec2 {
    x: Number,
    y: Scalar,
}

type Position = Vec2;

fn sum(left: Number, right: Scalar) -> Number {
    return left + right;
}

fn main() -> i32 {
    let samples: Samples = [10, 20, 12];
    let point: Position = Position { x: 5, y: 7 };
    print(sum(samples[2], point.x));
    print(point.y);
    return 0;
}
```

Type aliases do not create distinct or strongly typed values; they are names for the underlying type. The alias may be declared before or after the type it names. Arrays can be aliased as a whole, but using a scalar alias directly as the inline element type in a new `[Alias; N]` declaration is not yet supported; write the underlying scalar type there.

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

Offsets depend on the selected target ABI and the struct's declaration order. Use `packed struct` to disable padding between fields; field reads, writes, and literal initialization then use LLVM alignment `1` so unaligned fields are not incorrectly assumed to have natural alignment.

```rust
packed struct Packet {
    tag: u8,
    value: i32,
    tail: u16,
}

fn main() -> i32 {
    print(offset_of(Packet, value));
    print(size_of(Packet));
    print(align_of(Packet));
    return 0;
}
```

For this layout, `value` is at byte offset 1, the total size is 7 bytes, and the struct alignment is 1 byte. A packed layout is not automatically compatible with every C ABI or device protocol; match the target's actual definition and access rules.

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

`size_of(void)` is invalid. Cross-compiling may change pointer and struct sizes; use this intrinsic instead of hardcoding ABI-dependent byte counts. Heap allocation and C memory routines use target-sized `size_t` arguments (`i32` on supported 32-bit targets, `i64` on supported 64-bit targets). The source-language `size_of` result remains `i64`, so the compiler converts sizes to the target ABI width at calls.

## Saturating integer arithmetic

RamLang also exposes `sadd_sat_i32`, `ssub_sat_i32`, `uadd_sat_u32`, and `usub_sat_u32), with width variants for 8, 16, 32, and 64-bit integers. These return the calculated value directly, clamping signed results to the signed minimum/maximum and unsigned results to zero or the unsigned maximum instead of wrapping.

```rust
fn main() -> i32 {
    let signed_max: i32 = sadd_sat_i32(2147483647, 1);
    let signed_min: i32 = ssub_sat_i32(-2147483648, 1);
    let unsigned_max: u16 = uadd_sat_u16(65535, 1);
    let unsigned_min: u16 = usub_sat_u16(0, 1);
    print(signed_max);
    print(signed_min);
    print(unsigned_max);
    print(unsigned_min);
    return 0;
}
```

These operations lower to LLVM saturating arithmetic intrinsics. Choose saturating arithmetic when clamping is the desired policy; use the checked operations when the caller must detect overflow explicitly.

## Checked integer arithmetic

Use `sadd_overflow_i32(left, right, output_ptr)`, `ssub_overflow_i32`, and `smul_overflow_i32` for signed integer widths 8, 16, 32, and 64. The matching `uadd_overflow_u32`, `usub_overflow_u32`, and `umul_overflow_u32` forms support unsigned widths. Each function stores the wrapped arithmetic result through the output pointer and returns a `bool` that is `true` when overflow occurred.

```rust
fn main() -> i32 {
    let mut result: i32 = 0;
    let output: ptr = &result;
    let overflow: bool = sadd_overflow_i32(2147483647, 1, output);
    print(result);
    print(overflow);
    return 0;
}
```

These functions lower to LLVM's `*.with.overflow` intrinsics. The output pointer must point to a writable, naturally aligned value of the matching integer type. The wrapped result is defined by the intrinsic; the overflow flag lets callers choose whether to trap, saturate, or handle the condition another way.

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

## Atomic memory operations

RamLang exposes typed atomic operations for integer widths 8, 16, 32, and 64:

- `atomic_load_i32(ptr)` and `atomic_load_i32_acquire(ptr)`
- `atomic_store_i32(ptr, value)` and `atomic_store_i32_release(ptr, value)`
- `atomic_exchange_i32(ptr, value)`
- `atomic_fetch_add_i32(ptr, value)`, `atomic_fetch_sub_i32`, `atomic_fetch_and_i32`, `atomic_fetch_or_i32`, `atomic_fetch_xor_i32`, and `atomic_fetch_nand_i32`
- Signed `atomic_fetch_min_i32` / `atomic_fetch_max_i32` and unsigned `atomic_fetch_umin_u32` / `atomic_fetch_umax_u32`
- `atomic_compare_exchange_i32(ptr, expected_ptr, desired)`, which returns a boolean and writes the observed value back through `expected_ptr`

All operations default to sequentially consistent ordering when no ordering suffix is supplied. Supported explicit suffixes are `relaxed`, `acquire`, `release`, `acq_rel`, and `seq_cst`, subject to LLVM's ordering rules: loads do not accept release semantics, and stores do not accept acquire semantics. `relaxed` maps to LLVM's `monotonic` ordering. Read-modify-write operations return the previous value.

```rust
fn main() -> i32 {
    let mut value: i32 = 0;
    let address: ptr = &value;
    atomic_store_i32_release(address, 10);
    print(atomic_load_i32_acquire(address));
    print(atomic_fetch_add_i32_acq_rel(address, 5));
    print(atomic_load_i32(address));
    return 0;
}
```

For compare-exchange, `expected_ptr` must point to writable, correctly aligned storage for the same integer type and must not alias the atomic object's address. On failure, the observed value is written to `*expected_ptr`; on success, it remains equal to the previous expected value.

Atomic addresses must be valid and aligned for the selected integer type. These operations do not make arbitrary surrounding non-atomic accesses race-free. Use a complete synchronization protocol and appropriate ownership of shared state; atomic operations are currently provided for integer types, not floats or aggregate values.

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

## Allocation, zero-initialization, and resizing

Alongside `alloc(bytes)` and `free(ptr)`, RamLang provides `calloc(count, element_size)` to allocate zero-initialized storage and `realloc(ptr, new_size)` to resize an existing allocation.

```rust
fn main() -> i32 {
    let memory: ptr = calloc(2, size_of(i32));
    if (memory == null) {
        return 1;
    }

    store_i32(memory, 42);
    let expanded: ptr = realloc(memory, 16);
    if (expanded == null) {
        free(memory);
        return 1;
    }

    print(load_i32(expanded));
    free(expanded);
    return 0;
}
```

If `realloc` fails for a nonzero size, the original allocation remains valid, so do not overwrite your only copy of the original pointer before checking the result. On success, use the returned pointer and do not use the old pointer. All sizes use `i64`; the compiler does not automatically track allocation lengths or ownership.

## Bulk memory operations

RamLang provides `memcpy(destination, source, byte_count)`, `memmove(destination, source, byte_count)`, `memset(destination, fill_value, byte_count)`, and `memcmp(left, right, byte_count)`. These lower to the corresponding C runtime functions; byte counts use `i64`, and pointer-like arguments may be raw pointers, strings, arrays, or struct-backed values.

```rust
fn main() -> i32 {
    let source: ptr = alloc(8);
    let destination: ptr = alloc(8);
    memset(source, 0, 8);
    store_i32(source, 42);
    memcpy(destination, source, 8);
    print(load_i32(destination));
    print(memcmp(source, destination, 8));
    memmove(destination, source, 4);
    free(source);
    free(destination);
    return 0;
}
```

`memcpy` requires non-overlapping regions; use `memmove` when the regions may overlap. All operations require valid pointers and sufficient accessible memory. `memcmp` returns a negative, zero, or positive result according to the first differing byte, not necessarily exactly `-1` or `1`.

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

## Relative source imports

Use `import "path/to/file.ram";` at file scope to include another RamLang source file in the same compilation. Paths are resolved relative to the importing file, so a nested file can import from its parent directory with `../`. Imported files may themselves import other files; the compiler canonicalizes paths to avoid parsing the same file twice and tolerates import cycles while collecting declarations.

```rust
// examples/imports/main.ram
import "math/add.ram";
import "constants.ram";

fn main() -> i32 {
    print(add(BASE, 2));
    return 0;
}
```

You can compile only the entry file:

```sh
ramcomp examples/imports/main.ram -o imports-demo
./imports-demo
```

Imports currently share one global symbol namespace. They are source inclusion, not qualified namespaces or package management; cyclic imports are deduplicated, but mutually dependent declarations still need to be valid after being combined into the single LLVM module.

## Multiple source files

Pass multiple `.ram` files to one compiler invocation. All functions from the inputs are combined into one LLVM module, so a function in one file can call a function declared in another input file. Explicit multi-file compilation remains supported alongside relative imports.

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

This remains an early systems-language compiler, but now includes modules via source imports, type aliases, enums and match expressions, named and packed structs, constants/statics, target layout queries (`size_of`, `align_of`, `offset_of`), inline LLVM assembly, bit-manipulation intrinsics, integer atomics, memory-order fences, volatile/unaligned access, and bulk memory functions. The CLI can emit LLVM IR, bitcode, object files, assembly, or a linked executable and accepts a target triple, CPU, feature flags, sysroot, and optimization level. C interoperability supports fixed-signature functions, variadic `extern fn` declarations with default promotions for extra scalar arguments, and external globals. These low-level facilities are unsafe by design: the programmer remains responsible for allocation size, alignment, bounds, lifetime, synchronization, target-specific address validity, format-string correctness, and avoiding use-after-free or double-free. Remaining limitations include typed-pointer safety, nested arrays, qualified namespaces, package management, generics, constructors/methods, passing aggregates through C variadic arguments, and a formal ownership model. Signed division uses LLVM `sdiv`; unsigned division uses `udiv`. The language has no stable specification yet, and some target triples/toolchains may need an appropriate sysroot and cross-linker.

The compiler emits opaque-pointer LLVM IR, so the current native path targets LLVM/Clang 15+. LLVM IR is an implementation boundary, not a promise that every future RamLang feature or target is already supported.

## License

GPL-3.0. See [LICENSE](LICENSE).
