use polars::prelude::{DataFrame, JoinType, LazyFrame, NamedFrom, Series};

use crate::builtins::BuiltIn;

#[derive(Clone)]
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
    Date(i32), // days since 2000.01.01
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
    ///
    /// This wraps
    /// [`crate::program::Closure`] (params, its compiled entry point, and the
    /// `Arc<Program>` it belongs to) rather than the raw AST `Function` — a
    /// function literal compiles to bytecode once, at parse/compile time, and
    /// is invoked via `CALL`/`RET`, not re-walked per call.
    Closure(std::sync::Arc<crate::program::Closure>),
    /// A materialised table binding — one of `Vm`'s single binding map's two
    /// table-shaped kinds (see `Vm::globals` / `Vm::bind`). Meaningless inside
    /// a query expression — `vm::ast_val_to_expr` rejects it, and a bare
    /// lookup of one from `vm::Vm::lookup_global` returns `None` so it is
    /// never substituted as a literal into a column expression.
    Table(DataFrame),
    /// A stored **lazy** query plan (`x: lazy select ...`) — the other
    /// table-shaped binding kind. See `Table` above. Boxed: `LazyFrame` itself
    /// is a large struct (its whole optimizer/plan state, not just a handle),
    /// and inlining it here would balloon every `Value` — including the
    /// common scalar cases — to its size, which is enough to blow the stack
    /// on realistic function-call recursion (128 levels deep, each holding
    /// several `Value`/`Slot`s on the stack).
    Lazy(Box<LazyFrame>),
    /// Vector variants are all backed by a Polars `Series` so that native
    /// vectorised Polars operations (arithmetic, casts, gather/slice) apply
    /// directly instead of hand-rolled Rust loops. Each carries the same raw
    /// element representation as its scalar counterpart (e.g. `DateVec` holds
    /// day offsets since 2000.01.01, matching `Date`) — conversion to/from a
    /// native Polars dtype happens only in `vm::ast_val_to_expr` /
    /// `ops::column_to_value`. See [`VecKind`] for generic dispatch over
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

/// Manual only because `LazyFrame` has no `Debug` impl (a query plan isn't
/// meaningfully printable without collecting it, which `{v:?}` must never do
/// as a side effect); every other variant is formatted exactly as `derive`
/// would.
impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use Value::*;
        match self {
            Int(v) => f.debug_tuple("Int").field(v).finish(),
            Float(v) => f.debug_tuple("Float").field(v).finish(),
            Str(v) => f.debug_tuple("Str").field(v).finish(),
            Sym(v) => f.debug_tuple("Sym").field(v).finish(),
            Bool(v) => f.debug_tuple("Bool").field(v).finish(),
            Date(v) => f.debug_tuple("Date").field(v).finish(),
            Month(v) => f.debug_tuple("Month").field(v).finish(),
            Time(v) => f.debug_tuple("Time").field(v).finish(),
            Minute(v) => f.debug_tuple("Minute").field(v).finish(),
            Second(v) => f.debug_tuple("Second").field(v).finish(),
            Timestamp(v) => f.debug_tuple("Timestamp").field(v).finish(),
            Timespan(v) => f.debug_tuple("Timespan").field(v).finish(),
            Handle(v) => f.debug_tuple("Handle").field(v).finish(),
            Future(v) => f.debug_tuple("Future").field(v).finish(),
            Closure(v) => f.debug_tuple("Closure").field(v).finish(),
            Table(v) => f.debug_tuple("Table").field(v).finish(),
            Lazy(_) => write!(f, "Lazy(<lazy frame>)"),
            IntVec(v) => f.debug_tuple("IntVec").field(v).finish(),
            FloatVec(v) => f.debug_tuple("FloatVec").field(v).finish(),
            SymVec(v) => f.debug_tuple("SymVec").field(v).finish(),
            StrVec(v) => f.debug_tuple("StrVec").field(v).finish(),
            BoolVec(v) => f.debug_tuple("BoolVec").field(v).finish(),
            DateVec(v) => f.debug_tuple("DateVec").field(v).finish(),
            MonthVec(v) => f.debug_tuple("MonthVec").field(v).finish(),
            TimeVec(v) => f.debug_tuple("TimeVec").field(v).finish(),
            MinuteVec(v) => f.debug_tuple("MinuteVec").field(v).finish(),
            SecondVec(v) => f.debug_tuple("SecondVec").field(v).finish(),
            TimestampVec(v) => f.debug_tuple("TimestampVec").field(v).finish(),
            TimespanVec(v) => f.debug_tuple("TimespanVec").field(v).finish(),
        }
    }
}

/// Manual because `DataFrame` has no total-order-free equality worth deriving
/// (`equals_missing` treats nulls as equal to themselves, unlike Polars'
/// regular `==`), `LazyFrame` has none at all (comparing plans isn't
/// meaningful), and `Closure` compares by identity (`Arc::ptr_eq`) rather than
/// structurally — a function is only ever "the same" as itself in a `qpl`
/// program. Every other variant keeps the field-by-field equality `derive`
/// would have produced.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        use Value::*;
        match (self, other) {
            (Int(a), Int(b)) => a == b,
            (Float(a), Float(b)) => a == b,
            (Str(a), Str(b)) => a == b,
            (Sym(a), Sym(b)) => a == b,
            (Bool(a), Bool(b)) => a == b,
            (Date(a), Date(b)) => a == b,
            (Month(a), Month(b)) => a == b,
            (Time(a), Time(b)) => a == b,
            (Minute(a), Minute(b)) => a == b,
            (Second(a), Second(b)) => a == b,
            (Timestamp(a), Timestamp(b)) => a == b,
            (Timespan(a), Timespan(b)) => a == b,
            (Handle(a), Handle(b)) => a == b,
            (Future(a), Future(b)) => a == b,
            (Closure(a), Closure(b)) => std::sync::Arc::ptr_eq(a, b),
            (Table(a), Table(b)) => a.equals_missing(b),
            (Lazy(_), Lazy(_)) => false,
            (IntVec(a), IntVec(b)) => a == b,
            (FloatVec(a), FloatVec(b)) => a == b,
            (SymVec(a), SymVec(b)) => a == b,
            (StrVec(a), StrVec(b)) => a == b,
            (BoolVec(a), BoolVec(b)) => a == b,
            (DateVec(a), DateVec(b)) => a == b,
            (MonthVec(a), MonthVec(b)) => a == b,
            (TimeVec(a), TimeVec(b)) => a == b,
            (MinuteVec(a), MinuteVec(b)) => a == b,
            (SecondVec(a), SecondVec(b)) => a == b,
            (TimestampVec(a), TimestampVec(b)) => a == b,
            (TimespanVec(a), TimespanVec(b)) => a == b,
            _ => false,
        }
    }
}

/// Which element type a vector `Value` variant holds. Lets list-shaped
/// operations (materialise / index / take / scalarise) be written once,
/// generically, instead of once per vector variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VecKind {
    Int,
    Float,
    Sym,
    Str,
    Bool,
    Date,
    Month,
    Time,
    Minute,
    Second,
    Timestamp,
    Timespan,
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

    /// The one-element vector of an atomic scalar (`enlist`); `None` for
    /// anything that has no vector counterpart (a vector already, a handle, a closure).
    pub fn enlist(&self) -> Option<Value> {
        use Value::*;
        Some(match self {
            Int(n) => int_vec(vec![*n]),
            Float(f) => float_vec(vec![*f]),
            Bool(b) => bool_vec(vec![*b]),
            Str(s) => str_vec(vec![s.clone()]),
            Sym(s) => sym_vec(vec![s.clone()]),
            Date(d) => date_vec(vec![*d]),
            Month(m) => month_vec(vec![*m]),
            Time(t) => time_vec(vec![*t]),
            Minute(m) => minute_vec(vec![*m]),
            Second(s) => second_vec(vec![*s]),
            Timestamp(t) => timestamp_vec(vec![*t]),
            Timespan(t) => timespan_vec(vec![*t]),
            _ => return None,
        })
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

pub fn int_vec(v: Vec<i64>) -> Value {
    Value::IntVec(Series::new("".into(), v))
}
pub fn float_vec(v: Vec<f64>) -> Value {
    Value::FloatVec(Series::new("".into(), v))
}
pub fn bool_vec(v: Vec<bool>) -> Value {
    Value::BoolVec(Series::new("".into(), v))
}
pub fn sym_vec(v: Vec<String>) -> Value {
    Value::SymVec(str_series(v))
}
pub fn str_vec(v: Vec<String>) -> Value {
    Value::StrVec(str_series(v))
}
pub fn date_vec(v: Vec<i32>) -> Value {
    Value::DateVec(Series::new("".into(), v))
}
// `month` / `minute` / `second` have no native Polars dtype (only `date` /
// `time` / `datetime` / `duration` do), so — matching `CastTarget::Prim`,
// which only ever resolves those three for a *scalar* cast — nothing
// materialises a `MonthVec`/`MinuteVec`/`SecondVec` from a real column; only
// `enlist` produces them. The type has full parity with every other atomic
// scalar (see `VecKind`), ready for a real producer.
pub fn month_vec(v: Vec<i32>) -> Value {
    Value::MonthVec(Series::new("".into(), v))
}
pub fn time_vec(v: Vec<i64>) -> Value {
    Value::TimeVec(Series::new("".into(), v))
}
pub fn minute_vec(v: Vec<i32>) -> Value {
    Value::MinuteVec(Series::new("".into(), v))
}
pub fn second_vec(v: Vec<i32>) -> Value {
    Value::SecondVec(Series::new("".into(), v))
}
pub fn timestamp_vec(v: Vec<i64>) -> Value {
    Value::TimestampVec(Series::new("".into(), v))
}
pub fn timespan_vec(v: Vec<i64>) -> Value {
    Value::TimespanVec(Series::new("".into(), v))
}

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
    /// parens. Value context only, and only meaningful as `zip`'s own
    /// argument — `compiler::compile_value_expr`'s `zip` arm compiles it
    /// directly to `CAST_LIST`/`ZIP`; a bare `Dict` anywhere else is a
    /// compile-time error ("not supported in scalar context").
    Dict(Vec<(String, Expr)>),
    IColRef, // virtual i col (for indexing like: select i, col1, col2 from df)
    BinOp {
        left: Box<Expr>,
        op: String,
        right: Box<Expr>,
    },
    Call {
        func: String,
        args: Vec<Expr>,
    }, //  used for agg funcs like sum etc
    Cast {
        target: CastTarget,
        expr: Box<Expr>,
    },
    Case {
        branches: Vec<(Expr, Expr)>,
        default: Box<Expr>,
    },
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
    /// `Value`); anything else stays a frame. Compiled by
    /// `compiler::compile_value_expr`'s `Table` arm: frame ops, plus
    /// `COLUMN` when it's a one-column select.
    Table(Box<TableExpr>),
    /// `<n>#<expr>` — take the first `n` rows (`n >= 0`) or the last `-n`
    /// (`n < 0`) of a frame or list. `n` is any scalar-valued expression
    /// (a literal, a bound global, …), evaluated at run time.
    Take {
        n: Box<Expr>,
        expr: Box<Expr>,
    },
    /// `(<expr>) <i>` / `(<expr>) <i j k>` — positional index into a list with a
    /// single int or an int run.
    Index {
        expr: Box<Expr>,
        idx: Box<Expr>,
    },
    /// `f[a;b]` / `f[]` — apply a function to a semicolon-separated argument
    /// list. `f[x]` with a single argument and no `;` parses as `Index` instead
    /// and is resolved to an application at run time when `f` names a function.
    /// Value context only. `func` is usually an `Expr::ColRef`, but any
    /// expression evaluating to a `Value::Closure` applies.
    Apply {
        func: Box<Expr>,
        args: Vec<Expr>,
    },
    /// `{[p1,p2] stmt; ...; last-expr}` — a function literal. Compiled
    /// to `PUSH Func(proto)`, with the body
    /// appended as bytecode after the enclosing program's main code — see
    /// `compiler::compile_value_expr`'s `Lambda` arm. Value context only.
    Lambda(Function),
    /// `<conn> dispatch <rest of statement>` / `<conn> async dispatch <rest>` —
    /// ship `command` (the exact remaining source, reconstructed from tokens
    /// at parse time) to the connection named by `conn` and evaluate it there
    /// as if typed at that server's REPL. `is_async`: `dispatch` blocks for the
    /// reply; `async dispatch` returns a `Value::Future` immediately, resolved
    /// later by `await`. `ipc` feature only (see `Value::Handle`). Value
    /// context only; compiled directly to `Op::Dispatch`
    /// (`compiler::compile_value_expr`'s `Dispatch` arm).
    Dispatch {
        conn: Box<Expr>,
        command: String,
        is_async: bool,
    },
    /// `while[test; s1; ...; sn]` — while `test` (a boolean atom) is true, run
    /// the statements in order in the *current* scope (so assignments bind
    /// whatever scope the loop sits in: globals at the top level, locals inside
    /// a function). Yields noop. Value context only; compiled by
    /// `compiler::compile_while` to a backward `JUMP`/`JUMP_IF_FALSE` pair —
    /// nothing is interpreted from the AST.
    While {
        cond: Box<Expr>,
        body: Vec<Stmt>,
    },
    /// `noop` — evaluates to nothing: prints nothing, and can be neither
    /// assigned nor used as an operand (`Slot::Noop`). Value
    /// context only.
    Noop,
    /// `<expr> where <predicate>[, <predicate>...]` where `<expr>` is a *list*
    /// value (not a table-column expression, which has its own `where` sugar
    /// via `TableExpr::Select`'s `where_`) — filters the list elementwise.
    /// Each predicate is written against `x`, a plain column reference that
    /// resolves against the list's own (single, `x`-named) materialisation —
    /// see `compiler::compile_value_expr`'s `ListWhere` arm:
    /// `LIST_WHERE_FRAME`, the predicates in query context, `FILTER`, `COLUMN`.
    /// Value context only.
    ListWhere {
        list: Box<Expr>,
        where_: Vec<Expr>,
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
    /// `load "path.parquet"` / `load path` — a string literal or a bound
    /// scalar global; the path expression compiles to bytecode
    /// (`compiler::compile_source`) and is resolved by `Op::LoadFile` at run
    /// time.
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
    Assign {
        name: String,
        body: Box<Stmt>,
    },
    ScalarAssign {
        name: String,
        expr: Expr,
    },
    // single var on its own - this just evals and prints in repl
    SingleVar(Expr),
    /// The stdout write, in its bareword spelling: `log a b c` (each `Expr` is
    /// rendered and concatenated) or bare `log` (prints a blank line, `args`
    /// empty). The bracketed spelling `log[..]` parses as an ordinary
    /// `SingleVar(Expr::Call{func: "log", ..})` instead — see
    /// `compiler::compile_program`'s handling of a top-level `log[..]` call,
    /// which suppresses printing the same way this variant's compiled form
    /// does.
    Log(Vec<Expr>),
    /// `.qpl.cfg key=value key=value ...` (or a bare `.qpl.cfg`, `args`
    /// empty, which prints the current configuration).
    Cfg(String),
    /// A `\`-prefixed system command: `\d <stmt>` (disassemble), `\l <path>`
    /// (load a script flat), `\i "<path>"` (load a script as a namespaced
    /// import — `arg` is already the unquoted path), `\1 <path>` (mirror
    /// stdout to `<path>`; bare `\1` with an empty `arg` detaches it).
    System {
        cmd: char,
        arg: String,
    },
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
/// `{v:?}` on a `Value` is user-facing — it's what runtime errors
/// interpolate — and a dumped body drowns
/// both.
impl std::fmt::Debug for Function {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{[{}] ..}}", self.params.join(","))
    }
}
