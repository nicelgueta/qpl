use crate::ast::Stmt;
use crate::compiler::compile;
use crate::errors::QplError;
use crate::lexer::tokenise;
use crate::opcodes::disassemble_instructions;
use crate::parser::parse;
use crate::vm::Vm;
use polars::prelude::*;
use rustyline::{DefaultEditor, error::ReadlineError};

pub fn start() {
    let mut rl = DefaultEditor::new().expect("failed to create line editor");
    let mut vm = Vm::new();
    load_demo_tables(&mut vm);

    println!("qpl  –  polars made easy");
    println!("");

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
                match eval(&line, &mut vm) {
                    Ok(EvalResult::Table(df)) => println!("{df}"),
                    Ok(EvalResult::Stored(name)) => println!("`{name}"),
                    Err(e) => eprintln!("{e}"),
                }
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => break,
            Err(e) => { eprintln!("readline error: {e}"); break; }
        }
    }
}

fn load_demo_tables(vm: &mut Vm) {
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

enum EvalResult {
    Table(DataFrame),
    Stored(String),
}

fn disassemble(source: &str) -> Result<String, QplError> {
    let tokens = tokenise(source)?;
    let stmt   = parse(tokens)?;
    let prog   = compile(&stmt)?;
    Ok(disassemble_instructions(&prog).join("\n"))
}

fn eval(source: &str, vm: &mut Vm) -> Result<EvalResult, QplError> {
    let tokens  = tokenise(source)?;
    let stmt    = parse(tokens)?;
    let program = compile(&stmt)?;
    let df      = vm.eval(program)?;

    Ok(match &stmt {
        Stmt::Assign { name, .. } => {
            vm.tables.insert(name.clone(), df);
            EvalResult::Stored(name.clone())
        }
        _ => EvalResult::Table(df),
    })
}

