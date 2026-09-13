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
use crate::vm::{Lookup, Vm};
#[cfg(feature = "ipc")]
use crate::vm::EvalResult;

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

/// Require a value-context result to be a concrete scalar / list (not a frame).
fn expect_scalar(v: EvalValue) -> Result<ast::Value, QplError> {
    match v {
        EvalValue::Scalar(s) => Ok(s),
        EvalValue::Frame { .. } => {
            Err(QplError::Runtime("expected a scalar here, got a table".into()))
        }
    }
}

/// A dispatched command's outcome, on the client: a table binds like any
/// other table (`t2: t`); a scalar/`Stored`/`Lazy` result folds to the same
/// `Value` shape the REPL would show for it (see `Expr::Dispatch`).
#[cfg(feature = "ipc")]
fn eval_result_to_value(result: EvalResult) -> Result<EvalValue, QplError> {
    Ok(match result {
        EvalResult::Table(df) => EvalValue::Frame { lf: df.lazy(), lazy: false },
        EvalResult::Scalar(v) => EvalValue::Scalar(v),
        EvalResult::Stored => EvalValue::Scalar(Value::Bool(true)),
        EvalResult::Lazy(text) => EvalValue::Scalar(Value::Str(text)),
    })
}

/// Evaluate a value expression coming from `Instruction::Eval`.
pub fn eval_value(vm: &mut Vm, expr: &Expr) -> Result<EvalValue, QplError> {
    match expr {
        // a bare name: global scalar, lazy binding, or materialised table
        Expr::ColRef(name) => resolve_name(vm, name),

        // `` name`col `` / `` name`c1`c2 `` / a `select …` used as a value
        Expr::Table(te) => eval_table(vm, te),

        // `f[a;b]` / `f[]` — user-function application.
        Expr::Apply { func, args } => apply_function(vm, func, args),

        // `<conn> dispatch <cmd>` / `<conn> async dispatch <cmd>` — ship `cmd`
        // to `conn` for remote evaluation. Value context only, like `Table`
        // above; `command` is already-rendered source text (see `Expr::Dispatch`).
        #[cfg(feature = "ipc")]
        Expr::Dispatch { conn, command, is_async } => {
            let handle = match expect_scalar(eval_value(vm, conn)?)? {
                Value::Handle(id) => id,
                other => return Err(QplError::Runtime(
                    format!("dispatch expects a connection (from hopen), got {other:?}"))),
            };
            let client = vm.connections.get(&handle)
                .ok_or_else(|| QplError::Runtime("dispatch: no such connection (closed?)".into()))?;
            if *is_async {
                let rx = crate::ipc::enqueue(client, command.clone())?;
                let id = vm.next_handle;
                vm.next_handle += 1;
                vm.pending.insert(id, rx);
                Ok(EvalValue::Scalar(Value::Future(id)))
            } else {
                eval_result_to_value(crate::ipc::dispatch_blocking(client, command.clone())?)
            }
        }
        #[cfg(not(feature = "ipc"))]
        Expr::Dispatch { .. } => Err(QplError::Runtime(
            "dispatch requires qpl to be built with `--features ipc`".into())),

        // `f[x]` parsed as an index but `f` names a user function → monadic
        // application. Otherwise falls through to positional indexing below.
        Expr::Index { expr, idx }
            if matches!(expr.as_ref(), Expr::ColRef(n) if vm.lookup_function(n).is_some()) =>
        {
            apply_function(vm, expr, std::slice::from_ref(idx))
        }

        // `f x` — monadic user-function application (juxtaposition).
        Expr::Call { func, args } if vm.lookup_function(func).is_some() => {
            let func = Expr::ColRef(func.clone());
            apply_function(vm, &func, args)
        }

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

        // `conn: hopen 5001` / `hopen "host:5001"`.
        #[cfg(feature = "ipc")]
        Expr::Call { func, args } if func == "hopen" && args.len() == 1 => {
            let addr = match expect_scalar(eval_value(vm, &args[0])?)? {
                Value::Str(s) | Value::Sym(s) => s,
                Value::Int(n) => n.to_string(),
                other => return Err(QplError::Runtime(
                    format!("hopen expects a port or \"host:port\", got {other:?}"))),
            };
            let conn = crate::ipc::hopen(&addr)?;
            let id = vm.next_handle;
            vm.next_handle += 1;
            vm.connections.insert(id, conn);
            Ok(EvalValue::Scalar(Value::Handle(id)))
        }
        // `result: await resp` — resolve a pending `async dispatch` reply.
        #[cfg(feature = "ipc")]
        Expr::Call { func, args } if func == "await" && args.len() == 1 => {
            let id = match expect_scalar(eval_value(vm, &args[0])?)? {
                Value::Future(id) => id,
                other => return Err(QplError::Runtime(format!(
                    "await expects a pending response (from async dispatch), got {other:?}"))),
            };
            let rx = vm.pending.remove(&id).ok_or_else(|| QplError::Runtime(
                "await: no such pending response (already awaited?)".into()))?;
            eval_result_to_value(crate::ipc::await_reply(rx)?)
        }
        #[cfg(not(feature = "ipc"))]
        Expr::Call { func, args } if (func == "hopen" || func == "await") && args.len() == 1 => {
            Err(QplError::Runtime(format!("'{func}' requires qpl to be built with `--features ipc`")))
        }

        // `sum trades`price`, `2 shift px`, `2 round px`, `cumsum px`, …
        Expr::Call { func, args } if (1..=2).contains(&args.len()) => eval_call(vm, func, args),

        // `f64$trades`price` — a cast applied to a column expression, not a
        // literal scalar. `eval_scalar` can only fold true scalars (and its
        // `scalar_cast` only handles atoms), so a cast whose operand resolves
        // to a frame — or, since a one-column expression collapses eagerly
        // (`is_column_select`), to an already-materialised list — is applied
        // here instead, through Polars (the same cast logic the VM uses for
        // `select f64$price from trades`).
        Expr::Cast { target, expr } => match eval_value(vm, expr)? {
            EvalValue::Frame { lf, .. } => {
                let name = first_col_name(&lf)?;
                let casted = vm.build_cast_expr(target, col(name.as_str()), Some(&lf))?;
                let df = lf.select([casted.alias("r")]).collect().map_err(rt)?;
                Ok(EvalValue::Scalar(column_to_value(df.column("r").map_err(rt)?)?))
            }
            EvalValue::Scalar(v) if is_list_value(&v) => {
                let lf = list_to_lazy(v)?;
                let casted = vm.build_cast_expr(target, col("x"), Some(&lf))?;
                let df = lf.select([casted.alias("r")]).collect().map_err(rt)?;
                Ok(EvalValue::Scalar(column_to_value(df.column("r").map_err(rt)?)?))
            }
            EvalValue::Scalar(v) => Ok(EvalValue::Scalar(vm.eval_scalar(&Expr::Cast {
                target: target.clone(),
                expr: Box::new(Expr::Lit(v)),
            })?)),
        },

        // a binary op whose operands may themselves be function calls
        // (`n * fac[n-1]`): reduce both sides to scalars, then reuse the existing
        // operator logic on the two literals.
        Expr::BinOp { left, op, right } => {
            let l = expect_scalar(eval_value(vm, left)?)?;
            let r = expect_scalar(eval_value(vm, right)?)?;
            Ok(EvalValue::Scalar(vm.eval_scalar(&Expr::BinOp {
                left: Box::new(Expr::Lit(l)),
                op: op.clone(),
                right: Box::new(Expr::Lit(r)),
            })?))
        }

        // `?[c1;v1;c2;v2;default]` in value context — a scalar conditional,
        // tree-walked so it short-circuits (needed for conditional recursion in
        // function bodies).
        Expr::Case { branches, default } => {
            for (cond, val) in branches {
                match eval_value(vm, cond)? {
                    EvalValue::Scalar(Value::Bool(true)) => return eval_value(vm, val),
                    EvalValue::Scalar(Value::Bool(false)) => {}
                    _ => {
                        return Err(QplError::Runtime(
                            "a `?[..]` condition must be a boolean scalar in value context".into(),
                        ))
                    }
                }
            }
            eval_value(vm, default)
        }

        // everything else is a pure scalar fold (literals, symbols, binops, casts)
        other => Ok(EvalValue::Scalar(vm.eval_scalar(other)?)),
    }
}

fn resolve_name(vm: &Vm, name: &str) -> Result<EvalValue, QplError> {
    match vm.lookup(name) {
        Some(Lookup::Global(v)) => return Ok(EvalValue::Scalar(v.clone())),
        Some(Lookup::LazyFrame(lf)) => {
            return Ok(EvalValue::Frame { lf: lf.clone(), lazy: true });
        }
        Some(Lookup::Table(df)) => {
            return Ok(EvalValue::Frame { lf: df.clone().lazy(), lazy: false });
        }
        Some(Lookup::Function(_)) => {
            return Err(QplError::Runtime(format!(
                "'{name}' is a function — call it with '{name}[..]'"
            )));
        }
        None => {}
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

/// Apply a user function (`name: {[..] ..}`) to `args`.
///
/// Arguments are evaluated in the *caller* scope, then bound to the parameter
/// names in a child scope that shadows the session: params and any locals the
/// body assigns are discarded on return, so a function cannot mutate outer
/// bindings. The body's leading statements run for their side effects; its final
/// statement (an expression, guaranteed by the compiler) supplies the return.
fn apply_function(vm: &mut Vm, func: &Expr, args: &[Expr]) -> Result<EvalValue, QplError> {
    let name = match func {
        Expr::ColRef(n) => n.clone(),
        _ => {
            return Err(QplError::Runtime(
                "only a named function can be applied (higher-order use is unsupported)".into(),
            ))
        }
    };
    let def = vm
        .lookup_function(&name)
        .cloned()
        .ok_or_else(|| QplError::Runtime(format!("'{name}' is not a function")))?;
    if args.len() != def.params.len() {
        return Err(QplError::Runtime(format!(
            "function '{name}' takes {} argument(s), got {}",
            def.params.len(),
            args.len()
        )));
    }
    if vm.scopes.len() >= crate::vm::MAX_CALL_DEPTH {
        return Err(QplError::Runtime(format!(
            "function recursion too deep (limit {})",
            crate::vm::MAX_CALL_DEPTH
        )));
    }

    // arguments are evaluated in the *caller's* frame, before the callee's is pushed
    let arg_vals = args
        .iter()
        .map(|a| eval_value(vm, a))
        .collect::<Result<Vec<_>, _>>()?;

    // push a fresh call frame; pop it unconditionally on the way out. `lookup`
    // only ever consults the top frame + globals, so the callee cannot see this
    // caller's own locals — lexical, not dynamic, scoping.
    vm.push_scope();
    let result = run_body(vm, &def, arg_vals);
    vm.pop_scope();
    result
}

/// Bind `arg_vals` to `def.params` in the frame [`apply_function`] just pushed,
/// then run the body. Split out so the caller can pop that frame unconditionally
/// afterwards, on every exit path.
fn run_body(
    vm: &mut Vm,
    def: &ast::Function,
    arg_vals: Vec<EvalValue>,
) -> Result<EvalValue, QplError> {
    for (p, v) in def.params.iter().zip(arg_vals) {
        match v {
            EvalValue::Scalar(s) => vm.bind_global(p.clone(), s),
            EvalValue::Frame { lf, lazy } if lazy => vm.bind_lazy(p.clone(), lf),
            EvalValue::Frame { lf, .. } => {
                let df = lf.collect().map_err(rt)?;
                vm.bind_table(p.clone(), df);
            }
        }
    }
    let (last, head) = def.body.split_last().expect("non-empty function body");
    for st in head {
        let prog = crate::compiler::compile(st)?;
        vm.eval(prog)?;
    }
    match last {
        ast::Stmt::SingleVar(e) => eval_value(vm, e),
        ast::Stmt::RetTable(te) => {
            let mut instrs = Vec::new();
            crate::compiler::compile_tbl_expr(te, &mut instrs)?;
            let (lf, lazy) = vm.eval_frame(instrs)?;
            Ok(EvalValue::Frame { lf, lazy })
        }
        _ => Err(QplError::Runtime(
            "a function body must end with an expression".into(),
        )),
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

/// Is this a list-shaped `Value` (as opposed to an atomic scalar)?
fn is_list_value(v: &Value) -> bool {
    matches!(
        v,
        Value::IntVec(_) | Value::FloatVec(_) | Value::StrVec(_)
            | Value::SymVec(_) | Value::BoolVec(_)
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
        v @ (Value::Handle(_) | Value::Future(_)) => {
            return Err(QplError::Runtime(format!("{v:?} is not a list or column expression")))
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

    // --- functions ---

    #[test]
    fn define_and_call_a_scalar_function() {
        let mut vm = make_vm();
        run_vm("add: {[x,y] x+y}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "add[2;3]"), Value::Int(5));
        run_vm("inc: {[x] x+1}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "inc 41"), Value::Int(42)); // `f x`
        assert_eq!(scalar(&mut vm, "inc[41]"), Value::Int(42)); // `f[x]`
    }

    #[test]
    fn function_body_locals_are_scoped_and_do_not_leak() {
        let mut vm = make_vm();
        vm.globals.insert("tmp".into(), Value::Int(99));
        run_vm("sq: {[x] tmp: x*x; tmp}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "sq[9]"), Value::Int(81));
        assert_eq!(vm.globals.get("tmp"), Some(&Value::Int(99))); // outer `tmp` untouched
    }

    #[test]
    fn a_callee_cannot_see_its_caller_s_locals() {
        // lexical, not dynamic, scoping: `callee` has no param/local named `a`,
        // so it must resolve `a` against the true global (99), never against
        // `caller`'s own `a` param — even though `caller` is still on the call
        // stack when `callee` runs.
        let mut vm = make_vm();
        vm.globals.insert("a".into(), Value::Int(99));
        run_vm("callee: {[] a}", &mut vm).unwrap();
        run_vm("caller: {[a] callee[]}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "caller[1]"), Value::Int(99));
    }

    #[test]
    fn conditional_recursion_terminates() {
        let mut vm = make_vm();
        run_vm("fac: {[n] ?[n<2;1;n*fac[n-1]]}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "fac[5]"), Value::Int(120));
    }

    #[test]
    fn a_function_can_return_a_table() {
        let mut vm = make_vm();
        run_vm("q: {[k] select c2 from t where c1 = k}", &mut vm).unwrap();
        match run_vm("q[`a]", &mut vm).expect("run") {
            EvalResult::Table(df) => {
                let got: Vec<i64> =
                    df.column("c2").unwrap().i64().unwrap().into_no_null_iter().collect();
                assert_eq!(got, vec![10, 30]);
            }
            other => panic!("expected a table, got {}", kind(&other)),
        }
    }

    #[test]
    fn wrong_arity_is_a_runtime_error() {
        let mut vm = make_vm();
        run_vm("add: {[x,y] x+y}", &mut vm).unwrap();
        assert!(run_vm("add[1]", &mut vm).is_err());
    }

    #[test]
    fn unbounded_recursion_hits_the_depth_cap_and_unwinds_cleanly() {
        // `apply_function` recurses through the native Rust call stack (compile
        // + eval + eval_value per level), so MAX_CALL_DEPTH levels need more
        // headroom than the default *test-thread* stack (smaller than the main
        // thread the REPL actually runs on) reliably provides — run this one on
        // an explicitly-sized thread rather than weakening the real guard.
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let mut vm = make_vm();
                run_vm("loop: {[n] loop[n+1]}", &mut vm).unwrap();
                let err = run_vm("loop[0]", &mut vm);
                assert!(err.is_err());
                // every pushed call frame was popped again on the way back out
                // through the error, even though none of those calls returned
                // normally
                assert!(vm.scopes.is_empty());
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn call_frame_is_popped_after_an_error_inside_the_body() {
        let mut vm = make_vm();
        run_vm("boom: {[x] x + nope}", &mut vm).unwrap(); // `nope` is undefined
        assert!(run_vm("boom[1]", &mut vm).is_err());
        assert!(vm.scopes.is_empty());
    }

    #[test]
    fn a_bare_function_name_is_a_helpful_error() {
        let mut vm = make_vm();
        run_vm("f: {[x] x}", &mut vm).unwrap();
        let err = match run_vm("f", &mut vm) {
            Err(e) => e,
            Ok(_) => panic!("expected an error"),
        };
        assert!(format!("{err:?}").contains("is a function"));
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
    fn cast_then_reduce_a_column_expression() {
        // regression: `max f64$t`c2` used to error ("not supported in scalar
        // context") because a cast on a column expression fell into the plain
        // scalar folder, which can't resolve a table/column expr underneath it.
        assert_eq!(scalar(&mut make_vm(), "max f64$t`c2"), Value::Float(30.0));
        assert_eq!(scalar(&mut make_vm(), "avg f64$t`c2"), Value::Float(18.75));
    }

    #[test]
    fn cast_a_column_expression_without_reducing() {
        assert_eq!(
            scalar(&mut make_vm(), "f64$t`c2"),
            Value::FloatVec(vec![10.0, 20.0, 30.0, 15.0])
        );
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
