use crate::ast::{TableExpr, TableSource, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum BuiltIn {
    Cols(Box<TableExpr>),
    Show(Box<TableExpr>),
    Sink {name: TableSource, path: Value},
    Sort(Box<TableExpr>, Vec<(String, bool)>),
    Distinct(Box<TableExpr>),
    Limit(Box<TableExpr>, usize),
    Drop(Vec<String>, Box<TableExpr>),
    /// `lazy <table-expr>` — build a query plan and keep it lazy instead of
    /// materialising it into a DataFrame.
    Lazy(Box<TableExpr>),
    /// `collect <table-expr>` — force a lazy plan to materialise into a DataFrame.
    Collect(Box<TableExpr>),
}