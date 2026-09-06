use crate::ast::{TableExpr, TableSource, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum BuiltIn {
    Cols(Box<TableExpr>),
    Show(Box<TableExpr>),
    Sink {name: TableSource, path: Value},
    Sort(Box<TableExpr>, Vec<(String, bool)>),
    Distinct(Box<TableExpr>),
    Limit(Box<TableExpr>, usize),
}