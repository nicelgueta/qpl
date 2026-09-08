use std::collections::HashMap;

use polars::prelude::JoinType;

use crate::builtins::BuiltIn;


#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    /// an interned symbol — `` `foo `` outside a table expression; names a
    /// column, table or path. Distinct from `Str` even though it wraps a `String`.
    Sym(String),
    Bool(bool),
    IntVec(Vec<i64>),
    FloatVec(Vec<f64>),
    SymVec(Vec<String>),
    BoolVec(Vec<bool>),
}

/// Target of a `$` / `` `$ `` cast.
#[derive(Debug, Clone, PartialEq)]
pub enum CastTarget {
    /// primitive dtype: `f64`, `i32`, `u8`, `bool`, `str`, ...
    Prim(String),
    /// `` `$expr `` — to a Polars `Categorical`, default (u32) physical width
    Sym,
    /// `` u8!`$expr `` — to `Categorical` with an explicit physical width (`u8`/`u16`/`u32`)
    SymPhysical(String),
    /// `` name::`$expr `` — to a Polars `Enum` built from the global symbol vector `name`
    Enum(String),
}


#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Lit(Value),
    Sym(String),
    ColRef(String),
    Dict(HashMap<String, Value>),
    IColRef, // virtual i col (for indexing like: select i, col1, col2 from df)
    BinOp { left: Box<Expr>, op: String, right: Box<Expr>,},
    Call { func: String, args: Vec<Expr>,}, //  used for agg funcs like sum etc
    Cast { target: CastTarget, expr: Box<Expr> },
    Case { branches: Vec<(Expr, Expr)>, default: Box<Expr> },
    /// `<func> over `p1`p2 [order `k1 asc `k2 desc]` — a window function.
    /// `func` is either a column expression (`max salary`) applied per partition,
    /// or the bare ranking verb `rn` / `rank` / `drank`. `order` is empty unless
    /// an `order` sub-clause was given (only meaningful for the ranking verbs).
    Window {
        func: Box<Expr>,
        partition: Vec<String>,
        order: Vec<(String, bool)>, // (column, descending)
    },
}


/// used for aliasing columns in select statements
#[derive(Debug, Clone, PartialEq)]
pub struct Alias {
    pub name: Option<String>, // might not always have an alias, e.g. select col1 from df
    pub expr: Expr,
}


#[derive(Debug, Clone, PartialEq)]
pub enum TableSource {
    InMem(String),
    Load(String),
}


#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub cols: Vec<Alias>,
    pub from: TableSource,
    pub by: Option<Vec<Alias>>,
    pub where_: Option<Vec<Expr>>,
    pub order: Option<Vec<(String, bool)>>,
    pub join: Option<(TableSource, Value, Value, JoinType)>,
    pub update: bool,
    pub delete: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableExpr {
    Select(SelectStmt),
    BuiltIn(BuiltIn),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    RetTable(TableExpr),
    Assign { name: String, body: Box<Stmt> },
    ScalarAssign { name: String, expr: Expr },
    // single var on its own - this just evals and prints in repl
    SingleVar(Expr)
}