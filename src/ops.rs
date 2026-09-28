//! Type-dispatched value/column semantics shared by the `BINOP` and `CAST`
//! opcodes: binary operators and casts over scalars/vectors (evaluated
//! eagerly, in Rust) and over Polars column expressions. Moved out of `vm.rs`
//! so the opcode dispatch loop stays a thin `match` delegating here, per the
//! plan's "one evaluator, `ops.rs` holds the type-dispatched semantics" goal.

use crate::ast::{self, CastTarget, Value};
use crate::errors::QplError;
use crate::helpers;
use crate::program::BinOpKind;
use crate::temporal;
use crate::vm::{Slot, Vm};
use polars::prelude::*;

fn rt<E: std::fmt::Display>(e: E) -> QplError {
    QplError::Runtime(e.to_string())
}

pub(crate) const NS_PER_DAY: i64 = 86_400_000_000_000;
const NS_PER_MIN: i64 = 60_000_000_000;
const NS_PER_SEC: i64 = 1_000_000_000;
const NS_PER_MS: i64 = 1_000_000;

/// A temporal scalar as `(kind class, nanoseconds)` for comparison. Classes:
/// 0 = absolute instant (`date` / `timestamp` interchange), 1 = time of day
/// (`time` / `minute` / `second`), 2 = duration (`timespan`), 3 = month.
/// Comparison only crosses variants within the same class.
fn temporal_ns(v: &ast::Value) -> Option<(u8, i64)> {
    use ast::Value::*;
    Some(match *v {
        Timestamp(ns) => (0, ns),
        Date(d) => (0, d as i64 * NS_PER_DAY),
        Time(ns) => (1, ns),
        Minute(m) => (1, m as i64 * NS_PER_MIN),
        Second(s) => (1, s as i64 * NS_PER_SEC),
        Timespan(ns) => (2, ns),
        Month(m) => (3, m as i64),
        _ => return None,
    })
}

/// Nanosecond magnitude of a "duration-like" temporal scalar (`time`, `minute`,
/// `second`, `timespan`) added to / taken from a timestamp, date or time.
fn as_ns_delta(v: &ast::Value) -> Option<i64> {
    use ast::Value::*;
    Some(match *v {
        Time(ns) | Timespan(ns) => ns,
        Minute(m) => m as i64 * NS_PER_MIN,
        Second(s) => s as i64 * NS_PER_SEC,
        _ => return None,
    })
}

/// `<temporal> ± <int>` — kdb adds the integer in the operand's own resolution
/// (`date`+n days, `month`+n months, `time`+n ms, `minute`+n min, `second`+n s,
/// `timestamp`/`timespan`+n ns). `None` on a non-temporal `v` or on overflow.
fn shift_temporal_by_int(v: &ast::Value, n: i64) -> Option<ast::Value> {
    use ast::Value::*;
    let i32c = |x: i64| i32::try_from(x).ok();
    Some(match *v {
        Date(d) => Date(i32c(d as i64 + n)?),
        Month(m) => Month(i32c(m as i64 + n)?),
        Minute(x) => Minute(i32c(x as i64 + n)?),
        Second(x) => Second(i32c(x as i64 + n)?),
        Time(ns) => Time(ns.checked_add(n.checked_mul(NS_PER_MS)?)?),
        Timestamp(ns) => Timestamp(ns.checked_add(n)?),
        Timespan(ns) => Timespan(ns.checked_add(n)?),
        _ => return None,
    })
}

/// All binops where at least one side is a temporal scalar. `None` lets
/// `scalar_binop` fall through to the numeric path.
fn temporal_binop(
    l: &ast::Value,
    r: &ast::Value,
    op: &str,
) -> Option<Result<ast::Value, QplError>> {
    use ast::Value::*;
    let overflow = || QplError::Runtime("temporal arithmetic overflowed".into());

    // comparison — only within the same kind class
    if matches!(op, "=" | "<>" | "!=" | "<" | ">" | "<=" | ">=") {
        let ((lc, a), (rc, b)) = (temporal_ns(l)?, temporal_ns(r)?);
        if lc != rc {
            return Some(Err(QplError::Runtime(format!(
                "cannot compare {l:?} and {r:?}"
            ))));
        }
        return Some(Ok(Bool(match op {
            "=" => a == b,
            "<>" | "!=" => a != b,
            "<" => a < b,
            ">" => a > b,
            "<=" => a <= b,
            _ => a >= b,
        })));
    }

    if !matches!(op, "+" | "-" | "*") {
        return if temporal_ns(l).is_some() || temporal_ns(r).is_some() {
            Some(Err(QplError::Runtime(format!(
                "cannot apply '{op}' to {l:?} and {r:?}"
            ))))
        } else {
            None
        };
    }

    // temporal ± integer (each in the operand's own unit)
    if matches!(op, "+" | "-") {
        if let Int(n) = r
            && temporal_ns(l).is_some()
        {
            let n = if op == "-" { n.checked_neg()? } else { *n };
            return Some(shift_temporal_by_int(l, n).ok_or_else(overflow));
        }
        if let Int(n) = l
            && op == "+"
            && temporal_ns(r).is_some()
        {
            return Some(shift_temporal_by_int(r, *n).ok_or_else(overflow));
        }
        // timestamp / date / time / timespan ± a duration-like temporal
        if let Some(d0) = as_ns_delta(r) {
            let d = if op == "-" { d0.checked_neg()? } else { d0 };
            return Some(match *l {
                Timestamp(a) => a.checked_add(d).map(Timestamp).ok_or_else(overflow),
                Time(a) => a.checked_add(d).map(Time).ok_or_else(overflow),
                Timespan(a) => a.checked_add(d).map(Timespan).ok_or_else(overflow),
                Date(a) => (a as i64)
                    .checked_mul(NS_PER_DAY)
                    .and_then(|x| x.checked_add(d))
                    .map(Timestamp)
                    .ok_or_else(overflow),
                _ => {
                    return Some(Err(QplError::Runtime(format!(
                        "cannot apply '{op}' to {l:?} and {r:?}"
                    ))));
                }
            });
        }
        if op == "+" && as_ns_delta(l).is_some() && matches!(r, Timestamp(_) | Date(_)) {
            return temporal_binop(r, l, op); // commute
        }
    }

    let checked = |o: Option<ast::Value>| o.ok_or_else(overflow);
    Some(match (l, r, op) {
        (Date(a), Date(b), "-") => Ok(Int((*a - *b) as i64)),
        (Timestamp(a), Timestamp(b), "-") => checked(a.checked_sub(*b).map(Timespan)),
        (Month(a), Month(b), "-") => Ok(Int((*a - *b) as i64)),
        (Timespan(a), Int(b), "*") => checked(a.checked_mul(*b).map(Timespan)),
        (Int(a), Timespan(b), "*") => checked(b.checked_mul(*a).map(Timespan)),
        _ => Err(QplError::Runtime(format!(
            "cannot apply '{op}' to {l:?} and {r:?}"
        ))),
    })
}

pub(crate) fn scalar_binop(l: ast::Value, r: ast::Value, op: &str) -> Result<ast::Value, QplError> {
    use ast::Value::*;

    if (temporal_ns(&l).is_some() || temporal_ns(&r).is_some())
        && let Some(res) = temporal_binop(&l, &r, op)
    {
        return res;
    }

    // promote int to float when mixed
    let (l, r) = match (l, r) {
        (Int(a), Float(b)) => (Float(a as f64), Float(b)),
        (Float(a), Int(b)) => (Float(a), Float(b as f64)),
        pair => pair,
    };
    Ok(match (l, r, op) {
        (Int(a), Int(b), "+") => Int(a + b),
        (Int(a), Int(b), "-") => Int(a - b),
        (Int(a), Int(b), "*") => Int(a * b),
        // q's `%` is always true (float) division, even for two ints — 10%4 is
        // 2.5, not 2. Unlike the other int/int arms, this doesn't stay `Int`.
        (Int(a), Int(b), "%") => Float(a as f64 / b as f64),
        (Int(a), Int(b), "=") => Bool(a == b),
        (Int(a), Int(b), "<") => Bool(a < b),
        (Int(a), Int(b), ">") => Bool(a > b),
        (Int(a), Int(b), "<=") => Bool(a <= b),
        (Int(a), Int(b), ">=") => Bool(a >= b),
        (Int(a), Int(b), "<>" | "!=") => Bool(a != b),
        (Float(a), Float(b), "+") => Float(a + b),
        (Float(a), Float(b), "-") => Float(a - b),
        (Float(a), Float(b), "*") => Float(a * b),
        (Float(a), Float(b), "%") => Float(a / b),
        (Float(a), Float(b), "=") => Bool(a == b),
        (Float(a), Float(b), "<") => Bool(a < b),
        (Float(a), Float(b), ">") => Bool(a > b),
        (Float(a), Float(b), "<=") => Bool(a <= b),
        (Float(a), Float(b), ">=") => Bool(a >= b),
        (Str(a), Str(b), "+") => Str(a + &b),
        (Str(a) | Sym(a), Str(b) | Sym(b), "like") => Bool(like_match(&a, &b)?),
        (l, r, op) => {
            return Err(QplError::Runtime(format!(
                "cannot apply '{op}' to {l:?} and {r:?}"
            )));
        }
    })
}

/// `<vector> op <scalar>` / `<scalar> op <vector>` / `<vector> op <vector>` —
/// at least one operand is a vector `Value`. Reuses the same literal→`Expr`
/// bridge (`ast_val_to_expr`) and operator table (`apply_binop`) the table
/// pipeline uses for column expressions, so a vector composes with a scalar
/// exactly like a Polars column would (broadcasting a length-1 side, and
/// applying the same dtype/temporal promotion rules Polars applies to
/// columns) — this is the native vectorised path `scalar_binop` doesn't cover.
pub(crate) fn vector_binop(l: ast::Value, r: ast::Value, op: &str) -> Result<ast::Value, QplError> {
    let l_expr = crate::vm::ast_val_to_expr(l)?;
    let r_expr = crate::vm::ast_val_to_expr(r)?;
    let expr = apply_binop(l_expr, r_expr, &BinOpKind::from_op_str(op))?.alias("r");
    let df = df!("_" => [0i64])?.lazy().select([expr]).collect()?;
    column_to_value(df.column("r")?)
}

/// Combines [`scalar_binop`] / [`vector_binop`]'s dispatch (by whether either
/// side is a vector `Value`) with the `BinOpKind` → canonical-string bridge —
/// the `BINOP` opcode's "both operands are `Scalar`" path.
pub(crate) fn value_binop(l: Value, r: Value, op: &BinOpKind) -> Result<Value, QplError> {
    let op_str = op.as_str();
    if l.as_vec().is_some() || r.as_vec().is_some() {
        vector_binop(l, r, op_str)
    } else {
        scalar_binop(l, r, op_str)
    }
}

/// Scalar counterpart of `apply_binop`'s `"like"` arm: matches `text` against
/// a q-glob `pattern` directly, with no Polars column involved.
pub(crate) fn like_match(text: &str, pattern: &str) -> Result<bool, QplError> {
    let regex_src = like_pattern_to_regex(pattern);
    regex::Regex::new(&regex_src)
        .map(|re| re.is_match(text))
        .map_err(|e| QplError::Runtime(format!("invalid 'like' pattern '{pattern}': {e}")))
}

/// Casts a scalar [`Value`] to the family named by `dtype` — the same type
/// names [`crate::vm`]'s `polars_dtype` accepts. qpl scalars carry a single
/// integer and a single float type, so every `iN`/`uN` name folds to `Int` and
/// `f32`/`f64` to `Float`; the width only matters once the value reaches a
/// column.
pub(crate) fn scalar_cast(
    val: ast::Value,
    dtype: &str,
    use_qepoch: bool,
) -> Result<ast::Value, QplError> {
    use ast::Value::*;
    // shared string parse for `Str` / `Sym` sources; accepts an int- or
    // float-looking literal
    let as_int = |s: &str| {
        let s = s.trim();
        s.parse::<i64>()
            .ok()
            .or_else(|| s.parse::<f64>().ok().map(|f| f as i64))
    };
    let bad = |v: &ast::Value| QplError::Runtime(format!("cannot cast {v:?} to '{dtype}'"));
    // temporal targets (`` `date$x ``, `"p"$"…"`, …) have their own path
    if matches!(
        dtype,
        "date" | "month" | "time" | "minute" | "second" | "timestamp" | "timespan"
    ) {
        return scalar_temporal_cast(val, dtype, use_qepoch);
    }
    Ok(match dtype {
        "i64" | "int" | "long" | "i32" | "i16" | "i8" | "u64" | "u32" | "u16" | "u8" => match val {
            Int(n) => Int(n),
            Float(f) => Int(f as i64),
            Bool(b) => Int(b as i64),
            // a temporal scalar unwraps to its kdb integer offset
            Date(n) | Month(n) | Minute(n) | Second(n) => Int(n as i64),
            Time(n) | Timespan(n) => Int(n),
            // `timestamp` -> raw int crosses the epoch boundary: ns since the
            // Unix epoch by default, ns since kdb's 2000.01.01 with `useqepoch`
            Timestamp(n) => Int(if use_qepoch {
                n
            } else {
                n + temporal::NS_2000_TO_1970
            }),
            Str(ref s) | Sym(ref s) => Int(as_int(s)
                .ok_or_else(|| QplError::Runtime(format!("cannot parse '{s}' as '{dtype}'")))?),
            ref v => return Err(bad(v)),
        },
        "f64" | "float" | "f32" => match val {
            Int(n) => Float(n as f64),
            Float(f) => Float(f),
            Bool(b) => Float(b as i64 as f64),
            Str(ref s) | Sym(ref s) => Float(
                s.trim()
                    .parse::<f64>()
                    .map_err(|_| QplError::Runtime(format!("cannot parse '{s}' as '{dtype}'")))?,
            ),
            ref v => return Err(bad(v)),
        },
        "bool" => match val {
            Int(n) => Bool(n != 0),
            Float(f) => Bool(f != 0.0),
            Bool(b) => Bool(b),
            Str(ref s) | Sym(ref s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => Bool(true),
                "false" | "0" => Bool(false),
                _ => return Err(QplError::Runtime(format!("cannot parse '{s}' as 'bool'"))),
            },
            ref v => return Err(bad(v)),
        },
        "str" | "string" => match val {
            Int(n) => Str(n.to_string()),
            Float(f) => Str(f.to_string()),
            Bool(b) => Str(b.to_string()),
            Str(s) | Sym(s) => Str(s),
            ref v => match temporal::format_temporal(v) {
                Some(text) => Str(text),
                None => return Err(bad(v)),
            },
        },
        _ => return Err(QplError::Runtime(format!("unknown cast type '{dtype}'"))),
    })
}

/// `` `date$ ``, `` `month$ ``, `"p"$"…"` … — cast to a temporal scalar. A
/// string / symbol source is parsed with [`temporal::parse_temporal`]; a
/// temporal source is converted through its day- or nanosecond-offset; a plain
/// `Int` is reinterpreted directly as the offset (kdb `` `date$8000 ``).
fn scalar_temporal_cast(
    val: ast::Value,
    target: &str,
    use_qepoch: bool,
) -> Result<ast::Value, QplError> {
    use ast::Value::*;

    // string / symbol → parse, then fall through to the converters below
    let val = match val {
        Str(s) | Sym(s) => temporal::parse_temporal(&s)
            .ok_or_else(|| QplError::Runtime(format!("cannot parse {s:?} as a temporal value")))?,
        other => other,
    };

    // days since 2000.01.01 for any date-ish source
    let to_days = |v: &ast::Value| -> Option<i32> {
        Some(match *v {
            Date(d) => d,
            Timestamp(ns) => ns.div_euclid(NS_PER_DAY) as i32,
            Month(m) => {
                temporal::days_from_civil(2000 + m.div_euclid(12), (m.rem_euclid(12) + 1) as u32, 1)
                    - temporal::DAYS_2000_TO_1970
            }
            _ => return None,
        })
    };
    let bad = || QplError::Runtime(format!("cannot cast {val:?} to '{target}'"));

    Ok(match target {
        "date" => match val {
            Date(_) => val,
            Int(n) => Date(n as i32),
            ref v => Date(to_days(v).ok_or_else(bad)?),
        },
        "month" => match val {
            Month(_) => val,
            Int(n) => Month(n as i32),
            ref v => {
                let d = to_days(v).ok_or_else(bad)?;
                let (y, m, _) = temporal::civil_from_days(d + temporal::DAYS_2000_TO_1970);
                Month((y - 2000) * 12 + (m as i32 - 1))
            }
        },
        "timestamp" => match val {
            Timestamp(_) => val,
            // raw int crosses the epoch boundary: read as ns since the Unix
            // epoch by default, ns since kdb's 2000.01.01 with `useqepoch`
            Int(n) => Timestamp(if use_qepoch {
                n
            } else {
                n - temporal::NS_2000_TO_1970
            }),
            ref v => Timestamp(to_days(v).ok_or_else(bad)? as i64 * NS_PER_DAY),
        },
        "time" => match val {
            Time(_) => val,
            Int(n) => Time(n),
            Timestamp(ns) => Time(ns.rem_euclid(NS_PER_DAY)),
            Minute(m) => Time(m as i64 * 60_000_000_000),
            Second(s) => Time(s as i64 * 1_000_000_000),
            _ => return Err(bad()),
        },
        "minute" => match val {
            Minute(_) => val,
            Int(n) => Minute(n as i32),
            Time(ns) => Minute((ns / 60_000_000_000) as i32),
            Timestamp(ns) => Minute((ns.rem_euclid(NS_PER_DAY) / 60_000_000_000) as i32),
            Second(s) => Minute(s / 60),
            _ => return Err(bad()),
        },
        "second" => match val {
            Second(_) => val,
            Int(n) => Second(n as i32),
            Time(ns) => Second((ns / 1_000_000_000) as i32),
            Timestamp(ns) => Second((ns.rem_euclid(NS_PER_DAY) / 1_000_000_000) as i32),
            Minute(m) => Second(m * 60),
            _ => return Err(bad()),
        },
        "timespan" => match val {
            Timespan(_) => val,
            Int(n) => Timespan(n),
            Time(ns) => Timespan(ns),
            _ => return Err(bad()),
        },
        _ => return Err(QplError::Runtime(format!("unknown cast type '{target}'"))),
    })
}

/// The `CAST` opcode's "operand is an atomic `Scalar`" path: dispatches on the
/// [`CastTarget`]: a primitive target folds through [`scalar_cast`]; `` `$ ``
/// interns a string/symbol; a categorical/enum target is a column-only cast
/// and errors on a scalar.
pub(crate) fn scalar_cast_target(
    val: Value,
    target: &CastTarget,
    use_qepoch: bool,
) -> Result<Value, QplError> {
    match target {
        CastTarget::Prim(dtype) => scalar_cast(val, dtype, use_qepoch),
        // `` `$expr `` — intern a string into a symbol
        CastTarget::Sym => match val {
            Value::Str(s) | Value::Sym(s) => Ok(Value::Sym(s)),
            v => Err(QplError::Runtime(format!(
                "cannot make a symbol from {v:?}"
            ))),
        },
        CastTarget::SymPhysical(_) | CastTarget::Enum(_) => Err(QplError::Runtime(
            "categorical / enum casts apply to columns, not scalars".into(),
        )),
    }
}

pub(crate) fn apply_binop(left: Expr, right: Expr, op: &BinOpKind) -> Result<Expr, QplError> {
    use BinOpKind::*;
    if *op == Like {
        let pattern = match &right {
            Expr::Literal(lv) => lv.extract_str(),
            _ => None,
        }
        .ok_or_else(|| QplError::Runtime("'like's pattern must be a string literal".into()))?;
        let regex = like_pattern_to_regex(pattern);
        return Ok(left.str().contains(lit(regex), true));
    }
    Ok(match op {
        Add => left + right,
        Sub => left - right,
        Mul => left * right,
        // q's `%` is always true (float) division — Polars' `/` on two integer
        // columns truncates like Rust's own `/`, so force a float division by
        // casting both sides first (a no-op for columns already float).
        Div => left.cast(DataType::Float64) / right.cast(DataType::Float64),
        Eq => left.eq(right),
        Neq => left.neq(right),
        Lt => left.lt(right),
        Le => left.lt_eq(right),
        Gt => left.gt(right),
        Ge => left.gt_eq(right),
        And => left.and(right),
        Or => left.or(right),
        Like => unreachable!("handled above"),
        Other(op) => return Err(QplError::Runtime(format!("unknown operator '{op}'"))),
    })
}

/// Translates a q-style `like` glob pattern into an anchored regex.
///
/// q's glob syntax: `*` matches any sequence (incl. empty), `?` matches any
/// single character, `[abc]` / `[a-z]` / `[^abc]` are character classes. A
/// pattern character loses its special meaning inside `[...]` — including a
/// literal `]`, which is only a class member (not the closing bracket) when
/// it's the first character after `[` or `[^`, e.g. `[]]` matches `]`.
fn like_pattern_to_regex(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let n = chars.len();
    let mut out = String::from("^(?:");
    let mut i = 0;
    while i < n {
        match chars[i] {
            '*' => {
                out.push_str(".*");
                i += 1;
            }
            '?' => {
                out.push('.');
                i += 1;
            }
            '[' => {
                let open = i;
                i += 1;
                let mut class = String::new();
                if i < n && chars[i] == '^' {
                    class.push('^');
                    i += 1;
                }
                if i < n && chars[i] == ']' {
                    class.push_str("\\]");
                    i += 1;
                }
                while i < n && chars[i] != ']' {
                    // `\` and `[` are the only characters the regex crate
                    // still treats specially inside a class.
                    if chars[i] == '\\' || chars[i] == '[' {
                        class.push('\\');
                    }
                    class.push(chars[i]);
                    i += 1;
                }
                if i < n {
                    i += 1; // consume closing ']'
                    out.push('[');
                    out.push_str(&class);
                    out.push(']');
                } else {
                    // unterminated class: treat the '[' as a literal character
                    out.push_str("\\[");
                    i = open + 1;
                }
            }
            c => {
                if "\\.+^$|(){}".contains(c) {
                    out.push('\\');
                }
                out.push(c);
                i += 1;
            }
        }
    }
    out.push_str(")$");
    out
}

// --- list / column helpers shared by the opcodes
// (`vm.rs`'s `Op::Take`/`Op::Index`/`Op::Call`/`Op::Zip`/...) ---

/// Verbs that collapse a column to a single value.
pub(crate) fn is_reducer(f: &str) -> bool {
    matches!(
        f,
        "sum"
            | "avg"
            | "mean"
            | "min"
            | "max"
            | "first"
            | "last"
            | "count"
            | "std"
            | "dev"
            | "var"
            | "median"
            | "med"
            | "mode"
            | "modal"
            | "skew"
            | "kurt"
            | "kurtosis"
            | "any"
            | "all"
            | "prod"
            | "product"
            | "argmin"
            | "argmax"
            | "nnull"
            | "null_count"
            | "distinct"
            | "n_unique"
    )
}

/// Is this a list-shaped `Value` (as opposed to an atomic scalar)?
pub(crate) fn is_list_value(v: &Value) -> bool {
    v.as_vec().is_some()
}

pub(crate) fn first_col_name(lf: &LazyFrame) -> Result<String, QplError> {
    let mut lf = lf.clone();
    let schema = lf.collect_schema().map_err(rt)?;
    schema
        .iter_names()
        .next()
        .map(|s| s.to_string())
        .ok_or_else(|| QplError::Runtime("column expression has no columns".into()))
}

pub(crate) fn list_to_lazy(list: Value) -> Result<LazyFrame, QplError> {
    let expr = crate::vm::ast_val_to_expr(list)?.alias("x");
    Ok(df!("_" => [0i64]).map_err(rt)?.lazy().select([expr]))
}

/// Materialise one collected column into a qpl list `Value`. Rejects
/// null-containing columns and dtypes with no list representation. A temporal
/// column materialises to its matching typed vector (`DateVec`,
/// `TimestampVec`, …), carrying the same kdb integer offset its scalar
/// counterpart does.
pub(crate) fn column_to_value(col: &Column) -> Result<Value, QplError> {
    if col.null_count() > 0 {
        return Err(QplError::Runtime(format!(
            "column '{}' contains nulls and cannot be materialised into a list",
            col.name()
        )));
    }
    let dt = col.dtype();
    if dt.is_categorical() || dt.is_enum() {
        let c = col.cast(&DataType::String).map_err(rt)?;
        let ca = c.str().map_err(rt)?;
        return Ok(ast::sym_vec(
            ca.iter().flatten().map(str::to_owned).collect(),
        ));
    }
    Ok(match dt {
        DataType::Boolean => ast::bool_vec(col.bool().map_err(rt)?.iter().flatten().collect()),
        DataType::String => ast::str_vec(
            col.str()
                .map_err(rt)?
                .iter()
                .flatten()
                .map(str::to_owned)
                .collect(),
        ),
        DataType::Float32 | DataType::Float64 => {
            let c = col.cast(&DataType::Float64).map_err(rt)?;
            ast::float_vec(c.f64().map_err(rt)?.into_no_null_iter().collect())
        }
        DataType::Date => {
            let c = col.cast(&DataType::Int32).map_err(rt)?;
            ast::date_vec(
                c.i32()
                    .map_err(rt)?
                    .into_no_null_iter()
                    .map(|d| d - crate::temporal::DAYS_2000_TO_1970)
                    .collect(),
            )
        }
        // normalise to nanoseconds first — a column loaded from a file may be
        // ms / us resolution, not ns
        DataType::Datetime(_, _) => {
            let c = col
                .cast(&DataType::Datetime(TimeUnit::Nanoseconds, None))
                .map_err(rt)?
                .cast(&DataType::Int64)
                .map_err(rt)?;
            ast::timestamp_vec(
                c.i64()
                    .map_err(rt)?
                    .into_no_null_iter()
                    .map(|n| n - crate::temporal::NS_2000_TO_1970)
                    .collect(),
            )
        }
        DataType::Duration(_) => {
            let c = col
                .cast(&DataType::Duration(TimeUnit::Nanoseconds))
                .map_err(rt)?
                .cast(&DataType::Int64)
                .map_err(rt)?;
            ast::timespan_vec(c.i64().map_err(rt)?.into_no_null_iter().collect())
        }
        DataType::Time => {
            // Polars `Time` is always ns since midnight
            let c = col.cast(&DataType::Int64).map_err(rt)?;
            ast::time_vec(c.i64().map_err(rt)?.into_no_null_iter().collect())
        }
        d if d.is_integer() => {
            let c = col.cast(&DataType::Int64).map_err(rt)?;
            ast::int_vec(c.i64().map_err(rt)?.into_no_null_iter().collect())
        }
        other => {
            return Err(QplError::Runtime(format!(
                "column dtype {other:?} cannot be materialised into a list"
            )));
        }
    })
}

/// A one-element list collapses to the corresponding scalar.
pub(crate) fn scalarise(v: Value) -> Result<Value, QplError> {
    let (kind, s) = v.as_vec().ok_or_else(|| {
        QplError::Runtime(format!(
            "expected a single value from the reduction, got {v:?}"
        ))
    })?;
    if s.len() != 1 {
        return Err(QplError::Runtime(format!(
            "expected a single value from the reduction, got a list of length {}",
            s.len()
        )));
    }
    any_value_to_scalar(kind, s.get(0).map_err(rt)?)
}

fn any_value_to_scalar(kind: ast::VecKind, av: AnyValue) -> Result<Value, QplError> {
    use ast::VecKind::*;
    let bad = || QplError::Runtime(format!("cannot convert {av:?} to a scalar '{kind:?}'"));
    let as_str = |av: &AnyValue| -> Option<String> {
        match av {
            AnyValue::String(s) => Some((*s).to_owned()),
            AnyValue::StringOwned(s) => Some(s.to_string()),
            _ => None,
        }
    };
    Ok(match kind {
        Int => Value::Int(av.extract::<i64>().ok_or_else(bad)?),
        Float => Value::Float(av.extract::<f64>().ok_or_else(bad)?),
        Bool => match av {
            AnyValue::Boolean(b) => Value::Bool(b),
            _ => return Err(bad()),
        },
        Sym => Value::Sym(as_str(&av).ok_or_else(bad)?),
        Str => Value::Str(as_str(&av).ok_or_else(bad)?),
        Date => Value::Date(av.extract::<i32>().ok_or_else(bad)?),
        Month => Value::Month(av.extract::<i32>().ok_or_else(bad)?),
        Time => Value::Time(av.extract::<i64>().ok_or_else(bad)?),
        Minute => Value::Minute(av.extract::<i32>().ok_or_else(bad)?),
        Second => Value::Second(av.extract::<i32>().ok_or_else(bad)?),
        Timestamp => Value::Timestamp(av.extract::<i64>().ok_or_else(bad)?),
        Timespan => Value::Timespan(av.extract::<i64>().ok_or_else(bad)?),
    })
}

pub(crate) fn take_list(v: Value, n: i64) -> Result<Value, QplError> {
    let (kind, s) = v
        .as_vec()
        .ok_or_else(|| QplError::Runtime(format!("cannot slice {v:?} — not a list")))?;
    let len = s.len() as i64;
    let out = if n >= 0 {
        s.head(Some(n.clamp(0, len) as usize))
    } else {
        s.tail(Some((-n).clamp(0, len) as usize))
    };
    Ok(Value::from_vec(kind, out))
}

pub(crate) fn index_list(v: Value, idx: &[i64]) -> Result<Value, QplError> {
    let (kind, s) = v
        .as_vec()
        .ok_or_else(|| QplError::Runtime(format!("cannot index {v:?} — not a list")))?;
    Ok(Value::from_vec(kind, gather(s, idx)?))
}

fn gather(s: &Series, idx: &[i64]) -> Result<Series, QplError> {
    let len = s.len() as i64;
    let mut vals = Vec::with_capacity(idx.len());
    for &i in idx {
        if i < 0 || i >= len {
            return Err(QplError::Runtime(format!(
                "index {i} out of range for a list of length {len}"
            )));
        }
        vals.push(s.get(i as usize).map_err(rt)?);
    }
    Series::from_any_values_and_dtype(s.name().clone(), &vals, s.dtype(), false).map_err(rt)
}

// --- value-context call/verb dispatch ---
//
// These are the runtime bodies of the opcodes `vm.rs` uses for value
// context (`Op::Call`, `Op::Take`, `Op::Index`, `Op::Zip`, `Op::CastList`,
// `Op::Dispatch`): each takes already-popped `Slot`s (the opcode handler in
// `vm.rs` does all the stack bookkeeping) so the semantics live in one place.

/// [`Slot`] -> a concrete scalar/list `Value`; the exact error
/// wording for a `Frame` / `Noop` operand.
pub(crate) fn slot_to_scalar_value(slot: Slot) -> Result<Value, QplError> {
    match slot {
        Slot::Scalar(v) => Ok(v),
        Slot::Frame { .. } => Err(QplError::Runtime(
            "expected a scalar here, got a table".into(),
        )),
        Slot::Noop => Err(QplError::Runtime(
            "cannot use a no-op expression as a value".into(),
        )),
        other => Err(QplError::Runtime(format!(
            "Expected Expr on stack, got {}",
            other.type_name()
        ))),
    }
}

/// [`Slot`] -> a concrete list `Value`, materialising a single-column `Frame`
/// (rejecting a wider one).
pub(crate) fn slot_to_list_value(slot: Slot) -> Result<Value, QplError> {
    match slot {
        Slot::Noop => Err(QplError::Runtime(
            "cannot use a no-op expression as a value".into(),
        )),
        Slot::Scalar(v) => Ok(v),
        Slot::Frame { lf, .. } => {
            let df = (*lf).collect().map_err(rt)?;
            if df.width() != 1 {
                return Err(QplError::Runtime(
                    "can only index / slice a single-column expression".into(),
                ));
            }
            column_to_value(df.select_at_idx(0).expect("width == 1"))
        }
        other => Err(QplError::Runtime(format!(
            "Expected Expr on stack, got {}",
            other.type_name()
        ))),
    }
}

/// The elementwise form of value-context `?[..]`: `mask` is the boolean
/// vector condition that first diverted execution here (`Op::JumpIfVec`);
/// `rest` is every remaining condition, branch value and the default, in the
/// order they were pushed (`v1, c2, v2, …, ck, vk, d` — see
/// `compiler::compile_case_value`), already evaluated eagerly since each is
/// an ordinary compiled sub-expression rather than a thunk. Same semantics,
/// arm order and error text as the scalar `?[..]` path:
/// an atom operand broadcasts, a vector operand must match `mask`'s length,
/// the first true condition wins per element, only value operands (not
/// conditions) are checked for a text/non-text mix.
pub(crate) fn case_vec(mask: Value, rest: Vec<Slot>) -> Result<Value, QplError> {
    const COND_MSG: &str =
        "a `?[..]` condition must be a boolean scalar or vector in value context";
    let n = mask
        .as_vec()
        .map(|(_, s)| s.len())
        .expect("case_vec's mask is a BoolVec, guaranteed by Op::JumpIfVec");

    let mut items = rest
        .into_iter()
        .map(slot_to_scalar_value)
        .collect::<Result<Vec<Value>, QplError>>()?;
    let default = items.pop().expect("CASE_VEC always carries a default");
    let v1 = items.remove(0);

    fn check_len(v: &Value, what: &str, n: usize) -> Result<(), QplError> {
        if let Some((_, s)) = v.as_vec()
            && s.len() != n
        {
            return Err(QplError::Runtime(format!(
                "`?[..]` {what} has length {}, expected {n} (the length of the condition)",
                s.len()
            )));
        }
        Ok(())
    }
    fn kind_of(v: &Value) -> (bool, bool) {
        (
            matches!(
                v,
                Value::Str(_) | Value::Sym(_) | Value::StrVec(_) | Value::SymVec(_)
            ),
            matches!(v, Value::Sym(_) | Value::SymVec(_)),
        )
    }

    check_len(&v1, "branch", n)?;
    let mut kinds = vec![kind_of(&v1)];
    let mut arms = vec![(
        crate::vm::ast_val_to_expr(mask.clone())?,
        crate::vm::ast_val_to_expr(v1)?,
    )];

    let mut rest = items.into_iter();
    while let Some(c) = rest.next() {
        if !matches!(c, Value::Bool(_) | Value::BoolVec(_)) {
            return Err(QplError::Runtime(COND_MSG.into()));
        }
        check_len(&c, "condition", n)?;
        let v = rest
            .next()
            .expect("a condition is always paired with a value");
        check_len(&v, "branch", n)?;
        kinds.push(kind_of(&v));
        arms.push((
            crate::vm::ast_val_to_expr(c)?,
            crate::vm::ast_val_to_expr(v)?,
        ));
    }
    check_len(&default, "default", n)?;
    kinds.push(kind_of(&default));

    let mut acc = crate::vm::ast_val_to_expr(default)?;
    for (c, v) in arms.into_iter().rev() {
        acc = when(c).then(v).otherwise(acc);
    }
    if kinds.iter().any(|k| k.0) && !kinds.iter().all(|k| k.0) {
        return Err(QplError::Runtime(
            "`?[..]` branches mix text and non-text values".into(),
        ));
    }
    let all_sym = kinds.iter().all(|k| k.1);
    let df = df!("_" => [0i64])
        .map_err(rt)?
        .lazy()
        .select([acc.alias("r")])
        .collect()
        .map_err(rt)?;
    if df.height() != n {
        return Err(QplError::Runtime(format!(
            "`?[..]` produced {} value(s), expected {n} (the length of the condition)",
            df.height()
        )));
    }
    let out = column_to_value(df.column("r").map_err(rt)?)?;
    Ok(match out {
        Value::StrVec(s) if all_sym => Value::SymVec(s),
        other => other,
    })
}

/// [`Slot`] -> the lazy frame a column verb (`sum`, `shift`, `round`, …)
/// reduces/maps over, minus the "stay
/// lazy" fast path for an unmaterialised `` table`col `` (the compiler now
/// always collapses a one-column select to a `Scalar` list ahead of time —
/// see `compile_value_expr`'s `Expr::Table` arm — so this only ever sees a
/// `Frame` or an already-materialised list; a harmless extra round trip
/// through `list_to_lazy`, not a behaviour change).
fn slot_to_source_lf(slot: Slot) -> Result<LazyFrame, QplError> {
    match slot {
        Slot::Frame { lf, .. } => Ok(*lf),
        Slot::Scalar(v) => list_to_lazy(v),
        Slot::Noop => Err(QplError::Runtime(
            "cannot use a no-op expression as a value".into(),
        )),
        other => Err(QplError::Runtime(format!(
            "Expected Expr on stack, got {}",
            other.type_name()
        ))),
    }
}

/// `sum trades\`price`, `2 shift px`, `2 round px`, `cumsum px`, … in value
/// context — the generic-verb tail of `Op::Call`'s dispatch (`call_by_name`).
/// operand-for-operand the generic call path.
pub(crate) fn value_verb(vm: &mut Vm, name: &str, args: Vec<Slot>) -> Result<Slot, QplError> {
    let mut args = args.into_iter();
    let source = args
        .next()
        .ok_or_else(|| QplError::Runtime(format!("'{name}' needs at least one argument")))?;
    let param = args.next();
    let is_monadic = param.is_none();
    let lf = slot_to_source_lf(source)?;
    let col_name = first_col_name(&lf)?;
    let base = col(col_name.as_str());

    let applied = if name == "round" {
        let decimals = match param {
            Some(Slot::Scalar(Value::Int(n))) if n >= 0 => n as u32,
            _ => {
                return Err(QplError::Runtime(
                    "round expects `<precision> round <column>` with a non-negative integer literal"
                        .into(),
                ));
            }
        };
        base.round(decimals, vm.config.round_type)
    } else if let Some(p) = param {
        let p_val = slot_to_scalar_value(p)?;
        let p_expr = crate::vm::ast_val_to_expr(p_val)?;
        crate::vm::apply_dyadic(name, base, p_expr)?
    } else {
        crate::vm::apply_call(name, vec![base])?
    };

    let df = lf.select([applied.alias("r")]).collect().map_err(rt)?;
    let materialised = column_to_value(df.column("r").map_err(rt)?)?;
    if is_monadic && is_reducer(name) {
        Ok(Slot::Scalar(scalarise(materialised)?))
    } else {
        Ok(Slot::Scalar(materialised))
    }
}

/// `til n` → `0 .. n-1`; `lo til hi` → `lo .. hi-1`, operating on already-evaluated `Slot`s.
pub(crate) fn native_til(args: Vec<Slot>) -> Result<Slot, QplError> {
    let vals = args
        .into_iter()
        .map(slot_to_scalar_value)
        .collect::<Result<Vec<_>, _>>()?;
    let int_of = |v: &Value| match v {
        Value::Int(n) => Ok(*n),
        other => Err(QplError::Runtime(format!(
            "'til' expects an integer, got {other:?}"
        ))),
    };
    let (lo, hi) = match vals.as_slice() {
        [n] => (0i64, int_of(n)?),
        [hi, lo] => (int_of(lo)?, int_of(hi)?),
        _ => return Err(QplError::Runtime("'til' takes 1 or 2 arguments".into())),
    };
    if hi < lo {
        return Err(QplError::Runtime(format!(
            "'til': upper bound {hi} is less than lower bound {lo}"
        )));
    }
    Ok(Slot::Scalar(ast::int_vec((lo..hi).collect())))
}

/// `enlist <value>`.
pub(crate) fn native_enlist(args: Vec<Slot>) -> Result<Slot, QplError> {
    let mut args = args.into_iter();
    let v = slot_to_scalar_value(args.next().ok_or_else(|| {
        QplError::Runtime("'enlist' expects a single atom, not a list or function".into())
    })?)?;
    v.enlist().map(Slot::Scalar).ok_or_else(|| {
        QplError::Runtime("'enlist' expects a single atom, not a list or function".into())
    })
}

/// `<n>?<x>`. `args` is `[x, n]` (right
/// operand first, matching `til`'s convention).
pub(crate) fn native_roll(args: Vec<Slot>) -> Result<Slot, QplError> {
    let mut args = args.into_iter();
    let x = args
        .next()
        .ok_or_else(|| QplError::Runtime("'?' needs two arguments".into()))?;
    let n_slot = args
        .next()
        .ok_or_else(|| QplError::Runtime("'?' needs two arguments".into()))?;
    let n = match slot_to_scalar_value(n_slot)? {
        Value::Int(n) if n >= 0 => n as usize,
        other => {
            return Err(QplError::Runtime(format!(
                "'?' expects a non-negative int count on the left, got {other:?}"
            )));
        }
    };
    let rolled = match slot_to_list_value(x)? {
        Value::Int(hi) if hi > 0 => ast::int_vec(
            (0..n)
                .map(|_| helpers::rand_below(hi as u64) as i64)
                .collect(),
        ),
        Value::Int(hi) => {
            return Err(QplError::Runtime(format!(
                "'?' needs a positive upper bound to roll ints in, got {hi}"
            )));
        }
        Value::Float(hi) => ast::float_vec((0..n).map(|_| hi * helpers::rand_unit()).collect()),
        list if is_list_value(&list) => {
            let len = list.as_vec().expect("checked by is_list_value").1.len();
            if len == 0 && n > 0 {
                return Err(QplError::Runtime(
                    "'?' cannot roll from an empty list".into(),
                ));
            }
            let idx: Vec<i64> = (0..n)
                .map(|_| helpers::rand_below(len as u64) as i64)
                .collect();
            index_list(list, &idx)?
        }
        other => {
            return Err(QplError::Runtime(format!(
                "'?' expects an int, float or list on the right, got {other:?}"
            )));
        }
    };
    Ok(Slot::Scalar(rolled))
}

/// Render each argument via `repl::fmt_log_val` and concatenate, then write
/// through [`Vm::emit`], minus the
/// AST-evaluation step (already done by the compiled `Op::Call` args).
pub(crate) fn native_log(vm: &mut Vm, args: Vec<Slot>) -> Result<Slot, QplError> {
    let mut text = String::new();
    for a in args {
        let v = slot_to_scalar_value(a)?;
        text.push_str(&crate::repl::fmt_log_val(&v));
    }
    vm.emit(&text);
    Ok(Slot::Scalar(Value::Str(text)))
}

/// One `zip` column's Series, once already reduced to a list `Value` (a
/// top-level cast is handled separately by `Op::CastList`, which keeps the
/// column's exact width instead of round-tripping through `Value`) — the
/// non-cast tail of `zip`'s column conversion.
pub(crate) fn zip_value_to_series(v: Value, name: &str) -> Result<Series, QplError> {
    let (kind, s) = v
        .as_vec()
        .ok_or_else(|| QplError::Runtime(format!("'zip' column '{name}' is not a list: {v:?}")))?;
    if matches!(
        kind,
        ast::VecKind::Date
            | ast::VecKind::Month
            | ast::VecKind::Time
            | ast::VecKind::Minute
            | ast::VecKind::Second
            | ast::VecKind::Timestamp
            | ast::VecKind::Timespan
    ) {
        let df = list_to_lazy(v.clone())?.collect().map_err(rt)?;
        return Ok(df.column("x").map_err(rt)?.as_materialized_series().clone());
    }
    Ok(s.clone())
}

/// The runtime body of `Op::Call`'s "name resolves to something other than a
/// bound closure/builtin" path: `hopen`/`whopen`/`await`/`log`/`til` (in that
/// order) then the generic column verb (`value_verb`). `vm.is_callable(name)` — checked by the caller
/// first — is what lets a user function shadow any of these.
pub(crate) fn call_by_name(vm: &mut Vm, name: &str, args: Vec<Slot>) -> Result<Slot, QplError> {
    match name {
        #[cfg(feature = "ipc")]
        "hopen" if args.len() == 1 => native_hopen(vm, args, crate::ipc::HandleMode::Read),
        #[cfg(feature = "ipc")]
        "whopen" if args.len() == 1 => native_hopen(vm, args, crate::ipc::HandleMode::Write),
        #[cfg(feature = "ipc")]
        "await" if args.len() == 1 => native_await(vm, args),
        #[cfg(not(feature = "ipc"))]
        "hopen" | "whopen" | "await" if args.len() == 1 => Err(QplError::Runtime(format!(
            "'{name}' requires the `ipc` feature (on by default; this build used `--no-default-features`)"
        ))),
        "log" => native_log(vm, args),
        "til" => native_til(args),
        _ if (1..=2).contains(&args.len()) => value_verb(vm, name, args),
        _ => Err(QplError::Runtime(format!(
            "not supported in scalar context: Call {{ func: {name:?}, args: .. }}"
        ))),
    }
}

#[cfg(feature = "ipc")]
pub(crate) fn native_hopen(
    vm: &mut Vm,
    args: Vec<Slot>,
    mode: crate::ipc::HandleMode,
) -> Result<Slot, QplError> {
    let addr_val = slot_to_scalar_value(
        args.into_iter()
            .next()
            .expect("arity checked by the caller"),
    )?;
    let addr = match addr_val {
        Value::Str(s) | Value::Sym(s) => s,
        Value::Int(n) => n.to_string(),
        other => {
            return Err(QplError::Runtime(format!(
                "hopen expects a port or \"host:port\", got {other:?}"
            )));
        }
    };
    let conn = crate::ipc::hopen(&addr, mode)?;
    let id = vm.next_handle;
    vm.next_handle += 1;
    vm.connections.insert(id, conn);
    Ok(Slot::Scalar(Value::Handle(id)))
}

#[cfg(feature = "ipc")]
pub(crate) fn native_await(vm: &mut Vm, args: Vec<Slot>) -> Result<Slot, QplError> {
    let id_val = slot_to_scalar_value(
        args.into_iter()
            .next()
            .expect("arity checked by the caller"),
    )?;
    let id = match id_val {
        Value::Future(id) => id,
        other => {
            return Err(QplError::Runtime(format!(
                "await expects a pending response (from async dispatch), got {other:?}"
            )));
        }
    };
    let intr = vm.interrupt.clone();
    let rx = vm.pending.get(&id).ok_or_else(|| {
        QplError::Runtime("await: no such pending response (already awaited?)".into())
    })?;
    let reply = crate::ipc::await_reply(rx, &intr);
    if !matches!(reply, Err(QplError::Interrupted)) {
        vm.pending.remove(&id);
    }
    eval_result_to_slot(reply?)
}

/// `<conn> dispatch <cmd>` / `<conn> async dispatch <cmd>` — the `Op::Dispatch`
/// body.
#[cfg(feature = "ipc")]
pub(crate) fn native_dispatch(
    vm: &mut Vm,
    conn: Value,
    command: &str,
    is_async: bool,
) -> Result<Slot, QplError> {
    let handle = match conn {
        Value::Handle(id) => id,
        other => {
            return Err(QplError::Runtime(format!(
                "dispatch expects a connection (from hopen), got {other:?}"
            )));
        }
    };
    let client = vm
        .connections
        .get(&handle)
        .ok_or_else(|| QplError::Runtime("dispatch: no such connection (closed?)".into()))?;
    if is_async {
        let rx = crate::ipc::enqueue(client, command.to_string())?;
        let id = vm.next_handle;
        vm.next_handle += 1;
        vm.pending.insert(id, rx);
        Ok(Slot::Scalar(Value::Future(id)))
    } else {
        let intr = vm.interrupt.clone();
        eval_result_to_slot(crate::ipc::dispatch_blocking(
            client,
            command.to_string(),
            &intr,
        )?)
    }
}

/// A dispatched command's outcome, folded to a `Slot`.
#[cfg(feature = "ipc")]
fn eval_result_to_slot(result: crate::vm::EvalResult) -> Result<Slot, QplError> {
    use crate::vm::EvalResult;
    Ok(match result {
        EvalResult::Table(df) => Slot::Frame {
            lf: Box::new(df.lazy()),
            lazy: false,
        },
        EvalResult::Scalar(v) => Slot::Scalar(v),
        EvalResult::Stored => Slot::Scalar(Value::Bool(true)),
        EvalResult::Lazy(text) => Slot::Scalar(Value::Str(text)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_cast_covers_every_family() {
        use ast::Value::*;
        assert_eq!(scalar_cast(Float(45.3), "int", false).unwrap(), Int(45));
        assert_eq!(scalar_cast(Int(45), "f64", false).unwrap(), Float(45.0));
        assert_eq!(scalar_cast(Bool(true), "bool", false).unwrap(), Bool(true));
        assert_eq!(
            scalar_cast(Int(45), "str", false).unwrap(),
            Str("45".into())
        );
    }

    #[test]
    fn value_binop_dispatches_scalar_vs_vector() {
        use ast::Value::*;
        assert_eq!(
            value_binop(Int(1), Int(2), &BinOpKind::Add).unwrap(),
            Int(3)
        );
        assert_eq!(
            value_binop(ast::int_vec(vec![1, 2]), Int(1), &BinOpKind::Add).unwrap(),
            ast::int_vec(vec![2, 3])
        );
    }

    #[test]
    fn like_match_matches_a_glob() {
        assert!(like_match("quick", "qu?ck").unwrap());
        assert!(!like_match("quick", "slow").unwrap());
    }
}
