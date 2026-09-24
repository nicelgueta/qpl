//! Runtime resolution for the scalar / value side of the language.
//!
//! `Stmt::ScalarAssign` / `Stmt::SingleVar` compile to a single
//! `Instruction::Eval(expr)`; [`Vm::run_program`](crate::vm::Vm) hands that expr
//! here. Unlike the table pipeline this layer is a plain tree-walk: it resolves a
//! bare name against the session (`globals` → `lazy_frames` → `tables`), turns a
//! one-column `select` / `` table`col `` into a materialised list, reduces a
//! column to a scalar, and slices / indexes lists.

use polars::prelude::*;
use crate::ast::{self, Alias, Expr, SelectStmt, TableExpr, TableSource, Value};
use crate::errors::QplError;
use crate::helpers;
use crate::vm::{Lookup, Vm};
#[cfg(feature = "ipc")]
use crate::vm::EvalResult;

/// The outcome of evaluating a value expression: a concrete scalar / list, or a
/// frame (a bare table name, a multi-column `select`, `n#<table>`).
#[allow(clippy::large_enum_variant)] // mirrors `vm::StackObj` / `EvalResult`
pub enum EvalValue {
    Scalar(ast::Value),
    Frame { lf: LazyFrame, lazy: bool },
    /// Nothing: what `noop` and `while` evaluate to. Prints nothing; `x: <noop>`
    /// and any use as an operand are runtime errors.
    Noop,
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
        EvalValue::Noop => Err(no_value()),
    }
}

fn no_value() -> QplError {
    QplError::Runtime("cannot use a no-op expression as a value".into())
}

/// Evaluate a `?[..]` / `while[..]` test, which must be a boolean atom.
fn eval_test(vm: &mut Vm, test: &Expr, what: &str) -> Result<bool, QplError> {
    match eval_value(vm, test)? {
        EvalValue::Scalar(Value::Bool(b)) => Ok(b),
        _ => Err(QplError::Runtime(format!(
            "a `{what}` condition must be a boolean scalar in value context"
        ))),
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
                let intr = vm.interrupt.clone();
                eval_result_to_value(crate::ipc::dispatch_blocking(client, command.clone(), &intr)?)
            }
        }
        #[cfg(not(feature = "ipc"))]
        Expr::Dispatch { .. } => Err(QplError::Runtime(
            "dispatch requires the `ipc` feature (on by default; this build used `--no-default-features`)".into())),

        // `f[x]` parsed as an index but `f` names a user function or builtin,
        // or is a function literal applied in place (`{[y] y*2}[5]`) →
        // monadic application. Otherwise falls through to positional indexing
        // below. Deliberately only these two shapes: any *other* expression
        // here is a list being indexed, and probing it for a function would
        // mean evaluating it twice.
        Expr::Index { expr, idx }
            if matches!(expr.as_ref(), Expr::ColRef(n) if vm.is_callable(n))
                || matches!(expr.as_ref(), Expr::Lit(Value::Closure(_))) =>
        {
            apply_function(vm, expr, std::slice::from_ref(idx))
        }

        // `enlist <value>` (non-literal operand; a literal is folded by the
        // parser) — the one-element list of an atom. Ahead of the generic
        // call below so nothing can shadow the keyword.
        Expr::Call { func, args } if func == "enlist" && args.len() == 1 => {
            let v = expect_scalar(eval_value(vm, &args[0])?)?;
            v.enlist().map(EvalValue::Scalar).ok_or_else(|| {
                QplError::Runtime("'enlist' expects a single atom, not a list or function".into())
            })
        }

        // `3?6` / `2?10 20 30` — roll (`Parser::binop` lowers `?` to this).
        Expr::Call { func, args } if func == "?" && args.len() == 2 => eval_roll(vm, args),

        // `f x` — monadic user-function/builtin application (juxtaposition).
        Expr::Call { func, args } if vm.is_callable(func) => {
            let func = Expr::ColRef(func.clone());
            apply_function(vm, &func, args)
        }

        // `<n>#<expr>` — head (`n >= 0`) / tail (`n < 0`) slice. `n` is any
        // scalar expression (a literal, a bound global, …), resolved here.
        Expr::Take { n, expr } => {
            let n = match vm.eval_scalar(n)? {
                Value::Int(n) => n,
                other => return Err(QplError::Runtime(format!(
                    "take count must be an int, got {other:?}"
                ))),
            };
            match eval_value(vm, expr)? {
                EvalValue::Frame { lf, lazy } => {
                    let lf = if n >= 0 {
                        lf.limit(n as IdxSize)
                    } else {
                        let k = -n;
                        lf.slice(-k, k as IdxSize)
                    };
                    Ok(EvalValue::Frame { lf, lazy })
                }
                EvalValue::Scalar(list) => Ok(EvalValue::Scalar(take_list(list, n)?)),
                EvalValue::Noop => Err(no_value()),
            }
        }

        // `<list>[<i>]` / `<list>[<i j k>]` / `(<expr>) <i j k>` — positional
        // index. A single int picks an atom; an int run picks a sub-list.
        Expr::Index { expr, idx } => {
            let list = to_list(vm, expr)?;
            let (idxs, atom) = match vm.eval_scalar(idx)? {
                Value::Int(n) => (vec![n], true),
                v @ Value::IntVec(_) => (v.as_vec().unwrap().1.i64().map_err(rt)?.into_no_null_iter().collect(), false),
                other => {
                    return Err(QplError::Runtime(format!(
                        "index must be an int or int vector, got {other:?}"
                    )))
                }
            };
            let picked = index_list(list, &idxs)?;
            Ok(EvalValue::Scalar(if atom { scalarise(picked)? } else { picked }))
        }

        // `conn: hopen 5001` / `hopen "host:5001"` — a read-only handle (the
        // default). `` `w!hopen 5001 `` (parsed as `Call { func: "whopen", .. }`,
        // see `Parser::parse_expr_inner`) opens a write handle instead.
        #[cfg(feature = "ipc")]
        Expr::Call { func, args } if (func == "hopen" || func == "whopen") && args.len() == 1 => {
            let addr = match expect_scalar(eval_value(vm, &args[0])?)? {
                Value::Str(s) | Value::Sym(s) => s,
                Value::Int(n) => n.to_string(),
                other => return Err(QplError::Runtime(
                    format!("hopen expects a port or \"host:port\", got {other:?}"))),
            };
            let mode = if func == "whopen" {
                crate::ipc::HandleMode::Write
            } else {
                crate::ipc::HandleMode::Read
            };
            let conn = crate::ipc::hopen(&addr, mode)?;
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
            let intr = vm.interrupt.clone();
            let rx = vm.pending.get(&id).ok_or_else(|| QplError::Runtime(
                "await: no such pending response (already awaited?)".into()))?;
            let reply = crate::ipc::await_reply(rx, &intr);
            // an interrupted await leaves the future pending, so it can be awaited again
            if !matches!(reply, Err(QplError::Interrupted)) {
                vm.pending.remove(&id);
            }
            eval_result_to_value(reply?)
        }
        #[cfg(not(feature = "ipc"))]
        Expr::Call { func, args }
            if (func == "hopen" || func == "whopen" || func == "await") && args.len() == 1 =>
        {
            Err(QplError::Runtime(format!(
                "'{func}' requires the `ipc` feature (on by default; this build used `--no-default-features`)"
            )))
        }
        // `log[a b c]` / `log[a;b;c]` — the bracket-scoped spelling of the
        // bareword `log a b c` stdout-write (see `Parser::parse_noun`), usable
        // anywhere an expression is (composed inside a larger expression, or
        // as a statement in a function body), unlike the bareword form which
        // `repl::eval_line` only recognises as a whole top-level REPL/script
        // line. Any argument count, including zero (`log[]`, a blank line).
        Expr::Call { func, args } if func == "log" => Ok(EvalValue::Scalar(Value::Str(eval_log(vm, args)?))),

        // `til 5` / `10 til 15` — a range list constructor, not a reduction
        // over an existing list, so this is intercepted ahead of the generic
        // `eval_call` below (which assumes `args[0]` is already a list/frame).
        Expr::Call { func, args } if func == "til" => eval_til(vm, args),

        // `zip `k1`k2!v1 v2` — build a table from a dict of named lists.
        Expr::Call { func, args } if func == "zip" && args.len() == 1 => eval_zip(vm, &args[0]),

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
            EvalValue::Noop => Err(no_value()),
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

        // `<list-expr> where <predicate>...` — elementwise filter on a list.
        Expr::ListWhere { list, where_ } => eval_list_where(vm, list, where_),

        // `?[c1;v1;c2;v2;default]` in value context. A boolean *atom* condition
        // is tree-walked so only the taken branch runs (needed for conditional
        // recursion in function bodies). A boolean *vector* condition switches
        // to the elementwise form — see `eval_case_vector`.
        Expr::Case { branches, default } => {
            for (k, (cond, val)) in branches.iter().enumerate() {
                match eval_value(vm, cond)? {
                    EvalValue::Scalar(Value::Bool(true)) => return eval_value(vm, val),
                    EvalValue::Scalar(Value::Bool(false)) => {}
                    EvalValue::Scalar(mask @ Value::BoolVec(_)) => {
                        return eval_case_vector(vm, mask, &branches[k..], default);
                    }
                    _ => {
                        return Err(QplError::Runtime(
                            "a `?[..]` condition must be a boolean scalar or vector in value context".into(),
                        ))
                    }
                }
            }
            eval_value(vm, default)
        }

        // `noop` — nothing; `while[test; s1; ..; sn]` — run the statements in
        // the current scope (no frame is pushed) while `test` holds. Its
        // result is always noop.
        Expr::Noop => Ok(EvalValue::Noop),
        Expr::While { cond, body } => {
            while eval_test(vm, cond, "while")? {
                vm.interrupt.check()?;
                for st in body {
                    exec_stmt(vm, st)?;
                }
            }
            Ok(EvalValue::Noop)
        }

        // everything else is a pure scalar fold (literals, symbols, binops, casts)
        other => Ok(EvalValue::Scalar(vm.eval_scalar(other)?)),
    }
}

/// The elementwise form of `?[..]` outside a select: `first` is the boolean
/// vector condition of `branches[0]`, and every other condition, branch and the
/// default is evaluated (there is no short-circuit — each element picks its own
/// branch). An atom operand broadcasts; a vector operand must be exactly as long
/// as the condition, else it is a runtime error. The first true condition wins
/// per element, like the atom form; the result has the condition's length.
fn eval_case_vector(
    vm: &mut Vm,
    first: Value,
    branches: &[(Expr, Expr)],
    default: &Expr,
) -> Result<EvalValue, QplError> {
    let n = first.as_vec().map(|(_, s)| s.len()).expect("a BoolVec");
    // (is text, is symbol) per value operand, to reject a text/non-text mix
    // (Polars would silently stringify the numbers) and to keep an all-symbol
    // result a symbol vector rather than strings.
    let mut kinds: Vec<(bool, bool)> = Vec::new();
    let mut operand = |vm: &mut Vm, e: &Expr, what: &str, bool_only: bool| -> Result<polars::prelude::Expr, QplError> {
        let v = expect_scalar(eval_value(vm, e)?)?;
        if bool_only && !matches!(v, Value::Bool(_) | Value::BoolVec(_)) {
            return Err(QplError::Runtime(
                "a `?[..]` condition must be a boolean scalar or vector in value context".into(),
            ));
        }
        if let Some((_, s)) = v.as_vec()
            && s.len() != n
        {
            return Err(QplError::Runtime(format!(
                "`?[..]` {what} has length {}, expected {n} (the length of the condition)",
                s.len()
            )));
        }
        if !bool_only {
            kinds.push((
                matches!(v, Value::Str(_) | Value::Sym(_) | Value::StrVec(_) | Value::SymVec(_)),
                matches!(v, Value::Sym(_) | Value::SymVec(_)),
            ));
        }
        crate::vm::ast_val_to_expr(v)
    };

    let mut arms = Vec::with_capacity(branches.len());
    for (k, (cond, val)) in branches.iter().enumerate() {
        let c = if k == 0 {
            crate::vm::ast_val_to_expr(first.clone())?
        } else {
            operand(vm, cond, "condition", true)?
        };
        arms.push((c, operand(vm, val, "branch", false)?));
    }
    let mut acc = operand(vm, default, "default", false)?;
    for (c, v) in arms.into_iter().rev() {
        acc = when(c).then(v).otherwise(acc);
    }
    if kinds.iter().any(|k| k.0) && !kinds.iter().all(|k| k.0) {
        return Err(QplError::Runtime(
            "`?[..]` branches mix text and non-text values".into(),
        ));
    }
    let all_sym = kinds.iter().all(|k| k.1);
    let df = df!("_" => [0i64]).map_err(rt)?.lazy().select([acc.alias("r")]).collect().map_err(rt)?;
    if df.height() != n {
        return Err(QplError::Runtime(format!(
            "`?[..]` produced {} value(s), expected {n} (the length of the condition)", df.height()
        )));
    }
    let out = column_to_value(df.column("r").map_err(rt)?)?;
    Ok(EvalValue::Scalar(match out {
        Value::StrVec(s) if all_sym => Value::SymVec(s),
        other => other,
    }))
}

fn resolve_name(vm: &mut Vm, name: &str) -> Result<EvalValue, QplError> {
    // a niladic function/builtin resolves like any other bare name — called
    // with no arguments; anything with params still needs `name[..]`.
    if let Some(v) = call_niladic(vm, name)? {
        return Ok(v);
    }
    match vm.lookup(name) {
        Some(Lookup::Global(v)) => return Ok(EvalValue::Scalar(v.clone())),
        Some(Lookup::LazyFrame(lf)) => {
            return Ok(EvalValue::Frame { lf: lf.clone(), lazy: true });
        }
        Some(Lookup::Table(df)) => {
            return Ok(EvalValue::Frame { lf: df.clone().lazy(), lazy: false });
        }
        // a user function *is* a value and falls out of `Lookup::Global` above;
        // a builtin is not, so naming one bare (and not niladic) is an error.
        Some(Lookup::Builtin(_)) => {
            return Err(QplError::Runtime(format!(
                "'{name}' is a built-in function — call it with '{name}[..]'"
            )));
        }
        None => {}
    }
    Err(QplError::Runtime(format!(
        "undefined name '{name}' (not a variable, table or lazy frame)"
    )))
}

/// Resolves `name` when it names a niladic (zero-parameter) function value or
/// builtin, calling it with no arguments. Returns `Ok(None)` when `name`
/// resolves to anything else (a variable, table, or a function/builtin that
/// still takes parameters) so the caller falls back to its own handling —
/// used both by [`resolve_name`] (bare-name value context) and by
/// [`crate::vm::Vm`]'s `PushColRef` (bare name inside a column expression).
pub(crate) fn call_niladic(vm: &mut Vm, name: &str) -> Result<Option<EvalValue>, QplError> {
    match vm.lookup(name) {
        Some(Lookup::Builtin(b)) if b.arity == 0 => {
            let b = *b; // ends the borrow of `vm`
            Ok(Some(EvalValue::Scalar(b.call(&[])?)))
        }
        Some(Lookup::Global(Value::Closure(f))) if f.params.is_empty() => {
            let def = f.clone();
            vm.interrupt.check()?;
            if vm.scopes.len() >= crate::vm::MAX_CALL_DEPTH {
                return Err(QplError::Runtime(format!(
                    "function recursion too deep (limit {})",
                    crate::vm::MAX_CALL_DEPTH
                )));
            }
            let ns = vm.resolve_function_ns(name);
            vm.push_scope();
            vm.scopes.last_mut().expect("just pushed").current_ns = ns;
            let result = run_body(vm, &def, vec![]);
            vm.pop_scope();
            Ok(Some(result?))
        }
        _ => Ok(None),
    }
}

fn eval_table(vm: &mut Vm, te: &TableExpr) -> Result<EvalValue, QplError> {
    let mut instrs = Vec::new();
    crate::compiler::compile_tbl_expr(te, &mut instrs)?;
    let (lf, lazy) = vm.eval_frame(instrs)?;
    if is_column_select(te) {
        let df = lf.collect().map_err(rt)?;
        vm.interrupt.check()?;
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

/// `<list-expr> where <predicate>...` — filter a list value elementwise
/// (`nums where x > 10`; `x` is the current element, a plain column
/// reference — see below). `list` is evaluated to a concrete list `Value`,
/// then this reuses the exact same row-filter machinery a table's `where`
/// clause uses (`compile_select`'s `FrameExpr(Filter(n))`): the list's
/// one-column (named `x`, see `list_to_lazy`) materialisation is bound under
/// a throwaway lazy-frame name no user source could ever spell (a NUL byte —
/// the lexer never produces one), a `select x from <tmp> where <predicates>`
/// is compiled and run against it, and the binding is dropped again.
fn eval_list_where(vm: &mut Vm, list: &Expr, where_: &[Expr]) -> Result<EvalValue, QplError> {
    const TMP: &str = "\0qpl_list_where";
    let list_val = expect_scalar(eval_value(vm, list)?)?;
    if !is_list_value(&list_val) {
        return Err(QplError::Runtime(format!(
            "'where' needs a list on the left, got {list_val:?}"
        )));
    }
    vm.lazy_frames.insert(TMP.into(), list_to_lazy(list_val)?);
    let sel = TableExpr::Select(SelectStmt {
        cols: vec![Alias { name: None, expr: Expr::ColRef("x".into()) }],
        from: Box::new(TableExpr::Source(TableSource::InMem(TMP.into()))),
        by: None,
        where_: Some(where_.to_vec()),
        order: None,
        join: None,
        update: false,
        delete: false,
    });
    let result = (|| {
        let mut instrs = Vec::new();
        crate::compiler::compile_tbl_expr(&sel, &mut instrs)?;
        let (lf, _lazy) = vm.eval_frame(instrs)?;
        let df = lf.collect().map_err(rt)?;
        let col = df
            .select_at_idx(0)
            .ok_or_else(|| QplError::Runtime("'where' produced no columns".into()))?;
        column_to_value(col)
    })();
    vm.lazy_frames.remove(TMP);
    Ok(EvalValue::Scalar(result?))
}

/// Render each of `args` via `fmt_log_val` and concatenate, then write the
/// result through `Vm::emit` (mirrored to the stdout log). Shared by the
/// `log[..]` bracket-call arm above and by `repl::eval_line`'s bareword
/// `log a b c` handling, so both spellings go through one implementation.
/// Returns the text written, so a bracketed call composes as an ordinary
/// expression value (mirrors kdb's `1 x` returning what it wrote).
pub(crate) fn eval_log(vm: &mut Vm, args: &[Expr]) -> Result<String, QplError> {
    let mut text = String::new();
    for a in args {
        let val = expect_scalar(eval_value(vm, a)?)?;
        text.push_str(&crate::repl::fmt_log_val(&val));
    }
    vm.emit(&text);
    Ok(text)
}

/// `til n` → `0 .. n-1`; `lo til hi` → `lo .. hi-1`. The dyadic form is parsed
/// through the shared "param verb value" infix grammar (parser.rs), whose
/// convention is `args: [value, param]` — so here that's `[hi, lo]`, not
/// `[lo, hi]`.
fn eval_til(vm: &mut Vm, args: &[Expr]) -> Result<EvalValue, QplError> {
    let int_of = |vm: &Vm, e: &Expr| -> Result<i64, QplError> {
        match vm.eval_scalar(e)? {
            Value::Int(n) => Ok(n),
            other => Err(QplError::Runtime(format!("'til' expects an integer, got {other:?}"))),
        }
    };
    let (lo, hi) = match args {
        [n] => (0i64, int_of(vm, n)?),
        [hi, lo] => (int_of(vm, lo)?, int_of(vm, hi)?),
        _ => return Err(QplError::Runtime("'til' takes 1 or 2 arguments".into())),
    };
    if hi < lo {
        return Err(QplError::Runtime(format!(
            "'til': upper bound {hi} is less than lower bound {lo}"
        )));
    }
    Ok(EvalValue::Scalar(ast::int_vec((lo..hi).collect())))
}

/// `<n>?<x>` — `n` random items drawn with replacement. `args` is `[x, n]`
/// (right operand first, like `til`'s). `x` is an int `hi` (ints in `0..hi`),
/// a float `hi` (floats in `[0, hi)`) or a list of any kind (random elements).
fn eval_roll(vm: &mut Vm, args: &[Expr]) -> Result<EvalValue, QplError> {
    let n = match expect_scalar(eval_value(vm, &args[1])?)? {
        Value::Int(n) if n >= 0 => n as usize,
        other => return Err(QplError::Runtime(format!(
            "'?' expects a non-negative int count on the left, got {other:?}"
        ))),
    };
    let rolled = match to_list(vm, &args[0])? {
        Value::Int(hi) if hi > 0 => {
            ast::int_vec((0..n).map(|_| helpers::rand_below(hi as u64) as i64).collect())
        }
        Value::Int(hi) => return Err(QplError::Runtime(format!(
            "'?' needs a positive upper bound to roll ints in, got {hi}"
        ))),
        Value::Float(hi) => ast::float_vec((0..n).map(|_| hi * helpers::rand_unit()).collect()),
        list if is_list_value(&list) => {
            let len = list.as_vec().expect("checked by is_list_value").1.len();
            if len == 0 && n > 0 {
                return Err(QplError::Runtime("'?' cannot roll from an empty list".into()));
            }
            let idx: Vec<i64> = (0..n).map(|_| helpers::rand_below(len as u64) as i64).collect();
            index_list(list, &idx)?
        }
        other => return Err(QplError::Runtime(format!(
            "'?' expects an int, float or list on the right, got {other:?}"
        ))),
    };
    Ok(EvalValue::Scalar(rolled))
}

/// One `zip` column: the dict value evaluated to a list, as a `Series` with
/// the dtype the table column should have.
fn zip_column(vm: &mut Vm, name: &str, expr: &Expr) -> Result<Series, QplError> {
    // A cast at the top of the value (`i8$n ? 100`, `` `$syms ``, `f32$xs`) is
    // applied to the column directly: a list `Value` only holds i64 / f64 /
    // str / bool / temporal, so going through one would lose the narrower
    // integer / float widths and the categorical.
    if let Expr::Cast { target, expr: inner } = expr {
        let list = to_list(vm, inner)?;
        if !is_list_value(&list) {
            return Err(QplError::Runtime(format!("'zip' column '{name}' is not a list: {list:?}")));
        }
        let lf = list_to_lazy(list)?;
        let casted = vm.build_cast_expr(target, col("x"), Some(&lf))?;
        let df = lf.select([casted.alias("x")]).collect().map_err(rt)?;
        return Ok(df.column("x").map_err(rt)?.as_materialized_series().clone());
    }
    let list_val = expect_scalar(eval_value(vm, expr)?)?;
    let (kind, s) = list_val.as_vec().ok_or_else(|| {
        QplError::Runtime(format!("'zip' column '{name}' is not a list: {list_val:?}"))
    })?;
    // a temporal list holds kdb offsets (days / ns since 2000); the column
    // must carry the native Polars dtype, so give it the same conversion a
    // literal gets in any query (`ast_val_to_expr`)
    if matches!(kind, ast::VecKind::Date | ast::VecKind::Month | ast::VecKind::Time
        | ast::VecKind::Minute | ast::VecKind::Second | ast::VecKind::Timestamp | ast::VecKind::Timespan)
    {
        let df = list_to_lazy(list_val.clone())?.collect().map_err(rt)?;
        return Ok(df.column("x").map_err(rt)?.as_materialized_series().clone());
    }
    Ok(s.clone())
}

/// `zip `k1`k2!v1 v2` — evaluate each dict value to a list and assemble them,
/// in order, into a table (kdb's `flip` of a column dict, under a friendlier
/// name). Every value must be list-shaped and the same length.
fn eval_zip(vm: &mut Vm, dict_expr: &Expr) -> Result<EvalValue, QplError> {
    let pairs = match dict_expr {
        Expr::Dict(pairs) => pairs,
        other => {
            return Err(QplError::Runtime(format!(
                "'zip' expects a dict (`` `col1`col2!v1 v2 ``), got {other:?}"
            )))
        }
    };
    if pairs.is_empty() {
        return Err(QplError::Runtime("'zip' needs at least one column".into()));
    }
    let mut len = None;
    let mut columns = Vec::with_capacity(pairs.len());
    for (name, expr) in pairs {
        let s = &zip_column(vm, name, expr)?;
        match len {
            None => len = Some(s.len()),
            Some(l) if l != s.len() => {
                return Err(QplError::Runtime(format!(
                    "'zip' columns have mismatched lengths: '{name}' has {}, expected {l}",
                    s.len()
                )))
            }
            _ => {}
        }
        let mut s = s.clone();
        s.rename(name.as_str().into());
        columns.push(Column::from(s));
    }
    let df = DataFrame::new(len.expect("checked non-empty above"), columns).map_err(rt)?;
    Ok(EvalValue::Frame { lf: df.lazy(), lazy: false })
}

/// Coerce a value expression to a concrete list: materialise a single-column
/// frame, reject a multi-column one.
fn to_list(vm: &mut Vm, expr: &Expr) -> Result<Value, QplError> {
    match eval_value(vm, expr)? {
        EvalValue::Noop => Err(no_value()),
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

/// Resolve a column verb's source to a lazy frame without forcing an eager
/// materialise first when it's table-shaped. `eval_table`'s "one-column
/// select collapses to a list" step exists for genuine value-context list use
/// (indexing, arithmetic, `where`, …); `eval_call` is about to `.select()`
/// off it anyway, so materialising first would only buy a redundant collect
/// and a round trip through an owned buffer — the final `column_to_value` on
/// the *applied* column below still catches a null result (e.g. `max` of an
/// all-null column), it just no longer blanket-rejects a source column that
/// has *some* nulls a reducer would ignore regardless (`max`/`sum`/…, same as
/// plain Polars).
fn eval_call_source(vm: &mut Vm, expr: &Expr) -> Result<LazyFrame, QplError> {
    if let Expr::Table(te) = expr {
        let mut instrs = Vec::new();
        crate::compiler::compile_tbl_expr(te, &mut instrs)?;
        let (lf, _) = vm.eval_frame(instrs)?;
        return Ok(lf);
    }
    match eval_value(vm, expr)? {
        EvalValue::Frame { lf, .. } => Ok(lf),
        EvalValue::Scalar(list) => list_to_lazy(list),
        EvalValue::Noop => Err(no_value()),
    }
}

/// Apply a monadic / dyadic column verb to a column expression or list. A
/// reducing verb on a single argument yields a scalar; anything else yields a
/// new list.
fn eval_call(vm: &mut Vm, func: &str, args: &[Expr]) -> Result<EvalValue, QplError> {
    let lf = eval_call_source(vm, &args[0])?;
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

/// Apply a function or a builtin to `args`.
///
/// `func` is any expression that evaluates to a `Value::Closure` — usually a
/// bare name, but equally a parameter holding a function, a literal
/// `{[..] ..}`, or the result of another call, which is what makes higher-order
/// use (`apply: {[f;x] f[x]}`) work. A bare name is resolved directly rather
/// than evaluated, so a *niladic* function named as the call target isn't
/// invoked twice (`resolve_name` would have called it on sight).
///
/// Arguments are evaluated in the *caller* scope, then bound to the parameter
/// names in a child scope that shadows the session: params and any locals the
/// body assigns are discarded on return, so a function cannot mutate outer
/// bindings. Nothing is captured from the defining scope. The body's leading
/// statements run for their side effects; its final statement (an expression,
/// guaranteed by the parser) supplies the return.
fn apply_function(vm: &mut Vm, func: &Expr, args: &[Expr]) -> Result<EvalValue, QplError> {
    // `Some` only for a bare-name target — what error messages and namespace
    // resolution key off. An anonymous target has neither.
    let name = match func {
        Expr::ColRef(n) => Some(n.clone()),
        _ => None,
    };
    if let Some(b) = name.as_ref().and_then(|n| vm.builtins.get(n).copied()) {
        let name = name.expect("builtin was looked up by name");
        if args.len() != b.arity {
            return Err(QplError::Runtime(format!(
                "'{name}' takes {} argument(s), got {}", b.arity, args.len()
            )));
        }
        let arg_vals = args.iter()
            .map(|a| expect_scalar(eval_value(vm, a)?))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(EvalValue::Scalar(b.call(&arg_vals)?));
    }
    let def = match &name {
        Some(n) => vm
            .lookup_closure(n)
            .ok_or_else(|| QplError::Runtime(format!("'{n}' is not a function")))?,
        None => match expect_scalar(eval_value(vm, func)?)? {
            Value::Closure(f) => f,
            other => {
                return Err(QplError::Runtime(format!(
                    "cannot apply {other:?} — not a function"
                )))
            }
        },
    };
    let label = name.clone().unwrap_or_else(|| "{[..] ..}".to_string());
    if args.len() != def.params.len() {
        return Err(QplError::Runtime(format!(
            "function '{label}' takes {} argument(s), got {}",
            def.params.len(),
            args.len()
        )));
    }
    vm.interrupt.check()?;
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
    // resolved against the *caller's* active scope, before the callee's own is pushed
    let ns = name.as_deref().and_then(|n| vm.resolve_function_ns(n));

    // push a fresh call frame; pop it unconditionally on the way out. `lookup`
    // only ever consults the top frame + globals, so the callee cannot see this
    // caller's own locals — lexical, not dynamic, scoping.
    vm.push_scope();
    vm.scopes.last_mut().expect("just pushed").current_ns = ns;
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
            EvalValue::Scalar(s) => vm.bind_global(p.clone(), s)?,
            EvalValue::Frame { lf, lazy } if lazy => vm.bind_lazy(p.clone(), lf)?,
            EvalValue::Frame { lf, .. } => {
                let df = lf.collect().map_err(rt)?;
                vm.bind_table(p.clone(), df)?;
            }
            EvalValue::Noop => return Err(no_value()),
        }
    }
    let (last, head) = def.body.split_last().expect("non-empty function body");
    for st in head {
        exec_stmt(vm, st)?;
    }
    match last {
        ast::Stmt::SingleVar(_) | ast::Stmt::RetTable(_) => exec_stmt(vm, last),
        _ => Err(QplError::Runtime(
            "a function body must end with an expression".into(),
        )),
    }
}

/// Run one statement in the *current* scope: an expression yields its value
/// (a table statement its frame); an assignment binds and yields `Noop`.
fn exec_stmt(vm: &mut Vm, st: &ast::Stmt) -> Result<EvalValue, QplError> {
    vm.interrupt.check()?;
    match st {
        ast::Stmt::SingleVar(e) => eval_value(vm, e),
        ast::Stmt::RetTable(te) => {
            let mut instrs = Vec::new();
            crate::compiler::compile_tbl_expr(te, &mut instrs)?;
            let (lf, lazy) = vm.eval_frame(instrs)?;
            Ok(EvalValue::Frame { lf, lazy })
        }
        other => {
            vm.eval(crate::compiler::compile(other)?)?;
            Ok(EvalValue::Noop)
        }
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
    v.as_vec().is_some()
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
        return Ok(ast::sym_vec(ca.iter().flatten().map(str::to_owned).collect()));
    }
    Ok(match dt {
        DataType::Boolean => {
            ast::bool_vec(col.bool().map_err(rt)?.iter().flatten().collect())
        }
        DataType::String => ast::str_vec(
            col.str().map_err(rt)?.iter().flatten().map(str::to_owned).collect(),
        ),
        DataType::Float32 | DataType::Float64 => {
            let c = col.cast(&DataType::Float64).map_err(rt)?;
            ast::float_vec(c.f64().map_err(rt)?.into_no_null_iter().collect())
        }
        DataType::Date => {
            let c = col.cast(&DataType::Int32).map_err(rt)?;
            ast::date_vec(
                c.i32().map_err(rt)?.into_no_null_iter()
                    .map(|d| d - crate::temporal::DAYS_2000_TO_1970).collect(),
            )
        }
        // normalise to nanoseconds first — a column loaded from a file may be
        // ms / us resolution, not ns
        DataType::Datetime(_, _) => {
            let c = col
                .cast(&DataType::Datetime(TimeUnit::Nanoseconds, None)).map_err(rt)?
                .cast(&DataType::Int64).map_err(rt)?;
            ast::timestamp_vec(
                c.i64().map_err(rt)?.into_no_null_iter()
                    .map(|n| n - crate::temporal::NS_2000_TO_1970).collect(),
            )
        }
        DataType::Duration(_) => {
            let c = col
                .cast(&DataType::Duration(TimeUnit::Nanoseconds)).map_err(rt)?
                .cast(&DataType::Int64).map_err(rt)?;
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
            )))
        }
    })
}

/// A one-element list collapses to the corresponding scalar.
fn scalarise(v: Value) -> Result<Value, QplError> {
    let (kind, s) = v
        .as_vec()
        .ok_or_else(|| QplError::Runtime(format!("expected a single value from the reduction, got {v:?}")))?;
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

fn take_list(v: Value, n: i64) -> Result<Value, QplError> {
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

fn index_list(v: Value, idx: &[i64]) -> Result<Value, QplError> {
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

    fn strs(v: &Value) -> Vec<String> {
        v.vec_strings().expect("a string/symbol list")
    }

    #[test]
    fn juxtaposed_strings_are_a_str_vec() {
        let mut vm = make_vm();
        assert_eq!(scalar(&mut vm, r#"("string1" "string2")"#), ast::str_vec(vec!["string1".into(), "string2".into()]));
        assert_eq!(scalar(&mut vm, r#""a" "b" "c""#), ast::str_vec(vec!["a".into(), "b".into(), "c".into()]));
        // a string vector composes as a value: bindable, indexable
        scalar_or_stored(&mut vm, r#"v: ("a" "b" "c")"#);
        assert_eq!(scalar(&mut vm, "v[1]"), Value::Str("b".into()));
    }

    fn scalar_or_stored(vm: &mut Vm, src: &str) {
        run_vm(src, vm).expect("run");
    }

    #[test]
    fn enlist_makes_a_one_element_vector_of_any_atom() {
        let mut vm = make_vm();
        assert_eq!(scalar(&mut vm, "enlist 23"), ast::int_vec(vec![23]));
        assert_eq!(scalar(&mut vm, "enlist 1.5"), ast::float_vec(vec![1.5]));
        assert_eq!(scalar(&mut vm, "enlist 1b"), ast::bool_vec(vec![true]));
        assert_eq!(scalar(&mut vm, "enlist `a"), ast::sym_vec(vec!["a".into()]));
        // a string is one atom, not a list of characters
        assert_eq!(scalar(&mut vm, r#"enlist "hello""#), ast::str_vec(vec!["hello".into()]));
        assert_eq!(scalar(&mut vm, "enlist 2024.03.15"), ast::date_vec(vec![8840]));
    }

    #[test]
    fn enlist_of_a_variable_or_expression_is_evaluated_at_run_time() {
        let mut vm = make_vm();
        scalar_or_stored(&mut vm, "n: 5");
        assert_eq!(scalar(&mut vm, "enlist n"), ast::int_vec(vec![5]));
        assert_eq!(scalar(&mut vm, "enlist n + 1"), ast::int_vec(vec![6]));
        assert_eq!(scalar(&mut vm, "enlist first t`c2"), ast::int_vec(vec![10]));
    }

    #[test]
    fn enlist_rejects_a_list() {
        assert!(run_vm("enlist 1 2 3", &mut make_vm()).is_err());
    }

    #[test]
    fn roll_ints_draws_n_values_below_the_bound() {
        let mut vm = make_vm();
        for _ in 0..20 {
            let v = scalar(&mut vm, "50?6");
            let got: Vec<i64> = v.as_vec().unwrap().1.i64().unwrap().into_no_null_iter().collect();
            assert_eq!(got.len(), 50);
            assert!(got.iter().all(|n| (0..6).contains(n)), "{got:?}");
        }
    }

    #[test]
    fn roll_actually_varies() {
        let mut vm = make_vm();
        let v = scalar(&mut vm, "200?1000000");
        let got: Vec<i64> = v.as_vec().unwrap().1.i64().unwrap().into_no_null_iter().collect();
        let distinct: std::collections::HashSet<_> = got.iter().collect();
        assert!(distinct.len() > 150, "200 draws from 1e6 had only {} distinct values", distinct.len());
    }

    #[test]
    fn roll_from_a_list_picks_its_elements_with_replacement() {
        let mut vm = make_vm();
        let v = scalar(&mut vm, "2 ? 10 20 30 40");
        assert_eq!(v.as_vec().unwrap().1.len(), 2);
        let v = scalar(&mut vm, "30 ? 10 20");
        let got: Vec<i64> = v.as_vec().unwrap().1.i64().unwrap().into_no_null_iter().collect();
        assert_eq!(got.len(), 30, "more draws than elements: replacement");
        assert!(got.iter().all(|n| *n == 10 || *n == 20));
        // works for any list kind, and for a column
        let v = scalar(&mut vm, "20 ? `a`b`c");
        assert!(matches!(v, Value::SymVec(_)) && strs(&v).iter().all(|s| ["a", "b", "c"].contains(&s.as_str())));
        let v = scalar(&mut vm, r#"20 ? ("x" "y")"#);
        assert!(matches!(v, Value::StrVec(_)) && strs(&v).iter().all(|s| s == "x" || s == "y"));
        let v = scalar(&mut vm, "20 ? t`c2");
        assert!(v.as_vec().unwrap().1.i64().unwrap().into_no_null_iter().all(|n| [10, 20, 30, 15].contains(&n)));
    }

    #[test]
    fn roll_floats_and_zero_count() {
        let mut vm = make_vm();
        let v = scalar(&mut vm, "50?2.5");
        let got: Vec<f64> = v.as_vec().unwrap().1.f64().unwrap().into_no_null_iter().collect();
        assert_eq!(got.len(), 50);
        assert!(got.iter().all(|f| (0.0..2.5).contains(f)));
        assert_eq!(scalar(&mut vm, "0?5").as_vec().unwrap().1.len(), 0);
    }

    #[test]
    fn roll_composes_with_other_expressions() {
        let mut vm = make_vm();
        scalar_or_stored(&mut vm, "n: 4");
        assert_eq!(scalar(&mut vm, "count n?100"), Value::Int(4));
        let v = scalar(&mut vm, "100 + 3?1");
        assert_eq!(v, ast::int_vec(vec![100, 100, 100]));
    }

    #[test]
    fn roll_rejects_bad_operands() {
        let mut vm = make_vm();
        for bad in ["-1?5", "3?0", "3?`a", "1.5?5"] {
            assert!(run_vm(bad, &mut vm).is_err(), "{bad} should be an error");
        }
    }

    #[test]
    fn zip_gives_temporal_lists_their_native_dtype() {
        let mut vm = make_vm();
        scalar_or_stored(&mut vm, "ts: `timestamp$1700000000000000000 1700086400123456789");
        scalar_or_stored(&mut vm, "t2: zip `ts`day`tm`span!(ts) (`date$ts) (`time$ts) (`timespan$5 6)");
        let df = &vm.tables["t2"];
        let dtypes: Vec<_> = df.dtypes().iter().map(|d| d.to_string()).collect();
        assert_eq!(dtypes, ["datetime[ns]", "date", "time", "duration[ns]"]);
        assert_eq!(df.null_count().sum_horizontal(NullStrategy::Ignore).unwrap().unwrap().u32().unwrap().get(0), Some(0));
        // and the values survive the round trip through the column
        assert_eq!(scalar(&mut vm, "t2`day"), scalar(&mut vm, "`date$ts"));
    }

    #[test]
    fn zip_applies_a_top_level_cast_to_the_column_keeping_its_dtype() {
        let mut vm = make_vm();
        scalar_or_stored(&mut vm, "t3: zip `a`b`c`d`e!(i8$10 20 30) (i32$1 2 3) (f32$3 ? 1.0) (`$`x`y`x) (`timestamp$1700000000000000000 1700000000000000001 1700000000000000002)");
        let dtypes: Vec<_> = vm.tables["t3"].dtypes().iter().map(|d| d.to_string()).collect();
        assert_eq!(dtypes, ["i8", "i32", "f32", "cat", "datetime[ns]"]);
        // an operand may itself be a roll
        scalar_or_stored(&mut vm, "t4: zip `a!(i8$5 ? 100)");
        assert_eq!(vm.tables["t4"].dtypes()[0].to_string(), "i8");
        assert_eq!(vm.tables["t4"].height(), 5);
    }

    #[test]
    fn table_col_materialises_to_a_list() {
        assert_eq!(scalar(&mut make_vm(), "t`c3"), ast::float_vec(vec![1.0, 2.0, 3.0, 4.0]));
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
    fn function_body_tolerates_a_stray_extra_semicolon() {
        // regression: a doubled `;;` between statements (an easy typo, e.g.
        // from a trailing `;` left behind after reordering lines) failed with
        // "Unexpected token in primary: Semicolon" — the parser tried to read
        // a whole statement starting at the second `;` instead of treating it
        // as an empty no-op statement.
        let mut vm = make_vm();
        run_vm("sq: {[x] tmp: x*x;; tmp}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "sq[9]"), Value::Int(81));
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
    fn a_function_literal_applies_in_place() {
        assert_eq!(scalar(&mut make_vm(), "{[y] y*2}[5]"), Value::Int(10));
    }

    #[test]
    fn a_function_passed_as_an_argument_is_applied_by_the_callee() {
        let mut vm = make_vm();
        run_vm("apply: {[f,x] f[x]}", &mut vm).unwrap();
        // an anonymous literal ...
        assert_eq!(scalar(&mut vm, "apply[{[y] y*2}; 5]"), Value::Int(10));
        // ... and a bound name, which now resolves through `globals` like any value
        run_vm("double: {[y] y*2}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "apply[double; 21]"), Value::Int(42));
        // the parameter is callable more than once, and nests
        run_vm("twice: {[f,x] f[f[x]]}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "twice[{[y] y+3}; 1]"), Value::Int(7));
    }

    #[test]
    fn a_function_can_be_returned_stored_and_rebound() {
        let mut vm = make_vm();
        run_vm("mk: {[n] {[y] y+1}}", &mut vm).unwrap();
        run_vm("g: mk[0]", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "g[41]"), Value::Int(42));
        run_vm("h: g", &mut vm).unwrap(); // plain aliasing
        assert_eq!(scalar(&mut vm, "h[41]"), Value::Int(42));
    }

    #[test]
    fn a_returned_niladic_function_still_auto_invokes_on_a_bare_reference() {
        let mut vm = make_vm();
        run_vm("mk: {[n] {[] 99}}", &mut vm).unwrap();
        run_vm("g: mk[0]", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "g"), Value::Int(99));
        assert_eq!(scalar(&mut vm, "g[]"), Value::Int(99));
    }

    #[test]
    fn a_function_body_sees_no_caller_locals_even_when_passed_in() {
        // the non-capturing invariant: `f`'s body resolves `n` against the
        // session globals, never against `outer`'s frame.
        let mut vm = make_vm();
        run_vm("outer: {[n] apply[{[y] y+n}; 1]}", &mut vm).unwrap();
        run_vm("apply: {[f,x] f[x]}", &mut vm).unwrap();
        assert!(run_vm("outer[10]", &mut vm).is_err());
        assert!(vm.scopes.is_empty());
    }

    #[test]
    fn applying_a_non_function_value_is_an_error() {
        let mut vm = make_vm();
        run_vm("notafn: 3", &mut vm).unwrap();
        assert!(run_vm("notafn[1]", &mut vm).is_err());
        assert!(run_vm("apply: {[f,x] f[x]}", &mut vm).is_ok());
        assert!(run_vm("apply[3; 1]", &mut vm).is_err());
        assert!(vm.scopes.is_empty());
    }

    #[test]
    fn a_function_value_cannot_be_used_as_a_column() {
        let mut vm = make_vm();
        run_vm("myfn: {[x] x+1}", &mut vm).unwrap();
        let err = run_vm("select a: myfn from t", &mut vm).unwrap_err();
        assert!(format!("{err:?}").contains("cannot be used in a query expression"), "{err:?}");
    }

    #[test]
    fn a_bare_function_name_is_the_function_value_itself() {
        let mut vm = make_vm();
        run_vm("f: {[x] x}", &mut vm).unwrap();
        match run_vm("f", &mut vm) {
            Ok(crate::vm::EvalResult::Scalar(Value::Closure(f))) => {
                assert_eq!(f.params, vec!["x".to_string()]);
            }
            other => panic!("expected a closure, got {other:?}"),
        }
    }

    #[test]
    fn one_column_select_in_assignment_materialises_to_a_list() {
        let mut vm = make_vm();
        assert!(matches!(run_vm("px: select c2 from t", &mut vm), Ok(EvalResult::Stored)));
        assert_eq!(vm.globals.get("px"), Some(&ast::int_vec(vec![10, 20, 30, 15])));
    }

    #[test]
    fn bare_one_column_select_still_prints_a_table() {
        assert!(matches!(run_vm("select c2 from t", &mut make_vm()), Ok(EvalResult::Table(_))));
    }

    #[test]
    fn where_filters_a_column_expression() {
        assert_eq!(scalar(&mut make_vm(), "t`c2 where c2 > 15"), ast::int_vec(vec![20, 30]));
    }

    #[test]
    fn list_where_filters_a_bare_list_by_its_own_elements() {
        let mut vm = make_vm();
        run_vm("l: 10 20 30 40 50", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "l where x > 25"), ast::int_vec(vec![30, 40, 50]));
    }

    #[test]
    fn list_where_supports_comma_separated_predicates() {
        let mut vm = make_vm();
        run_vm("l: 10 20 30 40 50", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "l where x > 10, x < 50"), ast::int_vec(vec![20, 30, 40]));
    }

    // `` t`c2 where <pred> `` is already claimed by the *existing* column-expr
    // `where` sugar (a table row-filter by a real column, compiled through
    // `finish_table_ref` before list-where's own postfix check ever runs) —
    // list-where only kicks in on an operand that sugar didn't already
    // consume. Chaining onto its *result* works once parenthesised, since
    // the parenthesised expression is then a plain noun for list-where to
    // attach to.
    #[test]
    fn list_where_chains_onto_a_parenthesised_column_expression_result() {
        assert_eq!(
            scalar(&mut make_vm(), "(t`c2 where c2 > 10) where x < 30"),
            ast::int_vec(vec![20, 15]),
        );
    }

    #[test]
    fn list_where_on_a_non_list_value_is_a_runtime_error() {
        assert!(run_vm("2 where x > 1", &mut make_vm()).is_err());
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
    fn reducer_ignores_nulls_in_the_source_column() {
        // regression: `max t`col` used to force an eager materialise of the
        // raw source column before applying the reducer, which rejected any
        // column containing nulls outright ("cannot be materialised into a
        // list") even though the reducer itself (like plain Polars) would
        // just skip them. Pushing the reduction into the lazy plan directly
        // means only the *result* is checked for nulls now.
        let mut vm = make_vm();
        let df = df!["n" => [Some(10i64), None, Some(30)]].unwrap();
        vm.tables.insert("nt".into(), df);
        assert_eq!(scalar(&mut vm, "max nt`n"), Value::Int(30));
        // an all-null column still errors: the reduction result is itself null
        let df_all_null = df!["n" => [None::<i64>, None]].unwrap();
        vm.tables.insert("allnull".into(), df_all_null);
        assert!(run_vm("max allnull`n", &mut vm).is_err());
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
            ast::float_vec(vec![10.0, 20.0, 30.0, 15.0])
        );
    }

    #[test]
    fn take_head_and_tail() {
        assert_eq!(scalar(&mut make_vm(), "2#t`c2"), ast::int_vec(vec![10, 20]));
        assert_eq!(scalar(&mut make_vm(), "-2#t`c2"), ast::int_vec(vec![30, 15]));
    }

    #[test]
    fn take_count_can_be_a_bound_global() {
        // regression: `k#…` only ever accepted a literal int for `k`; a
        // variable count fell through to a parse error ("expected Eof/RParen,
        // got Hash") because the take-count was baked in at parse time.
        let mut vm = make_vm();
        run_vm("k: 2", &mut vm).expect("run");
        assert_eq!(scalar(&mut vm, "k#t`c2"), ast::int_vec(vec![10, 20]));
        assert_eq!(scalar(&mut vm, "-k#t`c2"), ast::int_vec(vec![30, 15]));
        assert!(matches!(run_vm("k#t", &mut vm), Ok(EvalResult::Table(_))));
        assert!(matches!(run_vm("(k+1)#t", &mut vm), Ok(EvalResult::Table(_))));
    }

    #[test]
    fn positional_index_with_an_int_run() {
        assert_eq!(scalar(&mut make_vm(), "(t`c2) 0 3"), ast::int_vec(vec![10, 15]));
    }

    #[test]
    fn bracket_index_atom_vs_slice() {
        let mut vm = make_vm();
        assert!(matches!(run_vm("l: 5 6 7 8 9", &mut vm), Ok(EvalResult::Stored)));
        // a single int picks an atom; an int run picks a sub-list
        assert_eq!(scalar(&mut vm, "l[0]"), Value::Int(5));
        assert_eq!(scalar(&mut vm, "l[1 3 4]"), ast::int_vec(vec![6, 8, 9]));
        // works directly on a column expression, and chains
        assert_eq!(scalar(&mut vm, "t`c2[2 1]"), ast::int_vec(vec![30, 20]));
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

    fn run_err(vm: &mut Vm, src: &str) -> String {
        match run_vm(src, vm) {
            Err(e) => e.to_string(),
            Ok(r) => panic!("expected an error for {src:?}, got a {} result", kind(&r)),
        }
    }

    fn run_stored(vm: &mut Vm, src: &str) {
        match run_vm(src, vm) {
            Ok(EvalResult::Stored) => {}
            Ok(r) => panic!("{src:?}: expected nothing, got a {} result", kind(&r)),
            Err(e) => panic!("{src:?}: {e}"),
        }
    }

    #[test]
    fn while_counts_down_in_the_current_scope() {
        let mut vm = make_vm();
        run_stored(&mut vm, "x: 5");
        run_stored(&mut vm, "while[x>2; x: x-1]");
        assert_eq!(scalar(&mut vm, "x"), Value::Int(2));
    }

    #[test]
    fn while_runs_its_statements_in_order_and_may_not_run_at_all() {
        let mut vm = make_vm();
        run_stored(&mut vm, "n: 0");
        run_stored(&mut vm, "acc: 0");
        // `(acc*10)+n` is order-sensitive: appending 0,1,2 gives 12
        run_stored(&mut vm, "while[n<3; acc: (acc*10)+n; n: n+1]");
        assert_eq!(scalar(&mut vm, "acc"), Value::Int(12));
        run_stored(&mut vm, "while[0b; acc: 99]");
        assert_eq!(scalar(&mut vm, "acc"), Value::Int(12));
    }

    #[test]
    fn while_body_may_run_table_statements() {
        let mut vm = make_vm();
        run_stored(&mut vm, "k: 0");
        run_stored(&mut vm, "while[k<2; t: select from t where c2 > 10; k: k+1]");
        assert_eq!(vm.tables["t"].height(), 3);
    }

    #[test]
    fn while_inside_a_function_binds_locals_only() {
        let mut vm = make_vm();
        run_stored(&mut vm, "s: 100");
        run_stored(&mut vm, "g: {[n] s: 0; while[n>0; s: s+n; n: n-1]; s}");
        assert_eq!(scalar(&mut vm, "g[4]"), Value::Int(10));
        assert_eq!(scalar(&mut vm, "s"), Value::Int(100), "the global is untouched");
    }

    #[test]
    fn a_function_still_cannot_assign_a_global() {
        let mut vm = make_vm();
        run_stored(&mut vm, "x: 1");
        run_stored(&mut vm, "bump: {[] x: x+1; x}");
        assert_eq!(scalar(&mut vm, "bump[]"), Value::Int(2));
        assert_eq!(scalar(&mut vm, "x"), Value::Int(1));
    }

    #[test]
    fn a_non_boolean_test_is_an_error() {
        let mut vm = make_vm();
        assert!(run_err(&mut vm, "while[1; 2]").contains("boolean scalar"));
        assert!(run_err(&mut vm, "while[1 2 3; 2]").contains("boolean scalar"));
    }

    #[test]
    fn noop_prints_nothing() {
        let mut vm = make_vm();
        run_stored(&mut vm, "noop");
        run_stored(&mut vm, "f: {[] noop}");
        run_stored(&mut vm, "f[]");
        run_stored(&mut vm, "?[0b; 1; noop]");
        run_stored(&mut vm, "while[0b; 1]");
    }

    #[test]
    fn a_noop_cannot_be_assigned() {
        let mut vm = make_vm();
        run_stored(&mut vm, "f: {[] noop}");
        for src in ["x: noop", "x: f[]", "x: while[0b; 1]", "x: ?[1b; noop; 1]"] {
            assert_eq!(run_err(&mut vm, src), "'cannot assign a no-op expression.", "{src}");
        }
        assert!(vm.globals.get("x").is_none());
    }

    #[test]
    fn a_noop_cannot_be_an_operand() {
        let mut vm = make_vm();
        run_stored(&mut vm, "g: {[a] a}");
        run_stored(&mut vm, "f: {[] noop}");
        for src in ["1 + noop", "g[noop]", "sum f[]"] {
            assert!(run_err(&mut vm, src).contains("no-op expression as a value"), "{src}");
        }
    }

    #[test]
    fn ctrl_c_stops_an_infinite_while_and_the_session_carries_on() {
        let mut vm = make_vm();
        run_stored(&mut vm, "x: 0");
        let interrupt = vm.interrupt.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            interrupt.request();
        });
        {
            let _running = vm.interrupt.statement();
            let err = run_vm("while[1b; x: x+1]", &mut vm).err().expect("should be interrupted");
            assert!(matches!(err, QplError::Interrupted), "{err}");
        }
        t.join().unwrap();
        // bindings made before the interrupt stay; the next statement runs normally
        match scalar(&mut vm, "x") {
            Value::Int(n) => assert!(n > 0),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn ctrl_c_stops_runaway_recursion() {
        let mut vm = make_vm();
        run_stored(&mut vm, "f: {[n] f[n+1]}");
        vm.interrupt.request();
        assert!(matches!(run_vm("f[0]", &mut vm), Err(QplError::Interrupted)));
        let _running = vm.interrupt.statement(); // a new statement clears the stale request
        assert_eq!(scalar(&mut vm, "1+1"), Value::Int(2));
    }

    #[test]
    fn nested_whiles_reset_their_inner_counter() {
        let mut vm = make_vm();
        run_stored(&mut vm, "a: 0");
        run_stored(&mut vm, "n: 0");
        run_stored(&mut vm, "while[a<3; b: 0; while[b<2; b: b+1; n: n+1]; a: a+1]");
        assert_eq!(scalar(&mut vm, "a"), Value::Int(3));
        assert_eq!(scalar(&mut vm, "b"), Value::Int(2));
        assert_eq!(scalar(&mut vm, "n"), Value::Int(6), "inner body ran 3 x 2 times");
    }

    #[test]
    fn the_test_is_re_evaluated_each_iteration_and_may_call_a_function() {
        let mut vm = make_vm();
        run_stored(&mut vm, "ok: {[v] v<3}");
        run_stored(&mut vm, "c: 0");
        run_stored(&mut vm, "while[ok[c]; c: c+1]");
        assert_eq!(scalar(&mut vm, "c"), Value::Int(3));
    }

    #[test]
    fn a_body_may_call_a_recursive_function_and_use_a_conditional() {
        let mut vm = make_vm();
        run_stored(&mut vm, "tri: {[n] ?[n<1; 0; n + tri[n-1]]}");
        run_stored(&mut vm, "c: 0");
        run_stored(&mut vm, "tot: 0");
        run_stored(&mut vm, "while[c<4; c: c+1; tot: tot + ?[c>2; tri[c]; 0]]");
        assert_eq!(scalar(&mut vm, "tot"), Value::Int(6 + 10));
    }

    #[test]
    fn while_in_a_conditional_branch_only_runs_when_taken() {
        let mut vm = make_vm();
        run_stored(&mut vm, "c: 0");
        run_stored(&mut vm, "?[0b; while[c<3; c: c+1]; noop]");
        assert_eq!(scalar(&mut vm, "c"), Value::Int(0));
        run_stored(&mut vm, "?[1b; while[c<3; c: c+1]; noop]");
        assert_eq!(scalar(&mut vm, "c"), Value::Int(3));
    }

    #[test]
    fn nothing_untaken_is_evaluated() {
        let mut vm = make_vm();
        // a body that would error is never reached when the test starts false
        run_stored(&mut vm, "while[0b; undefined_fn[1]]");
        // ... nor an untaken conditional branch
        run_stored(&mut vm, "?[1b; noop; undefined_fn[1]]");
    }

    #[test]
    fn an_error_in_the_body_stops_the_loop_and_keeps_earlier_bindings() {
        let mut vm = make_vm();
        run_stored(&mut vm, "c: 0");
        assert!(run_err(&mut vm, "while[c<5; c: c+1; boom[]; c: 100]").contains("boom"));
        assert_eq!(scalar(&mut vm, "c"), Value::Int(1), "the first iteration got as far as `boom[]`");
    }

    #[test]
    fn a_noop_or_table_test_is_not_a_boolean() {
        let mut vm = make_vm();
        assert!(run_err(&mut vm, "while[noop; 1]").contains("boolean scalar"));
        assert!(run_err(&mut vm, "while[t; 1]").contains("boolean scalar"));
        assert!(run_err(&mut vm, "?[noop; 1; 2]").contains("boolean scalar"));
    }

    #[test]
    fn a_function_may_run_a_while_and_a_noop_before_its_return() {
        let mut vm = make_vm();
        run_stored(&mut vm, "f: {[] while[0b; 1]; noop; 7}");
        assert_eq!(scalar(&mut vm, "f[]"), Value::Int(7));
    }

    #[test]
    fn a_function_body_that_ends_in_while_returns_nothing() {
        let mut vm = make_vm();
        run_stored(&mut vm, "f: {[n] while[n>0; n: n-1]}");
        run_stored(&mut vm, "f[3]");
        assert_eq!(run_err(&mut vm, "y: f[3]"), "'cannot assign a no-op expression.");
    }

    #[test]
    fn a_while_in_a_function_reads_globals_but_leaves_them_alone() {
        let mut vm = make_vm();
        run_stored(&mut vm, "lim: 3");
        run_stored(&mut vm, "f: {[] c: 0; while[c<lim; c: c+1]; c}");
        assert_eq!(scalar(&mut vm, "f[]"), Value::Int(3), "the test reads the global");
        // assigning the same name in the body binds a *local* that then shadows it
        run_stored(&mut vm, "g: {[] c: 0; while[c<lim; c: c+1; lim: 5]; c}");
        assert_eq!(scalar(&mut vm, "g[]"), Value::Int(5));
        assert_eq!(scalar(&mut vm, "lim"), Value::Int(3), "the global is untouched");
    }

    #[test]
    fn a_noop_argument_to_a_call_is_an_error_for_every_kind_of_callee() {
        let mut vm = make_vm();
        run_stored(&mut vm, "id: {[a] a}");
        run_stored(&mut vm, "nil: {[] noop}");
        for src in ["id[noop]", "id[nil[]]", "sum noop", "count noop", "{[a] a}[noop]"] {
            assert!(run_err(&mut vm, src).contains("no-op expression"), "{src}");
        }
    }

    #[test]
    fn a_noop_cannot_index_take_or_cast() {
        let mut vm = make_vm();
        for src in ["3#noop", "f64$noop", "noop[0]"] {
            let err = run_err(&mut vm, src);
            assert!(err.contains("no-op") || err.contains("noop"), "{src}: {err}");
        }
    }

    #[test]
    fn a_failed_noop_assignment_keeps_the_previous_binding() {
        let mut vm = make_vm();
        run_stored(&mut vm, "x: 5");
        assert!(run_err(&mut vm, "x: noop").contains("cannot assign a no-op"));
        assert_eq!(scalar(&mut vm, "x"), Value::Int(5));
    }

    #[test]
    fn an_interrupt_before_a_query_stops_it_and_binds_nothing() {
        let mut vm = make_vm();
        vm.interrupt.request();
        assert!(matches!(run_vm("select from t", &mut vm), Err(QplError::Interrupted)));
        assert!(matches!(run_vm("x: 1", &mut vm), Err(QplError::Interrupted)));
        assert!(!vm.globals.contains_key("x"));
        let _running = vm.interrupt.statement();
        assert_eq!(scalar(&mut vm, "1"), Value::Int(1));
    }

    #[test]
    fn an_interrupt_inside_a_nested_call_unwinds_the_scopes() {
        let mut vm = make_vm();
        run_stored(&mut vm, "f: {[n] while[1b; n: n+1]; n}");
        run_stored(&mut vm, "g: {[n] f[n]}");
        let interrupt = vm.interrupt.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            interrupt.request();
        });
        {
            let _running = vm.interrupt.statement();
            assert!(matches!(run_vm("g[0]", &mut vm), Err(QplError::Interrupted)));
        }
        t.join().unwrap();
        assert!(vm.scopes.is_empty(), "every call frame was popped on the way out");
        assert_eq!(scalar(&mut vm, "1+1"), Value::Int(2));
    }

    #[test]
    fn a_stale_interrupt_does_not_abort_the_next_statement() {
        let mut vm = make_vm();
        {
            let _running = vm.interrupt.statement();
            vm.interrupt.request(); // arrives after the last check point
        }
        assert_eq!(scalar(&mut vm, "1+1"), Value::Int(2));
    }

    #[test]
    fn a_vector_condition_gives_an_elementwise_result() {
        let mut vm = make_vm();
        assert_eq!(scalar(&mut vm, "?[1011b; 1; 0]"), ast::int_vec(vec![1, 0, 1, 1]));
        assert_eq!(scalar(&mut vm, "?[1011b; 1 2 3 4; 5 6 7 8]"), ast::int_vec(vec![1, 6, 3, 4]));
        assert_eq!(
            scalar(&mut vm, r#"?[1011b; "yes"; "no"]"#),
            ast::str_vec(vec!["yes".into(), "no".into(), "yes".into(), "yes".into()])
        );
    }

    #[test]
    fn a_vector_condition_may_come_from_a_variable_or_a_comparison() {
        let mut vm = make_vm();
        run_stored(&mut vm, "x: 5 6 7 8");
        assert_eq!(scalar(&mut vm, "?[x>6; x; 0]"), ast::int_vec(vec![0, 0, 7, 8]));
        run_stored(&mut vm, "f: {[m] ?[m; 1; 0]}");
        assert_eq!(scalar(&mut vm, "f[10b]"), ast::int_vec(vec![1, 0]));
    }

    #[test]
    fn a_vector_branch_must_match_the_condition_length() {
        let mut vm = make_vm();
        for src in ["?[1011b; 1 2 3; 0]", "?[1011b; 1; 5 6 7 8 9]", "?[10b; 1 2 3 4; 0]"] {
            let err = run_err(&mut vm, src);
            assert!(err.contains("has length") && err.contains("length of the condition"), "{src}: {err}");
        }
    }

    #[test]
    fn a_chained_vector_conditional_takes_the_first_true_condition_per_element() {
        let mut vm = make_vm();
        assert_eq!(scalar(&mut vm, "?[1011b; 1; 0110b; 2; 3]"), ast::int_vec(vec![1, 2, 1, 1]));
        assert_eq!(scalar(&mut vm, "?[0110b; 1; 0011b; 2; 3]"), ast::int_vec(vec![3, 1, 1, 2]));
        // every later condition must be boolean and as long as the first
        assert!(run_err(&mut vm, "?[1011b; 1; 01b; 2; 3]").contains("condition has length 2"));
        assert!(run_err(&mut vm, "?[1011b; 1; 1; 2; 3]").contains("boolean scalar or vector"));
    }

    #[test]
    fn an_atom_condition_after_a_vector_one_is_broadcast() {
        let mut vm = make_vm();
        assert_eq!(scalar(&mut vm, "?[1011b; 1; 1b; 2; 3]"), ast::int_vec(vec![1, 2, 1, 1]));
        assert_eq!(scalar(&mut vm, "?[1011b; 1; 0b; 2; 3]"), ast::int_vec(vec![1, 3, 1, 1]));
    }

    #[test]
    fn atom_conditions_before_a_vector_one_still_short_circuit() {
        let mut vm = make_vm();
        assert_eq!(scalar(&mut vm, "?[0b; undefined_fn[1]; 1011b; 5; 6]"), ast::int_vec(vec![5, 6, 5, 5]));
        // a true atom returns its branch as-is, whatever its length
        assert_eq!(scalar(&mut vm, "?[1b; 1 2 3; 1011b; 5; 6]"), ast::int_vec(vec![1, 2, 3]));
    }

    #[test]
    fn a_vector_conditional_keeps_symbols_symbols_and_rejects_a_text_number_mix() {
        let mut vm = make_vm();
        assert_eq!(scalar(&mut vm, "?[1011b; `a`b`c`d; `z]"), ast::sym_vec(vec!["a".into(), "z".into(), "c".into(), "d".into()]));
        assert!(run_err(&mut vm, r#"?[1011b; 1; "a"]"#).contains("mix text and non-text"));
        assert_eq!(scalar(&mut vm, "?[1011b; 1; 2.5]"), ast::float_vec(vec![1.0, 2.5, 1.0, 1.0]));
    }

    #[test]
    fn a_vector_conditional_on_an_empty_condition_is_empty() {
        let mut vm = make_vm();
        run_stored(&mut vm, "e: til 0");
        match scalar(&mut vm, "?[e>0; 1; 2]") {
            v @ Value::IntVec(_) => assert_eq!(v.as_vec().unwrap().1.len(), 0),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_noop_or_table_branch_of_a_vector_conditional_is_an_error() {
        let mut vm = make_vm();
        assert!(run_err(&mut vm, "?[1011b; noop; 1]").contains("no-op"));
        assert!(run_err(&mut vm, "?[1011b; t; 1]").contains("expected a scalar"));
    }

    #[test]
    fn while_still_needs_a_boolean_atom() {
        let mut vm = make_vm();
        assert!(run_err(&mut vm, "while[10b; 1]").contains("boolean scalar"));
    }
}
