mod ast;
mod builtins;
mod compiler;
mod errors;
mod lexer;
mod opcodes;
mod parser;
pub mod repl;
mod tokens;
mod vm;

fn main() {
    repl::start();
}
