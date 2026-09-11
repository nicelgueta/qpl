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
    /// kdb+ temporal scalars. Each carries the integer offset kdb uses; the
    /// conversion to the Polars (1970) epoch happens in `vm::ast_val_to_expr`.
    /// See [`crate::temporal`].
    Date(i32),      // days since 2000.01.01
    Month(i32),     // months since 2000.01
    Time(i64),      // ns since midnight
    Minute(i32),    // minutes since midnight
    Second(i32),    // seconds since midnight
    Timestamp(i64), // ns since 2000.01.01
    Timespan(i64),  // ns duration
    IntVec(Vec<i64>),
    FloatVec(Vec<f64>),
    SymVec(Vec<String>),
    StrVec(Vec<String>),
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
    /// an `order` sub-clause was given. `rolling` is `Some(n)` for the
    /// `<agg> <col> <n>!rolling over ...` form (a fixed `n`-row rolling
    /// aggregate); `func` must then be a plain aggregate call.
    Window {
        func: Box<Expr>,
        partition: Vec<String>,
        order: Vec<(String, bool)>, // (column, descending)
        rolling: Option<usize>,
    },
    /// A table expression used in a scalar / value context: `` name`col ``,
    /// `` name`c1`c2 ``, or a `select … from …` whose result feeds a reduction,
    /// slice, index or assignment rather than being printed as a table. A
    /// one-column `Select` is a *column expression* (materialises to a list
    /// `Value`); anything else stays a frame. Tree-walked by `resolve::eval_value`,
    /// never lowered to stack instructions.
    Table(Box<TableExpr>),
    /// `<n>#<expr>` — take the first `n` rows (`n >= 0`) or the last `-n`
    /// (`n < 0`) of a frame or list.
    Take { n: i64, expr: Box<Expr> },
    /// `(<expr>) <i>` / `(<expr>) <i j k>` — positional index into a list with a
    /// single int or an int run.
    Index { expr: Box<Expr>, idx: Box<Expr> },
    /// `f[a;b]` / `f[]` — apply a user function (defined with `name: {[..] ..}`)
    /// to a semicolon-separated argument list. `f[x]` with a single argument and
    /// no `;` parses as `Index` instead and is resolved to an application at run
    /// time when `f` names a function. Value context only; `func` is always an
    /// `Expr::ColRef` (higher-order use is unsupported).
    Apply { func: Box<Expr>, args: Vec<Expr> },
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
    SingleVar(Expr),
    /// `name: {[p1,p2] stmt; stmt; last-expr}` — bind a user function. The final
    /// statement in `body` must be an expression (its value is the return);
    /// earlier statements run for their (locally scoped) side effects.
    FuncDef { name: String, params: Vec<String>, body: Vec<Stmt> },
}

/// A user function bound by `Stmt::FuncDef`. Held in `Vm::functions` — a binding
/// kind alongside tables / lazy frames, not a first-class `Value`. The body is
/// kept as AST and recompiled per call (qpl recompiles every line anyway).
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
}
