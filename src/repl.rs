use crate::compiler::Compiler;
use crate::errors::QplError;
use crate::lexer::tokenise;
use crate::parser::Parser;
use crate::vm::{Value, Vm};
use rustyline::DefaultEditor;

pub fn start() {
    let mut rl = DefaultEditor::new().expect("failed to create line editor");
    let mut vm = Vm::new();

    loop {
        match rl.readline("qpl> ") {
            Ok(line) => {
                let _ = rl.add_history_entry(&line);
                match eval(&line, &mut vm) {
                    Ok(Value::Nil) => {}
                    Ok(val) => println!("{val}"),
                    Err(e) => eprintln!("{e}"),
                }
            }
            Err(_) => break,
        }
    }
}

fn eval(source: &str, vm: &mut Vm) -> Result<Value, QplError> {
    let tokens = tokenise(source)?;
    // let program = Parser::new(tokens).parse()?;
    // let bytecode = Compiler::new().compile(program)?;
    // vm.run(bytecode)
    Ok(Value::Nil)
}
