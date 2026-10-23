//! Rust-owned LLVM backend for RamLang.
//!
//! The front-end still builds a textual LLVM IR module while it is being migrated.
//! This module parses and verifies that IR in-process, then asks LLVM's target machine
//! to emit object, assembly, or bitcode files. Clang is retained only as the platform
//! linker driver for executable outputs; it no longer parses RamLang's LLVM IR.

use std::path::Path;

use inkwell::{
    OptimizationLevel,
    context::Context,
    memory_buffer::MemoryBuffer,
    targets::{
        CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine,
        TargetTriple,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    Object,
    Assembly,
    Bitcode,
}

fn parse_module<'ctx>(context: &'ctx Context, ir: &str) -> Result<inkwell::module::Module<'ctx>, String> {
    let buffer = MemoryBuffer::create_from_memory_range(ir.as_bytes(), "ramlang-generated.ll");
    let module = context
        .create_module_from_ir(buffer)
        .map_err(|error| format!("LLVM could not parse generated IR: {error}"))?;
    module
        .verify()
        .map_err(|error| format!("LLVM verifier rejected generated IR: {error}"))?;
    Ok(module)
}

/// Verify LLVM IR using the LLVM library linked into this Rust compiler process.
pub fn verify_ir(ir: &str) -> Result<(), String> {
    let context = Context::create();
    let module = parse_module(&context, ir)?;
    module
        .verify()
        .map_err(|error| format!("LLVM verifier rejected generated IR: {error}"))
}

fn optimization_level(level: &str) -> OptimizationLevel {
    match level {
        "0" => OptimizationLevel::None,
        "1" => OptimizationLevel::Less,
        "3" => OptimizationLevel::Aggressive,
        "s" | "z" => OptimizationLevel::Default,
        _ => OptimizationLevel::Default,
    }
}

/// Emit a native target artifact directly through LLVM, without invoking Clang to compile IR.
pub fn emit_file(
    ir: &str,
    output: &Path,
    kind: OutputKind,
    target: Option<&str>,
    cpu: Option<&str>,
    features: &[String],
    opt_level: &str,
) -> Result<(), String> {
    let context = Context::create();
    let mut module = parse_module(&context, ir)?;

    if kind == OutputKind::Bitcode {
        module
            .write_bitcode_to_path(output)
            .then_some(())
            .ok_or_else(|| format!("LLVM could not write bitcode to {}", output.display()))?;
        return Ok(());
    }

    Target::initialize_all(&InitializationConfig::default());

    let triple = target.map_or_else(
        || TargetMachine::get_default_triple(),
        TargetTriple::create,
    );
    let target = Target::from_triple(&triple)
        .map_err(|error| format!("LLVM does not support target '{}': {error}", triple.as_str().to_string_lossy()))?;
    let feature_string = features.join(",");
    let target_machine = target
        .create_target_machine(
            &triple,
            cpu.unwrap_or("generic"),
            &feature_string,
            optimization_level(opt_level),
            RelocMode::PIC,
            CodeModel::Default,
        )
        .ok_or_else(|| {
            format!(
                "LLVM could not create a target machine for '{}'",
                triple.as_str().to_string_lossy()
            )
        })?;

    module.set_triple(&triple);
    let data_layout = target_machine.get_target_data().get_data_layout();
    module.set_data_layout(&data_layout);
    module
        .verify()
        .map_err(|error| format!("LLVM verifier rejected target-configured IR: {error}"))?;

    let file_type = match kind {
        OutputKind::Object => FileType::Object,
        OutputKind::Assembly => FileType::Assembly,
        OutputKind::Bitcode => unreachable!("bitcode is handled before target-machine setup"),
    };
    target_machine
        .write_to_file(&module, file_type, output)
        .map_err(|error| format!("LLVM failed to write {}: {error}", output.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_llvm_ir_without_external_processes() {
        verify_ir("define i32 @main() { entry: ret i32 0 }").unwrap();
    }

    #[test]
    fn rejects_invalid_llvm_ir_with_diagnostic() {
        let error = verify_ir("define i32 @main() { entry: ret void }").unwrap_err();
        assert!(
            error.contains("parse") || error.contains("verifier"),
            "unexpected diagnostic: {error}"
        );
    }
}
