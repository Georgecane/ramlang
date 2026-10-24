//! Dependency-free reference IR and interpreter for Ramlang.
//!
//! This crate is intentionally backend-neutral. It is the first extraction point for
//! the semantic core: the legacy LLVM front end remains unchanged until callers have
//! migrated to this verifier and execution model.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Register(pub usize);
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionId(pub usize);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type { I64, Bool, Unit }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value { I64(i64), Bool(bool), Unit }

impl Value {
    pub const fn ty(self) -> Type {
        match self { Self::I64(_) => Type::I64, Self::Bool(_) => Type::Bool, Self::Unit => Type::Unit }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    ConstI64 { dst: Register, value: i64 },
    ConstBool { dst: Register, value: bool },
    Copy { dst: Register, src: Register },
    AddI64 { dst: Register, lhs: Register, rhs: Register },
    SubI64 { dst: Register, lhs: Register, rhs: Register },
    MulI64 { dst: Register, lhs: Register, rhs: Register },
    DivI64 { dst: Register, lhs: Register, rhs: Register },
    EqI64 { dst: Register, lhs: Register, rhs: Register },
    LtI64 { dst: Register, lhs: Register, rhs: Register },
    NotBool { dst: Register, src: Register },
    PrintI64 { src: Register },
    PrintBool { src: Register },
    Jump { target: usize },
    JumpIfFalse { condition: Register, target: usize },
    Call { dst: Option<Register>, function: FunctionId, args: Vec<Register> },
    Return { value: Option<Register> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    pub name: String,
    /// The first params.len() registers hold parameters on entry.
    pub params: Vec<Type>,
    pub result: Type,
    pub registers: Vec<Type>,
    pub code: Vec<Instruction>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program { pub functions: Vec<Function>, pub entry: FunctionId }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmError(String);
impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}
impl std::error::Error for VmError {}

impl VmError {
    pub(crate) fn new(message: impl Into<String>) -> Self { Self(message.into()) }
}

impl Program {
    /// Verify register references, instruction types, calls and branch targets.
    pub fn verify(&self) -> Result<(), VmError> {
        if self.functions.get(self.entry.0).is_none() {
            return Err(VmError(format!("entry function id {} does not exist", self.entry.0)));
        }
        for (function_index, function) in self.functions.iter().enumerate() {
            if function.code.is_empty() {
                return Err(VmError(format!("function '{}' has no instructions", function.name)));
            }
            if function.registers.len() < function.params.len() {
                return Err(VmError(format!("function '{}' has fewer registers than parameters", function.name)));
            }
            for (index, parameter_type) in function.params.iter().enumerate() {
                if function.registers[index] != *parameter_type {
                    return Err(VmError(format!(
                        "function '{}' parameter {index} expects {parameter_type:?}, but register type is {:?}",
                        function.name, function.registers[index]
                    )));
                }
            }
            for (pc, instruction) in function.code.iter().enumerate() {
                let fail = |message: String| VmError(format!("{} at {}:{pc}: {message}", function.name, function_index));
                match instruction {
                    Instruction::ConstI64 { dst, .. } => check_register(function, *dst, Type::I64).map_err(fail)?,
                    Instruction::ConstBool { dst, .. } => check_register(function, *dst, Type::Bool).map_err(fail)?,
                    Instruction::Copy { dst, src } => {
                        let source_type = register_type(function, *src).map_err(&fail)?;
                        check_register(function, *dst, source_type).map_err(fail)?;
                    }
                    Instruction::AddI64 { dst, lhs, rhs }
                    | Instruction::SubI64 { dst, lhs, rhs }
                    | Instruction::MulI64 { dst, lhs, rhs }
                    | Instruction::DivI64 { dst, lhs, rhs } => {
                        check_register(function, *dst, Type::I64).map_err(&fail)?;
                        check_register(function, *lhs, Type::I64).map_err(&fail)?;
                        check_register(function, *rhs, Type::I64).map_err(fail)?;
                    }
                    Instruction::EqI64 { dst, lhs, rhs } | Instruction::LtI64 { dst, lhs, rhs } => {
                        check_register(function, *dst, Type::Bool).map_err(&fail)?;
                        check_register(function, *lhs, Type::I64).map_err(&fail)?;
                        check_register(function, *rhs, Type::I64).map_err(fail)?;
                    }
                    Instruction::NotBool { dst, src } => {
                        check_register(function, *dst, Type::Bool).map_err(&fail)?;
                        check_register(function, *src, Type::Bool).map_err(fail)?;
                    }
                    Instruction::PrintI64 { src } => check_register(function, *src, Type::I64).map_err(&fail)?,
                    Instruction::PrintBool { src } => check_register(function, *src, Type::Bool).map_err(fail)?,
                    Instruction::Jump { target } => check_target(function, *target).map_err(fail)?,
                    Instruction::JumpIfFalse { condition, target } => {
                        check_register(function, *condition, Type::Bool).map_err(&fail)?;
                        check_target(function, *target).map_err(fail)?;
                    }
                    Instruction::Call { dst, function: target_id, args } => {
                        let target = self.functions.get(target_id.0)
                            .ok_or_else(|| fail(format!("call references missing function id {}", target_id.0)))?;
                        if args.len() != target.params.len() {
                            return Err(fail(format!("call to '{}' has {} arguments, expected {}", target.name, args.len(), target.params.len())));
                        }
                        for (argument, expected) in args.iter().zip(&target.params) {
                            check_register(function, *argument, *expected).map_err(&fail)?;
                        }
                        match (*dst, target.result) {
                            (None, Type::Unit) => {}
                            (Some(register), Type::I64) => check_register(function, register, Type::I64).map_err(&fail)?,
                            (Some(register), Type::Bool) => check_register(function, register, Type::Bool).map_err(&fail)?,
                            (None, result) => return Err(fail(format!("non-unit call to '{}' discards its {result:?} result", target.name))),
                            (Some(_), Type::Unit) => return Err(fail(format!("unit call to '{}' has a destination register", target.name))),
                        }
                    }
                    Instruction::Return { value } => match (*value, function.result) {
                        (None, Type::Unit) => {}
                        (Some(register), Type::I64) => check_register(function, register, Type::I64).map_err(&fail)?,
                        (Some(register), Type::Bool) => check_register(function, register, Type::Bool).map_err(&fail)?,
                        (None, _) => return Err(fail("return value is missing".to_owned())),
                        (Some(_), Type::Unit) => return Err(fail("unit function cannot return a value".to_owned())),
                    },
                }
            }
        }
        Ok(())
    }

    /// Run the entry function with an instruction budget shared across all calls.
    /// Arithmetic is checked; overflow and invalid division produce VM errors.
    pub fn run(&self, args: &[Value], fuel: u64) -> Result<Value, VmError> {
        self.run_capture(args, fuel).map(|(value, _)| value)
    }

    /// Run a program and capture lines emitted by the print instructions.
    pub fn run_capture(&self, args: &[Value], fuel: u64) -> Result<(Value, Vec<String>), VmError> {
        self.verify()?;
        let entry = self.functions.get(self.entry.0).expect("entry was verified");
        if args.len() != entry.params.len() {
            return Err(VmError(format!("entry '{}' expects {} arguments, received {}", entry.name, entry.params.len(), args.len())));
        }
        for (index, (argument, expected)) in args.iter().zip(&entry.params).enumerate() {
            if argument.ty() != *expected {
                return Err(VmError(format!("entry argument {index} has type {:?}, expected {expected:?}", argument.ty())));
            }
        }
        let mut remaining_fuel = fuel;
        let mut output = Vec::new();
        let value = self.invoke(self.entry, args, 0, &mut remaining_fuel, &mut output)?;
        Ok((value, output))
    }

    fn invoke(&self, function_id: FunctionId, args: &[Value], depth: usize, fuel: &mut u64, output: &mut Vec<String>) -> Result<Value, VmError> {
        const MAX_CALL_DEPTH: usize = 1024;
        if depth >= MAX_CALL_DEPTH { return Err(VmError("maximum VM call depth exceeded".to_owned())); }
        let function = self.functions.get(function_id.0)
            .ok_or_else(|| VmError(format!("function id {} does not exist", function_id.0)))?;
        let mut registers = vec![None; function.registers.len()];
        for (index, argument) in args.iter().copied().enumerate() { registers[index] = Some(argument); }
        let mut pc = 0usize;
        loop {
            if *fuel == 0 { return Err(VmError("instruction budget exhausted".to_owned())); }
            *fuel -= 1;
            let instruction = function.code.get(pc)
                .ok_or_else(|| VmError(format!("function '{}' fell off the end without returning", function.name)))?;
            pc += 1;
            match instruction {
                Instruction::ConstI64 { dst, value } => registers[dst.0] = Some(Value::I64(*value)),
                Instruction::ConstBool { dst, value } => registers[dst.0] = Some(Value::Bool(*value)),
                Instruction::Copy { dst, src } => registers[dst.0] = registers[src.0],
                Instruction::AddI64 { dst, lhs, rhs } => {
                    let (a, b) = (read_i64(&registers, *lhs)?, read_i64(&registers, *rhs)?);
                    registers[dst.0] = Some(Value::I64(a.checked_add(b).ok_or_else(|| arithmetic("addition overflow"))?));
                }
                Instruction::SubI64 { dst, lhs, rhs } => {
                    let (a, b) = (read_i64(&registers, *lhs)?, read_i64(&registers, *rhs)?);
                    registers[dst.0] = Some(Value::I64(a.checked_sub(b).ok_or_else(|| arithmetic("subtraction overflow"))?));
                }
                Instruction::MulI64 { dst, lhs, rhs } => {
                    let (a, b) = (read_i64(&registers, *lhs)?, read_i64(&registers, *rhs)?);
                    registers[dst.0] = Some(Value::I64(a.checked_mul(b).ok_or_else(|| arithmetic("multiplication overflow"))?));
                }
                Instruction::DivI64 { dst, lhs, rhs } => {
                    let (a, b) = (read_i64(&registers, *lhs)?, read_i64(&registers, *rhs)?);
                    if b == 0 { return Err(arithmetic("division by zero")); }
                    registers[dst.0] = Some(Value::I64(a.checked_div(b).ok_or_else(|| arithmetic("division overflow"))?));
                }
                Instruction::EqI64 { dst, lhs, rhs } => {
                    registers[dst.0] = Some(Value::Bool(read_i64(&registers, *lhs)? == read_i64(&registers, *rhs)?));
                }
                Instruction::LtI64 { dst, lhs, rhs } => {
                    registers[dst.0] = Some(Value::Bool(read_i64(&registers, *lhs)? < read_i64(&registers, *rhs)?));
                }
                Instruction::NotBool { dst, src } => registers[dst.0] = Some(Value::Bool(!read_bool(&registers, *src)?)),
                Instruction::PrintI64 { src } => output.push(read_i64(&registers, *src)?.to_string()),
                Instruction::PrintBool { src } => output.push(read_bool(&registers, *src)?.to_string()),
                Instruction::Jump { target } => pc = *target,
                Instruction::JumpIfFalse { condition, target } => if !read_bool(&registers, *condition)? { pc = *target; },
                Instruction::Call { dst, function: target, args } => {
                    let values = args.iter().map(|register| read_value(&registers, *register)).collect::<Result<Vec<_>, _>>()?;
                    let result = self.invoke(*target, &values, depth + 1, fuel, output)?;
                    if let Some(register) = dst { registers[register.0] = Some(result); }
                }
                Instruction::Return { value } => return match value {
                    Some(register) => read_value(&registers, *register),
                    None => Ok(Value::Unit),
                },
            }
        }
    }
}

fn arithmetic(message: &str) -> VmError { VmError(message.to_owned()) }
fn check_target(function: &Function, target: usize) -> Result<(), String> {
    if target < function.code.len() { Ok(()) } else { Err(format!("branch target {target} is out of range")) }
}
fn register_type(function: &Function, register: Register) -> Result<Type, String> {
    function.registers.get(register.0).copied().ok_or_else(|| format!("register r{} is out of range", register.0))
}
fn check_register(function: &Function, register: Register, expected: Type) -> Result<(), String> {
    let actual = register_type(function, register)?;
    if actual != expected { Err(format!("register r{} has type {actual:?}, expected {expected:?}", register.0)) } else { Ok(()) }
}
fn read_value(registers: &[Option<Value>], register: Register) -> Result<Value, VmError> {
    registers.get(register.0).copied().flatten().ok_or_else(|| VmError(format!("register r{} was read before initialization", register.0)))
}
fn read_i64(registers: &[Option<Value>], register: Register) -> Result<i64, VmError> {
    match read_value(registers, register)? { Value::I64(value) => Ok(value), other => Err(VmError(format!("expected i64 register, got {:?}", other.ty()))) }
}
fn read_bool(registers: &[Option<Value>], register: Register) -> Result<bool, VmError> {
    match read_value(registers, register)? { Value::Bool(value) => Ok(value), other => Err(VmError(format!("expected bool register, got {:?}", other.ty()))) }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn r(index: usize) -> Register { Register(index) }
    fn program(functions: Vec<Function>, entry: usize) -> Program { Program { functions, entry: FunctionId(entry) } }

    #[test]
    fn verifies_and_runs_checked_arithmetic() {
        let main = Function {
            name: "main".into(), params: vec![], result: Type::I64,
            registers: vec![Type::I64, Type::I64, Type::I64],
            code: vec![
                Instruction::ConstI64 { dst: r(0), value: 20 },
                Instruction::ConstI64 { dst: r(1), value: 22 },
                Instruction::AddI64 { dst: r(2), lhs: r(0), rhs: r(1) },
                Instruction::Return { value: Some(r(2)) },
            ],
        };
        assert_eq!(program(vec![main], 0).run(&[], 20), Ok(Value::I64(42)));
    }

    #[test]
    fn branches_and_calls_are_supported() {
        let double = Function {
            name: "double".into(), params: vec![Type::I64], result: Type::I64,
            registers: vec![Type::I64, Type::I64],
            code: vec![
                Instruction::ConstI64 { dst: r(1), value: 2 },
                Instruction::MulI64 { dst: r(1), lhs: r(0), rhs: r(1) },
                Instruction::Return { value: Some(r(1)) },
            ],
        };
        let main = Function {
            name: "main".into(), params: vec![], result: Type::I64,
            registers: vec![Type::I64, Type::I64, Type::Bool],
            code: vec![
                Instruction::ConstI64 { dst: r(0), value: 21 },
                Instruction::Call { dst: Some(r(1)), function: FunctionId(0), args: vec![r(0)] },
                Instruction::ConstI64 { dst: r(0), value: 42 },
                Instruction::EqI64 { dst: r(2), lhs: r(1), rhs: r(0) },
                Instruction::JumpIfFalse { condition: r(2), target: 6 },
                Instruction::Return { value: Some(r(1)) },
                Instruction::ConstI64 { dst: r(0), value: 0 },
                Instruction::Return { value: Some(r(0)) },
            ],
        };
        assert_eq!(program(vec![double, main], 1).run(&[], 50), Ok(Value::I64(42)));
    }

    #[test]
    fn rejects_invalid_register_types_and_branches() {
        let function = Function {
            name: "bad".into(), params: vec![], result: Type::I64, registers: vec![Type::Bool],
            code: vec![
                Instruction::ConstBool { dst: r(0), value: true },
                Instruction::Jump { target: 99 },
            ],
        };
        assert!(program(vec![function], 0).verify().unwrap_err().to_string().contains("branch target"));
    }

    #[test]
    fn fails_closed_on_overflow_division_by_zero_and_fuel_exhaustion() {
        let overflow = Function {
            name: "overflow".into(), params: vec![], result: Type::I64,
            registers: vec![Type::I64, Type::I64, Type::I64],
            code: vec![
                Instruction::ConstI64 { dst: r(0), value: i64::MAX },
                Instruction::ConstI64 { dst: r(1), value: 1 },
                Instruction::AddI64 { dst: r(2), lhs: r(0), rhs: r(1) },
                Instruction::Return { value: Some(r(2)) },
            ],
        };
        assert!(program(vec![overflow], 0).run(&[], 20).unwrap_err().to_string().contains("overflow"));
        let loop_forever = Function { name: "loop".into(), params: vec![], result: Type::Unit, registers: vec![], code: vec![Instruction::Jump { target: 0 }] };
        assert!(program(vec![loop_forever], 0).run(&[], 5).unwrap_err().to_string().contains("budget"));
    }

    #[test]
    fn rejects_call_signature_mismatch() {
        let callee = Function {
            name: "f".into(), params: vec![Type::I64], result: Type::I64,
            registers: vec![Type::I64], code: vec![Instruction::Return { value: Some(r(0)) }],
        };
        let caller = Function {
            name: "main".into(), params: vec![], result: Type::I64,
            registers: vec![Type::Bool],
            code: vec![
                Instruction::ConstBool { dst: r(0), value: true },
                Instruction::Call { dst: Some(r(0)), function: FunctionId(0), args: vec![r(0)] },
                Instruction::Return { value: Some(r(0)) },
            ],
        };
        assert!(program(vec![callee, caller], 1).verify().unwrap_err().to_string().contains("expected I64"));
    }
}
