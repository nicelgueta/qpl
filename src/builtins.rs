use crate::ast::{TableExpr, TableSource, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum BuiltIn {
    Cols(Box<TableExpr>),
    Show(Box<TableExpr>),
    Sink {name: TableSource, path: Value},
    Asc(Box<TableExpr>, String),
    Desc(Box<TableExpr>, String),
}