use crate::ast::self;
use crate::compiler::compile;
use crate::errors::QplError;
use crate::lexer::tokenise;
use crate::parser::parse;
use crate::opcodes::disassemble_instructions;
use crate::vm::{Vm, run_vm, EvalResult};
use polars::prelude::*;
use rustyline::{DefaultEditor, error::ReadlineError};

/// Run a `.qpl` script file, printing results. Returns Err on the first failure.
pub fn run_script(path: &str, vm: &mut Vm) -> Result<(), QplError> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| QplError::Runtime(format!("cannot read '{path}': {e}")))?;
    for (lineno, raw) in src.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('/') {
            continue;
        }
        match_run_vm(line, vm, path, lineno)?;
    }
    Ok(())
}

pub fn start(vm: &mut Vm) {
    let mut rl = DefaultEditor::new().expect("failed to create line editor");

    println!("qpl (Quick Polars Query Language) REPL - \\d to disassemble");

    loop {
        match rl.readline("qpl) ") {
            Ok(line) => {
                let line = line.trim().to_string();
                if line.is_empty() || line.starts_with('/') {
                    continue;
                }
                let _ = rl.add_history_entry(&line);
                if let Some(src) = line.strip_prefix("\\d").map(str::trim) {
                    match disassemble(src) {
                        Ok(listing) => println!("{listing}"),
                        Err(e) => eprintln!("{e}"),
                    }
                    continue;
                }
                if let Err(e) = match_run_vm(&line, vm, "<main>", 0) {
                    eprintln!("{:?}", e)
                };
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => break,
            Err(e) => { eprintln!("readline error: {e}"); break; }
        }
    }
}

pub fn load_demo_tables(vm: &mut Vm) {
    let trades = df![
        "sym"   => ["AAPL","AAPL","MSFT","MSFT","GOOG","GOOG","AAPL","MSFT"],
        "price" => [182.3f64, 183.1, 415.2, 416.0, 140.5, 141.2, 184.0, 414.8],
        "size"  => [100i64, 250, 80, 300, 150, 90, 500, 200],
        "side"  => ["buy","sell","buy","buy","sell","buy","sell","sell"],
    ].expect("trades");

    let quotes = df![
        "sym"   => ["AAPL","MSFT","GOOG","AAPL","MSFT"],
        "bid"   => [182.0f64, 415.0, 140.3, 183.8, 414.5],
        "ask"   => [182.5f64, 415.5, 140.8, 184.2, 415.0],
        "bsize" => [500i64, 300, 200, 400, 600],
        "asize" => [400i64, 250, 150, 350, 500],
    ].expect("quotes");

    vm.tables.insert("trades".into(), trades);
    vm.tables.insert("quotes".into(), quotes);
}

fn match_run_vm(line: &str, vm: &mut Vm, path: &str, lineno: usize) -> Result<(), QplError> {
    match run_vm(line, vm) {
        Ok(EvalResult::Table(df))    => println!("{df}"),
        Ok(EvalResult::Stored)                  => {},
        Ok(EvalResult::Scalar(val))      => println!("{}", fmt_val(&val)),
        Err(e) => {
            return Err(QplError::Runtime(format!("{}:{}: {e}", path, lineno + 1)));
        }
    };
    Ok(())
}

fn fmt_val(v: &ast::Value) -> String {
    match v {
        ast::Value::Int(n)   => format!("i64: {n}"),
        ast::Value::Float(f) => format!("f64: {f}"),
        ast::Value::Str(s)   => format!("str: \"{s}\""),
        ast::Value::Bool(b)  => format!("bool: {b}"),
        other                => format!("{other:?}"),
    }
}

fn disassemble(source: &str) -> Result<String, QplError> {
    let tokens = tokenise(source)?;
    let stmt   = parse(tokens)?;
    let prog   = compile(&stmt)?;
    Ok(disassemble_instructions(&prog).join("\n"))
}

