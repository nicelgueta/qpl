use crate::ast::{Expr, TableExpr, TableSource};

#[derive(Debug, Clone, PartialEq)]
pub enum BuiltIn {
    Cols(Box<TableExpr>),
    Show(Box<TableExpr>),
    /// `<table> >> <path>` / `<table> sink <path>` — stream the frame to a file.
    /// `path` is any scalar expression: a `` `literal ``, a string global, or
    /// `` `$expr `` to cast a string to a symbol path.
    Sink {name: TableSource, path: Expr},
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