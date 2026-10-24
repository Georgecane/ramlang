use std::{
    env,
    error::Error,
    fs,
    io::{self, Write},
    process,
};

use ramlang_core::{compile_source, Value};

const DEFAULT_FUEL: u64 = 1_000_000;

fn main() {
    match run() {
        Ok(status) => process::exit(status),
        Err(error) => {
            eprintln!("ramrun: {error}");
            process::exit(1);
        }
    }
}

fn run() -> Result<i32, Box<dyn Error>> {
    let mut arguments = env::args().skip(1);
    let path = arguments.next().ok_or(
        "usage: ramrun <source.ram> [--fuel <instruction-budget>]",
    )?;
    let mut fuel = DEFAULT_FUEL;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--fuel" => {
                let value = arguments.next().ok_or("--fuel requires an integer")?;
                fuel = value.parse().map_err(|_| "--fuel must be an unsigned integer")?;
            }
            "--help" | "-h" => {
                println!("Usage: ramrun <source.ram> [--fuel <instruction-budget>]");
                println!("Runs the dependency-free Ramlang reference-interpreter subset.");
                return Ok(0);
            }
            other => return Err(format!("unknown argument '{other}'").into()),
        }
    }

    let source = fs::read_to_string(&path)?;
    let program = compile_source(&source)?;
    let (value, lines) = program.run_capture(&[], fuel)?;

    let stdout = io::stdout();
    let mut output = stdout.lock();
    for line in lines {
        writeln!(output, "{line}")?;
    }
    output.flush()?;

    match value {
        Value::I64(status) => Ok(status as i32),
        Value::Unit => Ok(0),
        Value::Bool(_) => Err("main returned bool; expected an integer or void".into()),
        Value::Str(_) => Err("main returned str; expected an integer or void".into()),
    }
}
