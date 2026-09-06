use std::collections::HashMap;

use polars::prelude::JoinType;

use crate::builtins::BuiltIn;


#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    IntVec(Vec<i64>),
    FloatVec(Vec<f64>),
    SymVec(Vec<String>),
    BoolVec(Vec<bool>),
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
    Cast { dtype: String, expr: Box<Expr> },
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