use crate::ast::{Expr, TableExpr};

#[derive(Debug, Clone, PartialEq)]
pub enum BuiltIn {
    Cols(Box<TableExpr>),
    Show(Box<TableExpr>),
    /// `<table-expr> >> <path>` / `<table-expr> sink <path>` — stream a frame to
    /// a file. The left side is any table expression (`` `tbl ``, `select …`,
    /// `update …`, …), never a bare identifier. `path` is any scalar expression:
    /// a `` `literal ``, a string global, or `` `$expr `` to intern a string.
    Sink { src: Box<TableExpr>, path: Expr },
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