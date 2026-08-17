use crate::ast::SelectStmt;


#[derive(Debug, Clone, PartialEq)]
pub enum BuiltIn {
    Cols(String),
    Show(SelectStmt),
}