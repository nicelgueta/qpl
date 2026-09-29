use polars::prelude::{DataFrame, JoinType, LazyFrame, NamedFrom, Series};

use crate::builtins::BuiltIn;

#[derive(Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Str(String),
    /// An interned symbol (`` `foo ``): names a column, table or path.
    Sym(String),
    Bool(bool),
    /// kdb+ temporal scalars, each holding kdb's integer offset (see
    /// [`crate::temporal`]); converted to Polars' 1970 epoch in `vm::ast_val_to_expr`.
    Date(i32), // days since 2000.01.01
    Month(i32),     // months since 2000.01
    Time(i64),      // ns since midnight
    Minute(i32),    // minutes since midnight
    Second(i32),    // seconds since midnight
    Timestamp(i64), // ns since 2000.01.01
    Timespan(i64),  // ns duration
    /// An open IPC connection (`hopen`). Exists in every build so matches over
    /// `Value` need no `#[cfg]`; only the `ipc` feature produces one.
    Handle(i64),
    /// A pending `async dispatch` response, resolved by `await` (see `Handle`).
    Future(i64),
    /// A function value (`{[x] x+1}`): params, compiled entry point and owning
    /// `Program`. Nothing is captured; the `Arc` only makes cloning cheap.
    /// Rejected inside a query expression.
    Closure(std::sync::Arc<crate::program::Closure>),
    /// An eager table binding. Never substituted into a column expression
    /// (`lookup_global` skips it).
    Table(DataFrame),
    /// A lazy query plan binding (`x: lazy select ...`). Boxed because an
    /// inline `LazyFrame` would bloat every `Value` enough to overflow the
    /// stack under deep recursion.
    Lazy(Box<LazyFrame>),
    /// Vectors are backed by a `Series` so Polars ops apply directly. Elements
    /// use the same raw representation as the scalar counterpart (e.g.
    /// `DateVec` holds day offsets from 2000.01.01). See [`VecKind`].
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

/// Manual because `LazyFrame` has no `Debug`; otherwise identical to `derive`.
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

/// Manual because tables compare with nulls equal (`equals_missing`), lazy
/// plans aren't comparable, and closures compare by identity.
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

/// Element type of a vector `Value`, so list operations are written once.
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

    /// The one-element vector of an atom (`enlist`); `None` if there's no
    /// vector counterpart.
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
// month/minute/second have no Polars dtype, so only `enlist` produces these
// vectors.
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
    /// `` `k1`k2!v1 v2 ``: ordered (key, value) pairs, only valid as `zip`'s
    /// argument. Each value is a single noun; compound values need parens.
    /// TODO: support as a standalone object
    Dict(Vec<(String, Expr)>),
    IColRef, // virtual row-index column `i`
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
    /// `<func> over `p1`p2 [order `k asc ...] [rolling n]`. `func` is an
    /// expression evaluated per partition, or a ranking verb (`rn`/`rank`/
    /// `drank`). `rolling` requires a plain aggregate.
    Window {
        func: Box<Expr>,
        partition: Vec<String>,
        order: Vec<(String, bool)>, // (column, descending)
        rolling: Option<usize>,
    },
    /// A table expression in value context (`` t`col ``, or a select feeding
    /// a reduction/slice/index/assignment). A one-column select becomes a
    /// list; anything else stays a frame.
    Table(Box<TableExpr>),
    /// `<n>#<expr>`: first `n` rows/elements, or last `-n` if negative.
    Take {
        n: Box<Expr>,
        expr: Box<Expr>,
    },
    /// `(<expr>) <i>` / `(<expr>) <i j k>`: positional index into a list.
    Index {
        expr: Box<Expr>,
        idx: Box<Expr>,
    },
    /// `f[a;b]` / `f[]`. A single-argument `f[x]` parses as `Index` and becomes
    /// a call at run time if `f` is a function.
    Apply {
        func: Box<Expr>,
        args: Vec<Expr>,
    },
    /// `{[p1,p2] ...}`: a function literal. Its body is compiled after the
    /// enclosing program's main code.
    Lambda(Function),
    /// `<conn> [async] dispatch <rest>`: send `command` (the rest of the
    /// statement's source) to the server. Sync waits for the reply; async
    /// returns a `Value::Future` for `await`.
    Dispatch {
        conn: Box<Expr>,
        command: String,
        is_async: bool,
    },
    /// `while[test; s1; ...]`: runs the statements in the current scope while
    /// `test` (a boolean atom) holds. Yields noop.
    While {
        cond: Box<Expr>,
        body: Vec<Stmt>,
    },
    /// `noop`: evaluates to nothing; can't be assigned or used as an operand.
    Noop,
    /// `<list> where <pred>, ...`: filters a list elementwise, with `x` bound
    /// to each element. (Table columns use `Select`'s `where_` instead.)
    ListWhere {
        list: Box<Expr>,
        where_: Vec<Expr>,
    },
}

/// A column alias in a select projection.
#[derive(Debug, Clone, PartialEq)]
pub struct Alias {
    pub name: Option<String>, // might not always have an alias, e.g. select col1 from df
    pub expr: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableSource {
    InMem(String),
    /// `load "path"` / `load name`, resolved by `Op::LoadFile` at run time.
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
    /// A bare table name or `load "path"`.
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
    // a bare expression: evaluated and printed
    SingleVar(Expr),
    /// Bareword `log a b c` (args rendered and concatenated) or bare `log`
    /// (blank line). `log[..]` parses as an ordinary call instead.
    Log(Vec<Expr>),
    /// `.qpl.cfg key=value ...`, or bare `.qpl.cfg` to print the settings.
    Cfg(String),
    /// A `\` command: `\d <stmt>`, `\l <path>`, `\i "<path>"` (`arg` is
    /// unquoted), `\1 <path>`, `\port <n>`. An empty `arg` turns `\1`/`\port` off.
    System {
        cmd: char,
        arg: String,
    },
}

/// A system command's full name for error messages (`'p'` is `\port`).
pub fn system_cmd_name(cmd: char) -> String {
    if cmd == 'p' {
        "port".to_string()
    } else {
        cmd.to_string()
    }
}

/// A function literal: `{[p1,p2] stmt; stmt; last-expr}`. The last statement
/// must be an expression (the return value). Nothing is captured: a call sees
/// its own params plus globals.
#[derive(Clone, PartialEq)]
pub struct Function {
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
}

/// Prints as `{[x,y] ..}` rather than the body AST, since runtime errors
/// interpolate `{v:?}`.
impl std::fmt::Debug for Function {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{[{}] ..}}", self.params.join(","))
    }
}
