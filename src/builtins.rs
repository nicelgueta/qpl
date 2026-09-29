use crate::ast::{Expr, TableExpr};

#[derive(Debug, Clone, PartialEq)]
pub enum BuiltIn {
    Cols(Box<TableExpr>),
    /// `<table-expr> sink <path>`: stream a frame to a file. `path` is a
    /// string literal or string global.
    Sink {
        src: Box<TableExpr>,
        path: Expr,
    },
    Sort(Box<TableExpr>, Vec<(String, bool)>),
    Distinct(Box<TableExpr>),
    /// `` `a`b dropnull <table-expr> ``: drop rows with a null in any named column.
    DropNull(Vec<String>, Box<TableExpr>),
    /// `n limit <table-expr>` / `n#<table-expr>`: first `n` rows, or last `-n`
    /// if negative. `n` is evaluated at run time.
    Limit(Box<TableExpr>, Expr),
    Drop(Vec<String>, Box<TableExpr>),
    /// `lazy <table-expr>`: keep the plan lazy instead of collecting it.
    Lazy(Box<TableExpr>),
    /// `collect <table-expr>`: collect a lazy plan into a DataFrame.
    Collect(Box<TableExpr>),
}
