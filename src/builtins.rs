use crate::ast::{Expr, TableExpr};

#[derive(Debug, Clone, PartialEq)]
pub enum BuiltIn {
    Cols(Box<TableExpr>),
    /// `<table-expr> sink <path>` — stream a frame to a file. The left side is
    /// any table expression (`` `tbl ``, `select …`, `update …`, …), never a
    /// bare identifier. `path` is a string expression: a string literal or a
    /// string global.
    Sink { src: Box<TableExpr>, path: Expr },
    Sort(Box<TableExpr>, Vec<(String, bool)>),
    Distinct(Box<TableExpr>),
    /// `` `a`b dropnull <table-expr> `` — drop every row with a null in any of
    /// the named columns.
    DropNull(Vec<String>, Box<TableExpr>),
    /// `n limit <table-expr>` / `n#<table-expr>` — first `n` rows for `n >= 0`,
    /// last `|n|` rows (tail) for `n < 0`, kdb `#`-style. `n` is any
    /// scalar-valued expression (a literal, a bound global, …), evaluated at
    /// run time.
    Limit(Box<TableExpr>, Expr),
    Drop(Vec<String>, Box<TableExpr>),
    /// `lazy <table-expr>` — build a query plan and keep it lazy instead of
    /// materialising it into a DataFrame.
    Lazy(Box<TableExpr>),
    /// `collect <table-expr>` — force a lazy plan to materialise into a DataFrame.
    Collect(Box<TableExpr>),
}