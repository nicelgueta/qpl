//! Runtime resolution for the scalar / value side of the language.
//!
//! `Stmt::ScalarAssign` / `Stmt::SingleVar` compile to a single
//! `Instruction::Eval(expr)`; [`Vm::run_program`](crate::vm::Vm) hands that expr
//! here. Unlike the table pipeline this layer is a plain tree-walk: it resolves a
//! bare name against the session (`globals` → `lazy_frames` → `tables`), turns a
//! one-column `select` / `` table`col `` into a materialised list, reduces a
//! column to a scalar, and slices / indexes lists.

use polars::prelude::*;

use crate::ast::{self, Expr, TableExpr, Value};
use crate::errors::QplError;
use crate::vm::Vm;

/// The outcome of evaluating a value expression: a concrete scalar / list, or a
/// frame (a bare table name, a multi-column `select`, `n#<table>`).
#[allow(clippy::large_enum_variant)] // mirrors `vm::StackObj` / `EvalResult`
pub enum EvalValue {
    Scalar(ast::Value),
    Frame { lf: LazyFrame, lazy: bool },
}

fn rt<E: std::fmt::Display>(e: E) -> QplError {
    QplError::Runtime(e.to_string())
}

/// Evaluate a value expression coming from `Instruction::Eval`.
pub fn eval_value(vm: &mut Vm, expr: &Expr) -> Result<EvalValue, QplError> {
    match expr {
        // a bare name: global scalar, lazy binding, or materialised table
        Expr::ColRef(name) => resolve_name(vm, name),

        // `` name`col `` / `` name`c1`c2 `` / a `select …` used as a value
        Expr::Table(te) => eval_table(vm, te),

        // `<n>#<expr>` — head (`n >= 0`) / tail (`n < 0`) slice
        Expr::Take { n, expr } => match eval_value(vm, expr)? {
            EvalValue::Frame { lf, lazy } => {
                let lf = if *n >= 0 {
                    lf.limit(*n as IdxSize)
                } else {
                    let k = -*n;
                    lf.slice(-k, k as IdxSize)
                };
                Ok(EvalValue::Frame { lf, lazy })
            }
            EvalValue::Scalar(list) => Ok(EvalValue::Scalar(take_list(list, *n)?)),
        },

        // `<list>[<i>]` / `<list>[<i j k>]` / `(<expr>) <i j k>` — positional
        // index. A single int picks an atom; an int run picks a sub-list.
        Expr::Index { expr, idx } => {
            let list = to_list(vm, expr)?;
            let (idxs, atom) = match vm.eval_scalar(idx)? {
                Value::Int(n) => (vec![n], true),
                Value::IntVec(v) => (v, false),
                other => {
                    return Err(QplError::Runtime(format!(
                        "index must be an int or int vector, got {other:?}"
                    )))
                }
            };
            let picked = index_list(list, &idxs)?;
            Ok(EvalValue::Scalar(if atom { scalarise(picked)? } else { picked }))
        }

        // `sum trades`price`, `2 shift px`, `2 round px`, `cumsum px`, …
        Expr::Call { func, args } if (1..=2).contains(&args.len()) => eval_call(vm, func, args),

        // everything else is a pure scalar fold (literals, symbols, binops, casts)
        other => Ok(EvalValue::Scalar(vm.eval_scalar(other)?)),
    }
}

fn resolve_name(vm: &Vm, name: &str) -> Result<EvalValue, QplError> {
    if let Some(v) = vm.globals.get(name) {
        return Ok(EvalValue::Scalar(v.clone()));
    }
    if let Some(lf) = vm.lazy_frames.get(name) {
        return Ok(EvalValue::Frame { lf: lf.clone(), lazy: true });
    }
    if let Some(df) = vm.tables.get(name) {
        return Ok(EvalValue::Frame { lf: df.clone().lazy(), lazy: false });
    }
    Err(QplError::Runtime(format!(
        "undefined name '{name}' (not a variable, table or lazy frame)"
    )))
}

fn eval_table(vm: &mut Vm, te: &TableExpr) -> Result<EvalValue, QplError> {
    let mut instrs = Vec::new();
    crate::compiler::compile_tbl_expr(te, &mut instrs)?;
    let (lf, lazy) = vm.eval_frame(instrs)?;
    if is_column_select(te) {
        let df = lf.collect().map_err(rt)?;
        let col = df
            .select_at_idx(0)
            .ok_or_else(|| QplError::Runtime("empty column expression".into()))?;
        return Ok(EvalValue::Scalar(column_to_value(col)?));
    }
    Ok(EvalValue::Frame { lf, lazy })
}

/// A one-column `select` with no `by` — a *column expression* that materialises
/// to a list rather than a frame.
fn is_column_select(te: &TableExpr) -> bool {
    matches!(te, TableExpr::Select(sel)
        if sel.cols.len() == 1 && sel.by.is_none() && !sel.update && !sel.delete)
}

/// Coerce a value expression to a concrete list: materialise a single-column
/// frame, reject a multi-column one.
fn to_list(vm: &mut Vm, expr: &Expr) -> Result<Value, QplError> {
    match eval_value(vm, expr)? {
        EvalValue::Scalar(v) => Ok(v),
        EvalValue::Frame { lf, .. } => {
            let df = lf.collect().map_err(rt)?;
            if df.width() != 1 {
                return Err(QplError::Runtime(
                    "can only index / slice a single-column expression".into(),
                ));
            }
            column_to_value(df.select_at_idx(0).expect("width == 1"))
        }
    }
}

/// Apply a monadic / dyadic column verb to a column expression or list. A
/// reducing verb on a single argument yields a scalar; anything else yields a
/// new list.
fn eval_call(vm: &mut Vm, func: &str, args: &[Expr]) -> Result<EvalValue, QplError> {
    let lf = match eval_value(vm, &args[0])? {
        EvalValue::Frame { lf, .. } => lf,
        EvalValue::Scalar(list) => list_to_lazy(list)?,
    };
    let name = first_col_name(&lf)?;
    let base = col(name.as_str());

    let applied = if func == "round" {
        let decimals = match args.get(1) {
            Some(Expr::Lit(Value::Int(n))) if *n >= 0 => *n as u32,
            _ => {
                return Err(QplError::Runtime(
                    "round expects `<precision> round <column>` with a non-negative integer literal"
                        .into(),
                ))
            }
        };
        base.round(decimals, vm.config.round_type)
    } else if let Some(param) = args.get(1) {
        let p = crate::vm::ast_val_to_expr(vm.eval_scalar(param)?)?;
        crate::vm::apply_dyadic(func, base, p)?
    } else {
        crate::vm::apply_call(func, vec![base])?
    };

    let df = lf.select([applied.alias("r")]).collect().map_err(rt)?;
    let materialised = column_to_value(df.column("r").map_err(rt)?)?;
    if args.len() == 1 && is_reducer(func) {
        Ok(EvalValue::Scalar(scalarise(materialised)?))
    } else {
        Ok(EvalValue::Scalar(materialised))
    }
}

/// Verbs that collapse a column to a single value.
fn is_reducer(f: &str) -> bool {
    matches!(
        f,
        "sum" | "avg" | "mean" | "min" | "max" | "first" | "last" | "count"
            | "std" | "dev" | "var" | "median" | "med" | "mode" | "modal"
            | "skew" | "kurt" | "kurtosis" | "any" | "all" | "prod" | "product"
            | "argmin" | "argmax" | "nnull" | "null_count" | "distinct" | "n_unique"
    )
}

fn first_col_name(lf: &LazyFrame) -> Result<String, QplError> {
    let mut lf = lf.clone();
    let schema = lf.collect_schema().map_err(rt)?;
    schema
        .iter_names()
        .next()
        .map(|s| s.to_string())
        .ok_or_else(|| QplError::Runtime("column expression has no columns".into()))
}

fn list_to_lazy(list: Value) -> Result<LazyFrame, QplError> {
    let s: Series = match list {
        Value::IntVec(v) => Series::new("x".into(), v),
        Value::FloatVec(v) => Series::new("x".into(), v),
        Value::BoolVec(v) => Series::new("x".into(), v),
        Value::StrVec(v) | Value::SymVec(v) => {
            let strs: Vec<&str> = v.iter().map(String::as_str).collect();
            Series::new("x".into(), strs)
        }
        Value::Int(n) => Series::new("x".into(), [n]),
        Value::Float(n) => Series::new("x".into(), [n]),
        Value::Bool(b) => Series::new("x".into(), [b]),
        Value::Str(s) | Value::Sym(s) => Series::new("x".into(), [s]),
        // temporal scalars: a 1-row frame via the shared literal bridge
        v @ (Value::Date(_) | Value::Month(_) | Value::Time(_) | Value::Minute(_)
            | Value::Second(_) | Value::Timestamp(_) | Value::Timespan(_)) => {
            return Ok(df!("x" => [0i64])
                .map_err(rt)?
                .lazy()
                .select([crate::vm::ast_val_to_expr(v)?.alias("x")]));
        }
    };
    Ok(s.into_frame().lazy())
}

/// Materialise one collected column into a qpl list `Value`. Rejects
/// null-containing columns and dtypes with no list representation.
fn column_to_value(col: &Column) -> Result<Value, QplError> {
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
        return Ok(Value::SymVec(ca.iter().flatten().map(str::to_owned).collect()));
    }
    Ok(match dt {
        DataType::Boolean => {
            Value::BoolVec(col.bool().map_err(rt)?.iter().flatten().collect())
        }
        DataType::String => Value::StrVec(
            col.str().map_err(rt)?.iter().flatten().map(str::to_owned).collect(),
        ),
        DataType::Float32 | DataType::Float64 => {
            let c = col.cast(&DataType::Float64).map_err(rt)?;
            Value::FloatVec(c.f64().map_err(rt)?.into_no_null_iter().collect())
        }
        // temporal columns materialise to their kdb integer offsets (there is
        // no typed temporal-vector `Value` yet — Phase 2).
        DataType::Date => {
            let c = col.cast(&DataType::Int32).map_err(rt)?;
            Value::IntVec(
                c.i32().map_err(rt)?.into_no_null_iter()
                    .map(|d| (d - crate::temporal::DAYS_2000_TO_1970) as i64).collect(),
            )
        }
        // normalise to nanoseconds first — a column loaded from a file may be
        // ms / us resolution, not ns
        DataType::Datetime(_, _) => {
            let c = col
                .cast(&DataType::Datetime(TimeUnit::Nanoseconds, None)).map_err(rt)?
                .cast(&DataType::Int64).map_err(rt)?;
            Value::IntVec(
                c.i64().map_err(rt)?.into_no_null_iter()
                    .map(|n| n - crate::temporal::NS_2000_TO_1970).collect(),
            )
        }
        DataType::Duration(_) => {
            let c = col
                .cast(&DataType::Duration(TimeUnit::Nanoseconds)).map_err(rt)?
                .cast(&DataType::Int64).map_err(rt)?;
            Value::IntVec(c.i64().map_err(rt)?.into_no_null_iter().collect())
        }
        DataType::Time => {
            // Polars `Time` is always ns since midnight
            let c = col.cast(&DataType::Int64).map_err(rt)?;
            Value::IntVec(c.i64().map_err(rt)?.into_no_null_iter().collect())
        }
        d if d.is_integer() => {
            let c = col.cast(&DataType::Int64).map_err(rt)?;
            Value::IntVec(c.i64().map_err(rt)?.into_no_null_iter().collect())
        }
        other => {
            return Err(QplError::Runtime(format!(
                "column dtype {other:?} cannot be materialised into a list"
            )))
        }
    })
}

/// A one-element list collapses to the corresponding scalar.
fn scalarise(v: Value) -> Result<Value, QplError> {
    Ok(match v {
        Value::IntVec(mut xs) if xs.len() == 1 => Value::Int(xs.pop().unwrap()),
        Value::FloatVec(mut xs) if xs.len() == 1 => Value::Float(xs.pop().unwrap()),
        Value::StrVec(mut xs) if xs.len() == 1 => Value::Str(xs.pop().unwrap()),
        Value::SymVec(mut xs) if xs.len() == 1 => Value::Sym(xs.pop().unwrap()),
        Value::BoolVec(mut xs) if xs.len() == 1 => Value::Bool(xs.pop().unwrap()),
        other => {
            return Err(QplError::Runtime(format!(
                "expected a single value from the reduction, got {other:?}"
            )))
        }
    })
}

fn take_list(v: Value, n: i64) -> Result<Value, QplError> {
    Ok(match v {
        Value::IntVec(xs) => Value::IntVec(head_tail(&xs, n)),
        Value::FloatVec(xs) => Value::FloatVec(head_tail(&xs, n)),
        Value::StrVec(xs) => Value::StrVec(head_tail(&xs, n)),
        Value::SymVec(xs) => Value::SymVec(head_tail(&xs, n)),
        Value::BoolVec(xs) => Value::BoolVec(head_tail(&xs, n)),
        other => {
            return Err(QplError::Runtime(format!(
                "cannot slice {other:?} — not a list"
            )))
        }
    })
}

fn head_tail<T: Clone>(xs: &[T], n: i64) -> Vec<T> {
    let len = xs.len() as i64;
    if n >= 0 {
        xs[..n.clamp(0, len) as usize].to_vec()
    } else {
        xs[(len - (-n).clamp(0, len)) as usize..].to_vec()
    }
}

fn index_list(v: Value, idx: &[i64]) -> Result<Value, QplError> {
    Ok(match v {
        Value::IntVec(xs) => Value::IntVec(gather(&xs, idx)?),
        Value::FloatVec(xs) => Value::FloatVec(gather(&xs, idx)?),
        Value::StrVec(xs) => Value::StrVec(gather(&xs, idx)?),
        Value::SymVec(xs) => Value::SymVec(gather(&xs, idx)?),
        Value::BoolVec(xs) => Value::BoolVec(gather(&xs, idx)?),
        other => {
            return Err(QplError::Runtime(format!(
                "cannot index {other:?} — not a list"
            )))
        }
    })
}

fn gather<T: Clone>(xs: &[T], idx: &[i64]) -> Result<Vec<T>, QplError> {
    idx.iter()
        .map(|&i| {
            usize::try_from(i)
                .ok()
                .and_then(|u| xs.get(u))
                .cloned()
                .ok_or_else(|| {
                    QplError::Runtime(format!(
                        "index {i} out of range for a list of length {}",
                        xs.len()
                    ))
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::{run_vm, EvalResult, Vm};

    fn make_vm() -> Vm {
        let df = df![
            "c1" => ["a", "b", "a", "c"],
            "c2" => [10i64, 20, 30, 15],
            "c3" => [1.0f64, 2.0, 3.0, 4.0],
        ]
        .unwrap();
        let mut vm = Vm::new();
        vm.tables.insert("t".into(), df);
        vm
    }

    fn scalar(vm: &mut Vm, src: &str) -> Value {
        match run_vm(src, vm).expect("run") {
            EvalResult::Scalar(v) => v,
            other => panic!("expected scalar, got a different result kind: {}", kind(&other)),
        }
    }

    fn kind(r: &EvalResult) -> &'static str {
        match r {
            EvalResult::Table(_) => "table",
            EvalResult::Stored => "stored",
            EvalResult::Scalar(_) => "scalar",
            EvalResult::Lazy(_) => "lazy",
        }
    }

    #[test]
    fn table_col_materialises_to_a_list() {
        assert_eq!(scalar(&mut make_vm(), "t`c3"), Value::FloatVec(vec![1.0, 2.0, 3.0, 4.0]));
    }

    #[test]
    fn one_column_select_in_assignment_materialises_to_a_list() {
        let mut vm = make_vm();
        assert!(matches!(run_vm("px: select c2 from t", &mut vm), Ok(EvalResult::Stored)));
        assert_eq!(vm.globals.get("px"), Some(&Value::IntVec(vec![10, 20, 30, 15])));
    }

    #[test]
    fn bare_one_column_select_still_prints_a_table() {
        assert!(matches!(run_vm("select c2 from t", &mut make_vm()), Ok(EvalResult::Table(_))));
    }

    #[test]
    fn where_filters_a_column_expression() {
        assert_eq!(scalar(&mut make_vm(), "t`c2 where c2 > 15"), Value::IntVec(vec![20, 30]));
    }

    #[test]
    fn reduction_collapses_to_a_scalar_global() {
        let mut vm = make_vm();
        assert!(matches!(run_vm("m: max t`c2", &mut vm), Ok(EvalResult::Stored)));
        assert_eq!(vm.globals.get("m"), Some(&Value::Int(30)));
    }

    #[test]
    fn reduction_over_a_one_column_select() {
        assert_eq!(scalar(&mut make_vm(), "first select c1 from t"), Value::Str("a".into()));
    }

    #[test]
    fn take_head_and_tail() {
        assert_eq!(scalar(&mut make_vm(), "2#t`c2"), Value::IntVec(vec![10, 20]));
        assert_eq!(scalar(&mut make_vm(), "-2#t`c2"), Value::IntVec(vec![30, 15]));
    }

    #[test]
    fn positional_index_with_an_int_run() {
        assert_eq!(scalar(&mut make_vm(), "(t`c2) 0 3"), Value::IntVec(vec![10, 15]));
    }

    #[test]
    fn bracket_index_atom_vs_slice() {
        let mut vm = make_vm();
        assert!(matches!(run_vm("l: 5 6 7 8 9", &mut vm), Ok(EvalResult::Stored)));
        // a single int picks an atom; an int run picks a sub-list
        assert_eq!(scalar(&mut vm, "l[0]"), Value::Int(5));
        assert_eq!(scalar(&mut vm, "l[1 3 4]"), Value::IntVec(vec![6, 8, 9]));
        // works directly on a column expression, and chains
        assert_eq!(scalar(&mut vm, "t`c2[2 1]"), Value::IntVec(vec![30, 20]));
        assert_eq!(scalar(&mut vm, "(t`c2)[0]"), Value::Int(10));
    }

    #[test]
    fn bare_table_name_prints_as_a_table() {
        assert!(matches!(run_vm("t", &mut make_vm()), Ok(EvalResult::Table(_))));
    }

    #[test]
    fn assigning_a_bare_table_name_copies_it() {
        let mut vm = make_vm();
        assert!(matches!(run_vm("t2: t", &mut vm), Ok(EvalResult::Stored)));
        assert!(vm.tables.contains_key("t2"));
    }

    #[test]
    fn take_over_a_bare_table_stays_a_table() {
        assert!(matches!(run_vm("2#t", &mut make_vm()), Ok(EvalResult::Table(_))));
    }

    #[test]
    fn out_of_range_index_errors() {
        assert!(run_vm("(t`c2) 9", &mut make_vm()).is_err());
    }

    #[test]
    fn column_to_value_rejects_nulls() {
        let s = Series::new("x".into(), &[Some(1i64), None, Some(3)]);
        let col = Column::from(s);
        assert!(column_to_value(&col).is_err());
    }
}
