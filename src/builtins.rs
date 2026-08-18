use crate::ast::{SelectStmt, Value, TableSource};


#[derive(Debug, Clone, PartialEq)]
pub enum BuiltIn {
    Cols(String),
    Show(SelectStmt),
    Sink {name: TableSource, path: Value}
}