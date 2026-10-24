//! Cranelift JIT backend for the backend-neutral Ramlang core.
//!
//! The first native subset supports integer and boolean values, calls, branches,
//! loops and numeric/boolean print. Strings and low-level facilities remain in the
//! interpreter-only subset until their native representation is specified.

use std::{cell::RefCell, mem};

use cranelift_codegen::ir::{
    self, types, AbiParam, Block, InstBuilder, IntCC, TrapCode, UserFuncName,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, FuncId, Linkage, Module};
use ramlang_core::{Function, Instruction, Program, Type, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitError(String);

impl std::fmt::Display for JitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for JitError {}

struct ExecutionState {
    remaining_fuel: u64,
    output: Vec<String>,
    error: Option<String>,
}

thread_local! {
    static EXECUTION: RefCell<Option<ExecutionState>> = const { RefCell::new(None) };
}

extern "C" fn runtime_guard() -> u8 {
    EXECUTION.with(|cell| {
        let mut slot = cell.borrow_mut();
        let Some(state) = slot.as_mut() else { return 0; };
        if state.error.is_some() { return 0; }
        if state.remaining_fuel == 0 {
            state.error = Some("native JIT instruction budget exhausted".to_owned());
            return 0;
        }
        state.remaining_fuel -= 1;
        1
    })
}

extern "C" fn runtime_fail(code: u8) {
    let message = match code {
        1 => "signed integer addition overflow",
        2 => "signed integer subtraction overflow",
        3 => "signed integer multiplication overflow",
        4 => "division by zero",
        5 => "signed integer division overflow",
        6 => "native JIT reached a missing instruction",
        _ => "native JIT runtime failure",
    };
    EXECUTION.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            if state.error.is_none() {
                state.error = Some(message.to_owned());
            }
        }
    });
}

extern "C" fn runtime_print_i64(value: i64) {
    EXECUTION.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.output.push(value.to_string());
        }
    });
}

extern "C" fn runtime_print_bool(value: u8) {
    EXECUTION.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.output.push((value != 0).to_string());
        }
    });
}

/// Compile and run a verified core program through Cranelift native code generation.
///
/// `fuel` is decremented for every core instruction, including loop branches and calls.
/// Checked arithmetic errors and fuel exhaustion return Rust errors rather than wrapping
/// or running unbounded. Executable memory stays owned by the JIT module while invoked.
pub fn run_capture(program: &Program, fuel: u64) -> Result<(Value, Vec<String>), JitError> {
    program.verify().map_err(|error| JitError(error.to_string()))?;
    let entry = program.functions.get(program.entry.0)
        .ok_or_else(|| JitError("entry function does not exist".to_owned()))?;
    if !entry.params.is_empty() {
        return Err(JitError("native JIT entry point must not take parameters".to_owned()));
    }
    if !matches!(entry.result, Type::I64 | Type::Unit) {
        return Err(JitError("native JIT main must return i64 or void".to_owned()));
    }

    for function in &program.functions {
        if function.result == Type::Str
            || function.params.contains(&Type::Str)
            || function.registers.contains(&Type::Str)
            || function.code.iter().any(|instruction| matches!(
                instruction,
                Instruction::ConstStr { .. } | Instruction::PrintStr { .. }
            ))
        {
            return Err(JitError(
                "native JIT does not yet support str values; use ramrun for this source subset".to_owned(),
            ));
        }
    }

    let mut jit_builder = JITBuilder::new(default_libcall_names())
        .map_err(|error| JitError(format!("cannot initialize Cranelift JIT: {error}")))?;
    jit_builder.symbol("ramlang_jit_guard", runtime_guard as *const u8);
    jit_builder.symbol("ramlang_jit_fail", runtime_fail as *const u8);
    jit_builder.symbol("ramlang_jit_print_i64", runtime_print_i64 as *const u8);
    jit_builder.symbol("ramlang_jit_print_bool", runtime_print_bool as *const u8);
    let mut module = JITModule::new(jit_builder);

    let mut signatures = Vec::with_capacity(program.functions.len());
    let mut function_ids = Vec::with_capacity(program.functions.len());
    for (index, function) in program.functions.iter().enumerate() {
        let mut signature = module.make_signature();
        for parameter in &function.params {
            signature.params.push(AbiParam::new(clif_type(*parameter)?));
        }
        if function.result != Type::Unit {
            signature.returns.push(AbiParam::new(clif_type(function.result)?));
        }
        let id = module.declare_function(
            &format!("ramlang_fn_{index}"),
            Linkage::Local,
            &signature,
        ).map_err(|error| JitError(format!("cannot declare '{}': {error}", function.name)))?;
        signatures.push(signature);
        function_ids.push(id);
    }

    let guard_id = declare_import(&mut module, "ramlang_jit_guard", &[], &[types::I8])?;
    let fail_id = declare_import(&mut module, "ramlang_jit_fail", &[types::I8], &[])?;
    let print_i64_id = declare_import(&mut module, "ramlang_jit_print_i64", &[types::I64], &[])?;
    let print_bool_id = declare_import(&mut module, "ramlang_jit_print_bool", &[types::I8], &[])?;

    let mut context = module.make_context();
    let mut function_context = FunctionBuilderContext::new();
    for (index, function) in program.functions.iter().enumerate() {
        context.func.signature = signatures[index].clone();
        context.func.name = UserFuncName::user(0, function_ids[index].as_u32());
        translate_function(
            &mut module,
            &mut context,
            &mut function_context,
            function,
            &function_ids,
            guard_id,
            fail_id,
            print_i64_id,
            print_bool_id,
        )?;
        module.define_function(function_ids[index], &mut context)
            .map_err(|error| JitError(format!("cannot define '{}': {error}", function.name)))?;
        module.clear_context(&mut context);
    }
    module.finalize_definitions()
        .map_err(|error| JitError(format!("cannot finalize native code: {error}")))?;

    let code = module.get_finalized_function(function_ids[program.entry.0]);
    EXECUTION.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_some() {
            return Err(JitError("native JIT execution is already active on this thread".to_owned()));
        }
        *slot = Some(ExecutionState { remaining_fuel: fuel, output: Vec::new(), error: None });
        Ok(())
    })?;

    let returned = if entry.result == Type::I64 {
        // SAFETY: verified entry signature is extern-C-compatible fn() -> i64 and the
        // pointer refers to live, finalized code owned by module until this function ends.
        let native = unsafe { mem::transmute::<*const u8, extern "C" fn() -> i64>(code) };
        Value::I64(native())
    } else {
        // SAFETY: verified entry signature is extern-C-compatible fn() -> void and the
        // pointer refers to live, finalized code owned by module until this function ends.
        let native = unsafe { mem::transmute::<*const u8, extern "C" fn()>(code) };
        native();
        Value::Unit
    };

    let state = EXECUTION.with(|cell| cell.borrow_mut().take())
        .ok_or_else(|| JitError("native JIT execution state was lost".to_owned()))?;
    if let Some(error) = state.error {
        return Err(JitError(error));
    }
    Ok((returned, state.output))
}

fn clif_type(ty: Type) -> Result<ir::Type, JitError> {
    match ty {
        Type::I64 => Ok(types::I64),
        Type::Bool | Type::Unit => Ok(types::I8),
        Type::Str => Err(JitError("native JIT does not yet support str values".to_owned())),
    }
}

fn declare_import(
    module: &mut JITModule,
    name: &str,
    parameters: &[ir::Type],
    returns: &[ir::Type],
) -> Result<FuncId, JitError> {
    let mut signature = module.make_signature();
    signature.params.extend(parameters.iter().copied().map(AbiParam::new));
    signature.returns.extend(returns.iter().copied().map(AbiParam::new));
    module.declare_function(name, Linkage::Import, &signature)
        .map_err(|error| JitError(format!("cannot declare runtime symbol '{name}': {error}")))
}

#[allow(clippy::too_many_arguments)]
fn translate_function(
    module: &mut JITModule,
    context: &mut cranelift_codegen::Context,
    function_context: &mut FunctionBuilderContext,
    function: &Function,
    function_ids: &[FuncId],
    guard_id: FuncId,
    fail_id: FuncId,
    print_i64_id: FuncId,
    print_bool_id: FuncId,
) -> Result<(), JitError> {
    if function.code.is_empty() {
        return Err(JitError(format!("function '{}' has no instructions", function.name)));
    }

    let mut builder = FunctionBuilder::new(&mut context.func, function_context);
    let entry_block = builder.create_block();
    let guards: Vec<Block> = function.code.iter().map(|_| builder.create_block()).collect();
    let bodies: Vec<Block> = function.code.iter().map(|_| builder.create_block()).collect();
    let fallback = builder.create_block();

    let mut variables = Vec::with_capacity(function.registers.len());
    for (index, ty) in function.registers.iter().enumerate() {
        let variable = Variable::from_u32(index as u32);
        // Unit-typed registers are unused expression placeholders in this subset.
        builder.declare_var(variable, clif_type(*ty)?);
        variables.push(variable);
    }

    builder.switch_to_block(entry_block);
    builder.append_block_params_for_function_params(entry_block);
    let parameters = builder.block_params(entry_block).to_vec();
    if parameters.len() != function.params.len() {
        return Err(JitError(format!("function '{}' signature/parameter mismatch", function.name)));
    }
    for (index, value) in parameters.into_iter().enumerate() {
        builder.def_var(variables[index], value);
    }
    builder.ins().jump(guards[0], &[]);

    for (pc, instruction) in function.code.iter().enumerate() {
        builder.switch_to_block(guards[pc]);
        let guard_ref = module.declare_func_in_func(guard_id, &mut builder.func);
        let guard_call = builder.ins().call(guard_ref, &[]);
        let guard_result = builder.inst_results(guard_call)[0];
        let guard_condition = builder.ins().icmp_imm(IntCC::NotEqual, guard_result, 0);
        builder.ins().brif(guard_condition, bodies[pc], &[], fallback, &[]);

        builder.switch_to_block(bodies[pc]);
        match instruction {
            Instruction::ConstI64 { dst, value } => {
                let value = builder.ins().iconst(types::I64, *value);
                builder.def_var(variables[dst.0], value);
                jump_next(&mut builder, pc, &guards, fallback)?;
            }
            Instruction::ConstBool { dst, value } => {
                let value = builder.ins().iconst(types::I8, i64::from(*value));
                builder.def_var(variables[dst.0], value);
                jump_next(&mut builder, pc, &guards, fallback)?;
            }
            Instruction::ConstStr { .. } | Instruction::PrintStr { .. } => {
                return Err(JitError("native JIT does not yet support str values".to_owned()));
            }
            Instruction::Copy { dst, src } => {
                let value = builder.use_var(variables[src.0]);
                builder.def_var(variables[dst.0], value);
                jump_next(&mut builder, pc, &guards, fallback)?;
            }
            Instruction::AddI64 { dst, lhs, rhs } => {
                let left = builder.use_var(variables[lhs.0]);
                let right = builder.use_var(variables[rhs.0]);
                let (value, overflow) = builder.ins().sadd_overflow(left, right);
                emit_checked_result(module, &mut builder, variables[dst.0], value, overflow, 1, fail_id, fallback, &guards, pc)?;
            }
            Instruction::SubI64 { dst, lhs, rhs } => {
                let left = builder.use_var(variables[lhs.0]);
                let right = builder.use_var(variables[rhs.0]);
                let (value, overflow) = builder.ins().ssub_overflow(left, right);
                emit_checked_result(module, &mut builder, variables[dst.0], value, overflow, 2, fail_id, fallback, &guards, pc)?;
            }
            Instruction::MulI64 { dst, lhs, rhs } => {
                let left = builder.use_var(variables[lhs.0]);
                let right = builder.use_var(variables[rhs.0]);
                let (value, overflow) = builder.ins().smul_overflow(left, right);
                emit_checked_result(module, &mut builder, variables[dst.0], value, overflow, 3, fail_id, fallback, &guards, pc)?;
            }
            Instruction::DivI64 { dst, lhs, rhs } => {
                let left = builder.use_var(variables[lhs.0]);
                let right = builder.use_var(variables[rhs.0]);
                let zero = builder.ins().icmp_imm(IntCC::Equal, right, 0);
                let zero_block = builder.create_block();
                let check_overflow = builder.create_block();
                let overflow_block = builder.create_block();
                let good_block = builder.create_block();
                builder.ins().brif(zero, zero_block, &[], check_overflow, &[]);

                builder.switch_to_block(zero_block);
                emit_fail(&mut builder, module, fail_id, fallback, 4);

                builder.switch_to_block(check_overflow);
                let is_min = builder.ins().icmp_imm(IntCC::Equal, left, i64::MIN);
                let is_negative_one = builder.ins().icmp_imm(IntCC::Equal, right, -1);
                let signed_overflow = builder.ins().band(is_min, is_negative_one);
                builder.ins().brif(signed_overflow, overflow_block, &[], good_block, &[]);

                builder.switch_to_block(overflow_block);
                emit_fail(&mut builder, module, fail_id, fallback, 5);

                builder.switch_to_block(good_block);
                let value = builder.ins().sdiv(left, right);
                builder.def_var(variables[dst.0], value);
                jump_next(&mut builder, pc, &guards, fallback)?;
            }
            Instruction::EqI64 { dst, lhs, rhs } => {
                let left = builder.use_var(variables[lhs.0]);
                let right = builder.use_var(variables[rhs.0]);
                let condition = builder.ins().icmp(IntCC::Equal, left, right);
                let value = bool_to_i8(&mut builder, condition);
                builder.def_var(variables[dst.0], value);
                jump_next(&mut builder, pc, &guards, fallback)?;
            }
            Instruction::LtI64 { dst, lhs, rhs } => {
                let left = builder.use_var(variables[lhs.0]);
                let right = builder.use_var(variables[rhs.0]);
                let condition = builder.ins().icmp(IntCC::SignedLessThan, left, right);
                let value = bool_to_i8(&mut builder, condition);
                builder.def_var(variables[dst.0], value);
                jump_next(&mut builder, pc, &guards, fallback)?;
            }
            Instruction::NotBool { dst, src } => {
                let source = builder.use_var(variables[src.0]);
                let condition = builder.ins().icmp_imm(IntCC::Equal, source, 0);
                let value = bool_to_i8(&mut builder, condition);
                builder.def_var(variables[dst.0], value);
                jump_next(&mut builder, pc, &guards, fallback)?;
            }
            Instruction::PrintI64 { src } => {
                let value = builder.use_var(variables[src.0]);
                let callee = module.declare_func_in_func(print_i64_id, &mut builder.func);
                builder.ins().call(callee, &[value]);
                jump_next(&mut builder, pc, &guards, fallback)?;
            }
            Instruction::PrintBool { src } => {
                let value = builder.use_var(variables[src.0]);
                let callee = module.declare_func_in_func(print_bool_id, &mut builder.func);
                builder.ins().call(callee, &[value]);
                jump_next(&mut builder, pc, &guards, fallback)?;
            }
            Instruction::Jump { target } => {
                let target = guards.get(*target)
                    .ok_or_else(|| JitError(format!("branch target {target} is out of range")))?;
                builder.ins().jump(*target, &[]);
            }
            Instruction::JumpIfFalse { condition, target } => {
                let source = builder.use_var(variables[condition.0]);
                let condition_value = builder.ins().icmp_imm(IntCC::NotEqual, source, 0);
                let next = guards.get(pc + 1)
                    .ok_or_else(|| JitError(format!("function '{}' falls off its end", function.name)))?;
                let target = guards.get(*target)
                    .ok_or_else(|| JitError(format!("branch target {target} is out of range")))?;
                builder.ins().brif(condition_value, *next, &[], *target, &[]);
            }
            Instruction::Call { dst, function: target, args } => {
                let callee_id = *function_ids.get(target.0)
                    .ok_or_else(|| JitError(format!("call target {} does not exist", target.0)))?;
                let callee = module.declare_func_in_func(callee_id, &mut builder.func);
                let arguments = args.iter().map(|register| builder.use_var(variables[register.0])).collect::<Vec<_>>();
                let call = builder.ins().call(callee, &arguments);
                if let Some(register) = dst {
                    let result = builder.inst_results(call).first().copied()
                        .ok_or_else(|| JitError("non-unit call had no return value".to_owned()))?;
                    builder.def_var(variables[register.0], result);
                }
                jump_next(&mut builder, pc, &guards, fallback)?;
            }
            Instruction::Return { value } => {
                if let Some(register) = value {
                    let value = builder.use_var(variables[register.0]);
                    builder.ins().return_(&[value]);
                } else {
                    builder.ins().return_(&[]);
                }
            }
        }
    }

    builder.switch_to_block(fallback);
    match function.result {
        Type::I64 => {
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().return_(&[zero]);
        }
        Type::Bool => {
            let zero = builder.ins().iconst(types::I8, 0);
            builder.ins().return_(&[zero]);
        }
        Type::Unit => { builder.ins().return_(&[]); }
        Type::Str => return Err(JitError("native JIT does not yet support str values".to_owned())),
    }
    builder.seal_all_blocks();
    builder.finalize(module.target_config());
    Ok(())
}

fn bool_to_i8(builder: &mut FunctionBuilder<'_>, condition: ir::Value) -> ir::Value {
    let one = builder.ins().iconst(types::I8, 1);
    let zero = builder.ins().iconst(types::I8, 0);
    builder.ins().select(condition, one, zero)
}

fn jump_next(
    builder: &mut FunctionBuilder<'_>,
    pc: usize,
    guards: &[Block],
    fallback: Block,
) -> Result<(), JitError> {
    if let Some(next) = guards.get(pc + 1) {
        builder.ins().jump(*next, &[]);
    } else {
        builder.ins().jump(fallback, &[]);
    }
    Ok(())
}

fn emit_fail(
    builder: &mut FunctionBuilder<'_>,
    module: &mut JITModule,
    fail_id: FuncId,
    fallback: Block,
    code: i64,
) {
    let callee = module.declare_func_in_func(fail_id, &mut builder.func);
    let code_value = builder.ins().iconst(types::I8, code);
    builder.ins().call(callee, &[code_value]);
    builder.ins().jump(fallback, &[]);
}

fn emit_checked_result(
    module: &mut JITModule,
    builder: &mut FunctionBuilder<'_>,
    destination: Variable,
    value: ir::Value,
    overflow: ir::Value,
    error_code: i64,
    fail_id: FuncId,
    fallback: Block,
    guards: &[Block],
    pc: usize,
) -> Result<(), JitError> {
    let error_block = builder.create_block();
    let success_block = builder.create_block();
    builder.ins().brif(overflow, error_block, &[], success_block, &[]);
    builder.switch_to_block(error_block);
    emit_fail(builder, module, fail_id, fallback, error_code);
    builder.switch_to_block(success_block);
    builder.def_var(destination, value);
    jump_next(builder, pc, guards, fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ramlang_core::compile_source;

    #[test]
    fn native_code_matches_reference_for_loops_calls_branches_and_print() {
        let source = r#"
            fn double(x: i32) -> i32 { return x * 2; }
            fn main() -> i32 {
                let mut i: i32 = 0;
                let mut sum: i32 = 0;
                while (i < 5) {
                    sum = sum + i;
                    i = i + 1;
                }
                if (sum == 10 && double(1) == 2) { print(sum); } else { print(0); }
                return 0;
            }
        "#;
        let program = compile_source(source).unwrap();
        let reference = program.run_capture(&[], 10_000).unwrap();
        let native = run_capture(&program, 10_000).unwrap();
        assert_eq!(native, reference);
    }

    #[test]
    fn native_jit_reports_checked_arithmetic_errors() {
        let program = compile_source("fn main() -> i32 { let x: i64 = 9223372036854775807; print(x + 1); return 0; }").unwrap();
        assert!(run_capture(&program, 100).unwrap_err().to_string().contains("addition overflow"));
        let division = compile_source("fn main() -> i32 { let x: i32 = 1; print(x / 0); return 0; }").unwrap();
        assert!(run_capture(&division, 100).unwrap_err().to_string().contains("division by zero"));
    }

    #[test]
    fn native_jit_enforces_a_runtime_instruction_budget() {
        let program = compile_source("fn main() -> i32 { while (true) { } return 0; }").unwrap();
        assert!(run_capture(&program, 20).unwrap_err().to_string().contains("instruction budget"));
    }
}
