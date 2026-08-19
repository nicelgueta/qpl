mod ast;
mod builtins;
mod compiler;
mod errors;
mod lexer;
mod opcodes;
mod parser;
mod enums;
pub mod repl;
mod tokens;
mod vm;

use clap::{ArgAction::SetTrue, Parser};

#[derive(Parser)]
#[command(name = "qpl", about = "Quick Polars Query Language", version)]
struct Cli {
    /// Script file to execute (.qpl)
    file: Option<String>,

    /// Run script then drop into the REPL
    #[arg(short = 'i', requires = "file")]
    interactive: bool,

    #[arg(long = "load-demo", action = SetTrue)]
    load_demo: bool

}

fn main() {
    let cli = Cli::parse();
    let mut vm = vm::Vm::new();
    
    if cli.load_demo {
        println!("Loading demo tables `trades` and `quotes`");
        repl::load_demo_tables(&mut vm)
    };
    match cli.file {
        None => repl::start(&mut vm),
        Some(ref path) => {
            if let Err(e) = repl::run_script(path, &mut vm) {
                eprintln!("{e}");
                std::process::exit(1);
            }
            if cli.interactive {
                repl::start(&mut vm);
            }
        }
    }
}
