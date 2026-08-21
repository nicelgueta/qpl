use crate::ast::{SelectStmt, TableExpr, TableSource, Value, Expr};

#[derive(Debug, Clone, PartialEq)]
pub enum BuiltIn {
    Cols(String),
    Show(SelectStmt),
    Sink {name: TableSource, path: Value},
    Asc(Box<TableExpr>, String),
    Desc(Box<TableExpr>, String),
}