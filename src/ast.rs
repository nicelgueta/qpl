use polars::prelude::{JoinType, NamedFrom, Series};

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
    /// `conn: hopen 5001` — an opaque handle to an open IPC connection. Only
    /// meaningful as the left operand of `dispatch`/`async dispatch`. `ipc`
    /// feature only; the variant itself always exists so non-`ipc` builds
    /// don't need `#[cfg]` on every match over `Value`.
    Handle(i64),
    /// `resp: conn async dispatch ...` — a pending response, resolved by
    /// `await`. `ipc` feature only (see `Handle`).
    Future(i64),
    /// `f: {[x] x+1}` / a bare `{[x] x+1}` in expression position — a
    /// function as a first-class value: passable, returnable, storable. There
    /// is no captured environment (see [`Function`]), so the `Arc` is shared
    /// purely to keep cloning a binding cheap. Meaningless inside a query
    /// expression — `vm::ast_val_to_expr` rejects it.
    Closure(std::sync::Arc<Function>),
    /// Vector variants are all backed by a Polars `Series` so that native
    /// vectorised Polars operations (arithmetic, casts, gather/slice) apply
    /// directly instead of hand-rolled Rust loops. Each carries the same raw
    /// element representation as its scalar counterpart (e.g. `DateVec` holds
    /// day offsets since 2000.01.01, matching `Date`) — conversion to/from a
    /// native Polars dtype happens only in `vm::ast_val_to_expr` /
    /// `resolve::column_to_value`. See [`VecKind`] for generic dispatch over
    /// these variants.
    IntVec(Series),
    FloatVec(Series),
    SymVec(Series),
    StrVec(Series),
    BoolVec(Series),
    DateVec(Series),
    MonthVec(Series),
    TimeVec(Series),
    MinuteVec(Series),
    SecondVec(Series),
    TimestampVec(Series),
    TimespanVec(Series),
}

/// Which element type a vector `Value` variant holds. Lets list-shaped
/// operations (materialise / index / take / scalarise) be written once,
/// generically, instead of once per vector variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VecKind {
    Int, Float, Sym, Str, Bool,
    Date, Month, Time, Minute, Second, Timestamp, Timespan,
}

impl Value {
    /// If `self` is a vector variant, its kind and backing `Series`.
    pub fn as_vec(&self) -> Option<(VecKind, &Series)> {
        use Value::*;
        Some(match self {
            IntVec(s) => (VecKind::Int, s),
            FloatVec(s) => (VecKind::Float, s),
            SymVec(s) => (VecKind::Sym, s),
            StrVec(s) => (VecKind::Str, s),
            BoolVec(s) => (VecKind::Bool, s),
            DateVec(s) => (VecKind::Date, s),
            MonthVec(s) => (VecKind::Month, s),
            TimeVec(s) => (VecKind::Time, s),
            MinuteVec(s) => (VecKind::Minute, s),
            SecondVec(s) => (VecKind::Second, s),
            TimestampVec(s) => (VecKind::Timestamp, s),
            TimespanVec(s) => (VecKind::Timespan, s),
            _ => return None,
        })
    }

    /// Extract the string values of a `SymVec` / `StrVec` as owned `String`s.
    pub fn vec_strings(&self) -> Result<Vec<String>, String> {
        match self {
            Value::SymVec(s) | Value::StrVec(s) => Ok(s
                .str()
                .map_err(|e| e.to_string())?
                .iter()
                .flatten()
                .map(str::to_owned)
                .collect()),
            other => Err(format!("expected a symbol vector, got {other:?}")),
        }
    }

    /// Build a vector `Value` of the given kind from a backing `Series`.
    pub fn from_vec(kind: VecKind, s: Series) -> Value {
        match kind {
            VecKind::Int => Value::IntVec(s),
            VecKind::Float => Value::FloatVec(s),
            VecKind::Sym => Value::SymVec(s),
            VecKind::Str => Value::StrVec(s),
            VecKind::Bool => Value::BoolVec(s),
            VecKind::Date => Value::DateVec(s),
            VecKind::Month => Value::MonthVec(s),
            VecKind::Time => Value::TimeVec(s),
            VecKind::Minute => Value::MinuteVec(s),
            VecKind::Second => Value::SecondVec(s),
            VecKind::Timestamp => Value::TimestampVec(s),
            VecKind::Timespan => Value::TimespanVec(s),
        }
    }
}

pub fn int_vec(v: Vec<i64>) -> Value { Value::IntVec(Series::new("".into(), v)) }
pub fn float_vec(v: Vec<f64>) -> Value { Value::FloatVec(Series::new("".into(), v)) }
pub fn bool_vec(v: Vec<bool>) -> Value { Value::BoolVec(Series::new("".into(), v)) }
pub fn sym_vec(v: Vec<String>) -> Value { Value::SymVec(str_series(v)) }
pub fn str_vec(v: Vec<String>) -> Value { Value::StrVec(str_series(v)) }
pub fn date_vec(v: Vec<i32>) -> Value { Value::DateVec(Series::new("".into(), v)) }
// `month` / `minute` / `second` have no native Polars dtype (only `date` /
// `time` / `datetime` / `duration` do), so — matching `CastTarget::Prim`,
// which only ever resolves those three for a *scalar* cast — nothing yet
// materialises a `MonthVec`/`MinuteVec`/`SecondVec` from a real column.
// These constructors exist so the type itself has full parity with every
// other atomic scalar (see `VecKind`), ready for a future producer.
#[allow(dead_code)]
pub fn month_vec(v: Vec<i32>) -> Value { Value::MonthVec(Series::new("".into(), v)) }
pub fn time_vec(v: Vec<i64>) -> Value { Value::TimeVec(Series::new("".into(), v)) }
#[allow(dead_code)]
pub fn minute_vec(v: Vec<i32>) -> Value { Value::MinuteVec(Series::new("".into(), v)) }
#[allow(dead_code)]
pub fn second_vec(v: Vec<i32>) -> Value { Value::SecondVec(Series::new("".into(), v)) }
pub fn timestamp_vec(v: Vec<i64>) -> Value { Value::TimestampVec(Series::new("".into(), v)) }
pub fn timespan_vec(v: Vec<i64>) -> Value { Value::TimespanVec(Series::new("".into(), v)) }

fn str_series(v: Vec<String>) -> Series {
    let strs: Vec<&str> = v.iter().map(String::as_str).collect();
    Series::new("".into(), strs)
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
    /// `` `k1`k2!v1 v2 `` — a dict literal: an ordered list of (key, value-expr)
    /// pairs (order matters — it becomes column order when fed to `zip`).
    /// Each value is parsed as a single noun; a compound expression needs
    /// parens. Value context only, never lowered to stack instructions —
    /// see `resolve::eval_value`'s `zip` handling.
    Dict(Vec<(String, Expr)>),
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
    /// (`n < 0`) of a frame or list. `n` is any scalar-valued expression
    /// (a literal, a bound global, …), evaluated at run time.
    Take { n: Box<Expr>, expr: Box<Expr> },
    /// `(<expr>) <i>` / `(<expr>) <i j k>` — positional index into a list with a
    /// single int or an int run.
    Index { expr: Box<Expr>, idx: Box<Expr> },
    /// `f[a;b]` / `f[]` — apply a function to a semicolon-separated argument
    /// list. `f[x]` with a single argument and no `;` parses as `Index` instead
    /// and is resolved to an application at run time when `f` names a function.
    /// Value context only. `func` is usually an `Expr::ColRef`, but any
    /// expression evaluating to a `Value::Closure` applies.
    Apply { func: Box<Expr>, args: Vec<Expr> },
    /// `<conn> dispatch <rest of statement>` / `<conn> async dispatch <rest>` —
    /// ship `command` (the exact remaining source, reconstructed from tokens
    /// at parse time) to the connection named by `conn` and evaluate it there
    /// as if typed at that server's REPL. `is_async`: `dispatch` blocks for the
    /// reply; `async dispatch` returns a `Value::Future` immediately, resolved
    /// later by `await`. `ipc` feature only (see `Value::Handle`). Value
    /// context only, tree-walked by `resolve::eval_value` like `Table` above —
    /// there's nothing here for the compiler to lower.
    Dispatch { conn: Box<Expr>, command: String, is_async: bool },
    /// `<expr> where <predicate>[, <predicate>...]` where `<expr>` is a *list*
    /// value (not a table-column expression, which has its own `where` sugar
    /// via `TableExpr::Select`'s `where_`) — filters the list elementwise.
    /// Each predicate is written against `x`, a plain column reference that
    /// resolves against the list's own (single, `x`-named) materialisation —
    /// see `resolve::eval_value`'s `Expr::ListWhere` arm. Value context only,
    /// never lowered to stack instructions.
    ListWhere { list: Box<Expr>, where_: Vec<Expr> },
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
    /// `load "path.parquet"` / `load path` — a string literal or a bound
    /// scalar global, resolved to a path at run time (see `vm::run_program`'s
    /// `TableSource::Load` arm).
    Load(Box<Expr>),
}


#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub cols: Vec<Alias>,
    pub from: Box<TableExpr>,
    pub by: Option<Vec<Alias>>,
    pub where_: Option<Vec<Expr>>,
    pub order: Option<Vec<(String, bool)>>,
    pub join: Option<(Box<TableExpr>, Value, Value, JoinType)>,
    pub update: bool,
    pub delete: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableExpr {
    Select(SelectStmt),
    BuiltIn(BuiltIn),
    /// a bare table name or a `load "path"` — the base case a `from` clause
    /// eventually bottoms out at once any nested table expressions are peeled away.
    Source(TableSource),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    RetTable(TableExpr),
    Assign { name: String, body: Box<Stmt> },
    ScalarAssign { name: String, expr: Expr },
    // single var on its own - this just evals and prints in repl
    SingleVar(Expr),
}

/// A function literal: `{[p1,p2] stmt; stmt; last-expr}`. Wrapped in a
/// [`Value::Closure`] the moment it is parsed, so `name: {[..] ..}` is just an
/// ordinary scalar assignment and a function is an ordinary value. The final
/// statement in `body` must be an expression (its value is the return); earlier
/// statements run for their (locally scoped) side effects. Nothing is captured
/// — a call sees its own params plus the session globals, exactly as a named
/// function always has (see `Vm::lookup`). The body is kept as AST and
/// recompiled per call (qpl recompiles every line anyway).
#[derive(Clone, PartialEq)]
pub struct Function {
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
}

/// Prints as the source shape (`{[x,y] ..}`) rather than the whole body AST.
/// `{v:?}` on a `Value` is user-facing — it's what `\d` disassembles an
/// `EVAL` to and what runtime errors interpolate — and a dumped body drowns
/// both.
impl std::fmt::Debug for Function {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{[{}] ..}}", self.params.join(","))
    }
}
