use crate::ast::{Expr, Program, Stmt};
use crate::errors::QplError;
use crate::opcodes::Opcode;

pub struct Bytecode {
    pub instructions: Vec<Opcode>,
    pub constants: Vec<crate::vm::Value>,
}

pub struct Compiler {
    instructions: Vec<Opcode>,
    constants: Vec<crate::vm::Value>,
}

impl Compiler {
    pub fn new() -> Self {
        Self {
            instructions: Vec::new(),
            constants: Vec::new(),
        }
    }

    pub fn compile(&mut self, program: Program) -> Result<Bytecode, QplError> {
        for stmt in program.stmts {
            self.compile_stmt(stmt)?;
        }
        Ok(Bytecode {
            instructions: std::mem::take(&mut self.instructions),
            constants: std::mem::take(&mut self.constants),
        })
    }

    fn compile_stmt(&mut self, stmt: Stmt) -> Result<(), QplError> {
        // TODO: implement statement compilation
        Ok(())
    }

    fn compile_expr(&mut self, expr: Expr) -> Result<(), QplError> {
        // TODO: implement expression compilation
        Ok(())
    }
}
