use crate::ast::{CastTarget, Expr, Function, SelectStmt, Stmt, TableExpr, TableSource, Value};
use crate::builtins::BuiltIn;
use crate::errors::QplError;
use crate::native::NativeId;
use crate::program::{
    BinOpKind, FuncProto, LineEntry, Op, Operand, PendingClosure, Program, WindowFn, WindowSpec,
};
use std::collections::HashSet;
use std::sync::Arc;

pub fn compile(stmt: &Stmt) -> Result<Program, QplError> {
    let mut out = Program::new();
    compile_stmt(stmt, &mut out)?;
    finish_pending(&mut out)?;
    Ok(out)
}

/// Drains `out.pending_closures`: each
/// function literal seen while compiling the statement/expression gets its
/// body appended here, after everything compiled so far, and its placeholder
/// `Operand::Func` (pushed with a dummy `(0,0)` entry at the literal's own
/// site) patched with the entry point now that it's known. A body may itself
/// contain nested lambdas, which get queued the same way and drained by the
/// same loop (LIFO order doesn't matter — every entry ends up correctly
/// resolved once the queue is empty). Both places that build a `Program`
/// from scratch — [`compile`] and the whole-program compiler — call this
/// once, after all of their statements are compiled.
pub(crate) fn finish_pending(out: &mut Program) -> Result<(), QplError> {
    if out.pending_closures.is_empty() {
        return Ok(());
    }
    // execution must never fall off the end of the statement's own code
    // straight into an appended body — `CALL` reaches a body only via its
    // recorded entry point (see `Op::Halt`'s doc comment).
    out.emit(Op::Halt);
    while let Some(pending) = out.pending_closures.pop() {
        let entry = (out.code.len() as u32, out.operands.len() as u32);
        compile_function_body(&pending.body, out)?;
        out.operands[pending.operand_index] = Operand::Func(Arc::new(FuncProto {
            params: pending.params,
            entry,
            display: pending.display,
        }));
    }
    Ok(())
}

/// A function body's bytecode: leading
/// statements run for their side effects (an assignment is already net-zero
/// on the stack via `STORE`; a bare expression statement is popped), and the
/// final statement (guaranteed by the parser to be an expression) supplies
/// the return value, followed by `RET`.
fn compile_function_body(body: &[Stmt], out: &mut Program) -> Result<(), QplError> {
    let (last, head) = body
        .split_last()
        .expect("function body is non-empty (checked by the parser)");
    for st in head {
        compile_stmt_for_effect(st, out)?;
    }
    match last {
        Stmt::SingleVar(expr) => compile_value_expr(expr, out)?,
        Stmt::RetTable(te) => compile_tbl_expr(te, out)?,
        // the parser rejects a body ending in an assignment before this is
        // ever reached (`parse_func_lit`), so this is unreachable via normal
        // parsing — kept as a real error, not a panic, in case a future
        // caller ever constructs a `Function` AST node directly.
        Stmt::Assign { .. } | Stmt::ScalarAssign { .. } => {
            return Err(QplError::Compile(
                "a function body must end with an expression".into(),
            ));
        }
        // `Log`/`Cfg`/`System` are only ever produced by
        // `parser::parse_top_level_stmt`, never
        // by the ordinary statement grammar `parse_func_lit` uses for a
        // function body — unreachable via normal parsing.
        Stmt::Log(_) | Stmt::Cfg(_) | Stmt::System { .. } => {
            return Err(QplError::Compile(
                "a `log`/`.qpl.cfg`/`\\`-command statement cannot end a function body".into(),
            ));
        }
    }
    out.emit(Op::Ret);
    Ok(())
}

fn compile_stmt(stmt: &Stmt, out: &mut Program) -> Result<(), QplError> {
    match stmt {
        // `compile_tbl_expr` always leaves the resulting `Frame` on top of the
        // stack (or nothing, for the terminal `sink` builtin) — there is no
        // separate "materialise the result" instruction to append.
        Stmt::RetTable(tbl_expr) => compile_tbl_expr(tbl_expr, out),
        // assignment: compile the body; workspace binding is handled by the VM
        Stmt::Assign { name, body, .. } => {
            compile_stmt(body, out)?;
            out.push_operand(Operand::Name(name.as_str().into()));
            out.emit(Op::Store);
            Ok(())
        }
        // scalar assigns are evaluated by the REPL before reaching the compiler
        Stmt::ScalarAssign { name, expr } => {
            compile_value_expr(expr, out)?;
            out.push_operand(Operand::Name(name.as_str().into()));
            out.emit(Op::Store);
            Ok(())
        }
        Stmt::SingleVar(expr) => {
            compile_value_expr(expr, out)?;
            Ok(())
        }
        // Only ever produced at the top level (`parser::parse_top_level_stmt`)
        // and handled there by `compile_program_stmt`, never reached through
        // this single-statement entry point.
        Stmt::Log(_) | Stmt::Cfg(_) | Stmt::System { .. } => Err(QplError::Compile(
            "a `log`/`.qpl.cfg`/`\\`-command statement is only valid at the top level".into(),
        )),
    }
}

/// Compile a table expression. Always leaves exactly one `Frame` on top of
/// the stack (`sink` excepted — it's terminal and consumes it). Wraps the
/// recursive [`compile_tbl_expr_inner`] with a single post-pass: if this
/// (sub)tree references the virtual column `i` anywhere, a `RowIndex`
/// instruction is inserted right after every `Source`/`LoadFile` it compiled
/// to — computed once here, at compile time. Safe to splice a
/// single-byte opcode into `code` at any point: `Op::Push` is the only
/// opcode that consumes anything from `operands`, and it does so by a
/// separate counter (`cp`) advanced only when it executes — inserting a
/// no-operand opcode like `RowIndex` never desynchronises the two streams.
pub(crate) fn compile_tbl_expr(tbl_expr: &TableExpr, out: &mut Program) -> Result<(), QplError> {
    let start = out.code.len();
    compile_tbl_expr_inner(tbl_expr, out)?;
    insert_row_index_if_referenced(out, start);
    Ok(())
}

fn insert_row_index_if_referenced(out: &mut Program, start: usize) {
    let needs_i = out.code[start..].contains(&(Op::LoadRowIdx as u8));
    if !needs_i {
        return;
    }
    let mut i = start;
    while i < out.code.len() {
        let byte = out.code[i];
        if byte == Op::Source as u8 || byte == Op::LoadFile as u8 {
            out.code.insert(i + 1, Op::RowIndex as u8);
            i += 2;
        } else {
            i += 1;
        }
    }
}

fn compile_tbl_expr_inner(tbl_expr: &TableExpr, out: &mut Program) -> Result<(), QplError> {
    match tbl_expr {
        TableExpr::Select(sel) => compile_select(sel, out),
        TableExpr::BuiltIn(func) => compile_builtin(func, out),
        TableExpr::Source(src) => compile_source(src, out),
    }
}

fn compile_source(src: &crate::ast::TableSource, out: &mut Program) -> Result<(), QplError> {
    use crate::ast::TableSource;
    match src {
        TableSource::InMem(name) => {
            out.push_operand(Operand::Name(name.as_str().into()));
            out.emit(Op::Source);
        }
        TableSource::Load(path) => {
            compile_value_expr(path, out)?;
            out.emit(Op::LoadFile);
        }
    }
    Ok(())
}

fn compile_builtin(builtin: &BuiltIn, out: &mut Program) -> Result<(), QplError> {
    match builtin {
        BuiltIn::Cols(tbl_expr) => {
            compile_tbl_expr_inner(tbl_expr, out)?;
            out.emit(Op::Cols);
            Ok(())
        }
        BuiltIn::Sink { src, path } => {
            compile_tbl_expr_inner(src.as_ref(), out)?;
            compile_value_expr(path, out)?;
            out.emit(Op::Sink);
            Ok(())
        }
        BuiltIn::Sort(tbl_expr, sort_map) => {
            compile_tbl_expr_inner(tbl_expr.as_ref(), out)?;
            out.push_operand(Operand::Sort(sort_map.clone().into()));
            out.emit(Op::Sort);
            Ok(())
        }
        BuiltIn::Distinct(tbl_expr) => {
            compile_tbl_expr_inner(tbl_expr.as_ref(), out)?;
            out.emit(Op::Distinct);
            Ok(())
        }
        BuiltIn::DropNull(columns, tbl_expr) => {
            compile_tbl_expr_inner(tbl_expr.as_ref(), out)?;
            out.push_operand(Operand::Names(columns.clone().into()));
            out.emit(Op::DropNull);
            Ok(())
        }
        BuiltIn::Limit(tbl_expr, count) => {
            compile_tbl_expr_inner(tbl_expr.as_ref(), out)?;
            compile_value_expr(count, out)?;
            out.emit(Op::Limit);
            Ok(())
        }
        BuiltIn::Drop(columns, tbl_expr) => {
            compile_tbl_expr_inner(tbl_expr.as_ref(), out)?;
            out.push_operand(Operand::Names(columns.clone().into()));
            out.emit(Op::Drop);
            Ok(())
        }
        BuiltIn::Lazy(tbl_expr) => {
            // The frame must already be on the stack for `Lazy` to flip its
            // flag, so the body is compiled first.
            compile_tbl_expr_inner(tbl_expr.as_ref(), out)?;
            out.emit(Op::Lazy);
            Ok(())
        }
        BuiltIn::Collect(tbl_expr) => {
            compile_tbl_expr_inner(tbl_expr.as_ref(), out)?;
            out.emit(Op::Collect);
            Ok(())
        }
    }
}

fn compile_select(sel: &SelectStmt, out: &mut Program) -> Result<(), QplError> {
    if sel.delete {
        compile_tbl_expr_inner(&sel.from, out)?;
        if let Some(preds) = &sel.where_ {
            for (index, expr) in preds.iter().enumerate() {
                compile_expr(expr, out)?;
                if index > 0 {
                    out.push_operand(Operand::BinOp(BinOpKind::from_op_str("&")));
                    out.emit(Op::BinOp);
                }
            }
            out.push_operand(Operand::Count(1));
            out.push_operand(Operand::Verb("not".into()));
            out.emit(Op::Verb);
            out.push_operand(Operand::Count(1));
            out.emit(Op::Filter);
        }
        let columns = sel
            .cols
            .iter()
            .map(delete_column_name)
            .collect::<Result<Vec<_>, _>>()?;
        // `Update` always expects a (possibly empty) keys `List` below its
        // predicates — `delete` has neither `by` nor a projected predicate,
        // so both are zero/empty.
        out.push_operand(Operand::Count(0));
        out.emit(Op::List);
        emit_update(out, 0, &columns, 0);
        return Ok(());
    }

    if sel.update {
        compile_tbl_expr_inner(&sel.from, out)?;
        let predicate_count = if let Some(preds) = &sel.where_ {
            for expr in preds {
                compile_expr(expr, out)?;
            }
            preds.len()
        } else {
            0
        };
        // Always emit a `List` for the keys — empty when there is no `by` —
        // so `Update` has a fixed stack shape regardless.
        let key_count = sel.by.as_ref().map_or(0, Vec::len);
        if let Some(keys) = &sel.by {
            for alias in keys {
                compile_expr(&alias.expr, out)?;
                emit_alias(
                    out,
                    alias.name.clone().or_else(|| implicit_alias(&alias.expr)),
                );
            }
        }
        out.push_operand(Operand::Count(key_count as u32));
        out.emit(Op::List);
        for alias in &sel.cols {
            compile_expr(&alias.expr, out)?;
            emit_alias(out, alias.name.clone());
        }
        let columns = sel
            .cols
            .iter()
            .map(|alias| {
                alias.name.clone().ok_or_else(|| {
                    QplError::Compile("update expressions require column aliases".into())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        emit_update(out, sel.cols.len(), &columns, predicate_count);
        return Ok(());
    }

    // Join phrase - done first so that the join is applied before any where clause filters
    if let Some((join_src, left_on, right_on, join_type)) = &sel.join {
        compile_tbl_expr_inner(&sel.from, out)?;
        let left_count = if let Value::SymVec(_) = left_on {
            let names = left_on.vec_strings().map_err(QplError::Runtime)?;
            for name in &names {
                out.push_operand(Operand::Name(name.as_str().into()));
                out.emit(Op::LoadCol);
            }
            names.len()
        } else {
            return Err(QplError::Runtime(format!(
                "Expected symbol for left_on, got {:?}",
                left_on
            )));
        };
        out.push_operand(Operand::Count(left_count as u32));
        out.emit(Op::List);
        let right_count = if let Value::SymVec(_) = right_on {
            let names = right_on.vec_strings().map_err(QplError::Runtime)?;
            for name in &names {
                out.push_operand(Operand::Name(name.as_str().into()));
                out.emit(Op::LoadCol);
            }
            names.len()
        } else {
            return Err(QplError::Runtime(format!(
                "Expected symbol for right_on, got {:?}",
                right_on
            )));
        };
        out.push_operand(Operand::Count(right_count as u32));
        out.emit(Op::List);
        compile_tbl_expr_inner(join_src, out)?;
        out.push_operand(Operand::Join(join_type.clone()));
        out.emit(Op::Join);
    } else {
        // From phrase
        compile_tbl_expr_inner(&sel.from, out)?;
    }

    // Where phrase: each subphrase is a successive filter (spec: evaluated left-to-right)
    if let Some(preds) = &sel.where_ {
        let n = preds.len();
        for expr in preds {
            compile_expr(expr, out)?;
        }
        out.push_operand(Operand::Count(n as u32));
        out.emit(Op::Filter);
    }

    // By phrase
    let has_by = sel.by.is_some();
    let by_names: Vec<String> = sel.by.as_ref().map_or(vec![], |keys| {
        keys.iter()
            .filter_map(|alias| alias.name.clone().or_else(|| implicit_alias(&alias.expr)))
            .collect()
    });
    if let Some(keys) = &sel.by {
        for alias in keys {
            compile_expr(&alias.expr, out)?;
            emit_alias(
                out,
                alias.name.clone().or_else(|| implicit_alias(&alias.expr)),
            );
        }
        out.push_operand(Operand::Count(keys.len() as u32));
        out.emit(Op::List);
    }

    // Select phrase — `group_by(keys).agg(proj)` already carries the key
    // columns through, so re-projecting a column under the same name as a
    // `by` key would hand Polars two columns with one name; skip it.
    let mut proj_count = 0;
    for alias in &sel.cols {
        let name = alias.name.clone().or_else(|| implicit_alias(&alias.expr));
        if has_by
            && name
                .as_deref()
                .is_some_and(|n| by_names.iter().any(|k| k == n))
        {
            continue;
        }
        compile_expr(&alias.expr, out)?;
        emit_alias(out, name);
        proj_count += 1;
    }
    out.push_operand(Operand::Count(proj_count));
    out.emit(Op::List);

    out.emit(if has_by { Op::SelectBy } else { Op::Select });
    if let Some(order) = &sel.order {
        out.push_operand(Operand::Sort(order.clone().into()));
        out.emit(Op::Sort);
    }
    Ok(())
}

/// Emit `UPDATE`'s three trailing operands (count, predicates, names — pushed
/// in that order so `Op::Update` pops them count-first, matching the order
/// they were pushed) after the exprs/list/preds/frame are already on the
/// stack.
fn emit_update(out: &mut Program, count: usize, names: &[String], predicates: usize) {
    out.push_operand(Operand::Names(names.to_vec().into()));
    out.push_operand(Operand::Count(predicates as u32));
    out.push_operand(Operand::Count(count as u32));
    out.emit(Op::Update);
}

/// `Alias{name: None}` emits nothing — the expression is left as-is.
fn emit_alias(out: &mut Program, name: Option<String>) {
    if let Some(name) = name {
        out.push_operand(Operand::Name(name.as_str().into()));
        out.emit(Op::Alias);
    }
}

fn delete_column_name(alias: &crate::ast::Alias) -> Result<String, QplError> {
    match &alias.expr {
        Expr::ColRef(name) | Expr::Sym(name) => Ok(name.clone()),
        other => Err(QplError::Compile(format!(
            "delete expects column names, got {other:?}"
        ))),
    }
}

/// A one-column `select` with no `by` — a *column expression* that
/// materialises to a list rather than a frame.
pub(crate) fn is_column_select(te: &TableExpr) -> bool {
    matches!(te, TableExpr::Select(sel)
        if sel.cols.len() == 1 && sel.by.is_none() && !sel.update && !sel.delete)
}

/// Lower a value-context expression.
/// Every node kind compiles straight to
/// bytecode. `Lit`/`Sym` push a literal,
/// `ColRef` resolves a bare name (`LOAD`), `BinOp`/`Cast` recurse into their
/// own operand(s) then emit the matching opcode — so e.g. `n * fac[n-1]`
/// compiles the multiplication directly and only defers the `fac[..]` call to
/// run time (`Op::Call`'s dispatch). A bare `Dict`/`IColRef`/`Window` (never
/// meaningful outside a `zip`/select) or a `Call` of an arity nothing
/// recognises is a compile-time error (see the `other =>` arm below).
pub(crate) fn compile_value_expr(node: &Expr, out: &mut Program) -> Result<(), QplError> {
    match node {
        Expr::Lit(v) => out.push_operand(Operand::Value(v.clone())),
        // unlike `compile_expr` (query context, where a bare symbol is a
        // string literal), a symbol in value context is its own value kind.
        Expr::Sym(s) => out.push_operand(Operand::Value(Value::Sym(s.clone()))),
        Expr::ColRef(name) => {
            out.push_operand(Operand::Name(name.as_str().into()));
            out.emit(Op::Load);
        }
        Expr::BinOp { left, op, right } => {
            compile_value_expr(left, out)?;
            compile_value_expr(right, out)?;
            out.push_operand(Operand::BinOp(BinOpKind::from_op_str(op)));
            out.emit(Op::BinOp);
        }
        Expr::Cast { target, expr } => {
            compile_value_expr(expr, out)?;
            out.push_operand(Operand::Cast(target.clone()));
            out.emit(Op::Cast);
        }

        // `enlist <value>` — an unconditional keyword: checked ahead of everything else so nothing can shadow
        // it.
        Expr::Call { func, args } if func == "enlist" && args.len() == 1 => {
            compile_value_expr(&args[0], out)?;
            out.push_operand(Operand::Count(1));
            out.push_operand(Operand::Native(NativeId::Enlist));
            out.emit(Op::Call);
        }

        // `3?6` / `2?10 20 30` — roll. Also unconditional.
        Expr::Call { func, args } if func == "?" && args.len() == 2 => {
            compile_value_expr(&args[0], out)?;
            compile_value_expr(&args[1], out)?;
            out.push_operand(Operand::Count(2));
            out.push_operand(Operand::Native(NativeId::Roll));
            out.emit(Op::Call);
        }

        // `log a b c` / `log[..]` — writes the concatenated rendering of its
        // (possibly zero) arguments through `Vm::emit` and returns the text.
        // Checked ahead of the generic 1..=2-arg `Call` arm so every arity
        // takes the same path; `Op::Call`'s runtime dispatch
        // (`ops::call_by_name`) still tries a bound closure/builtin named
        // `log` first, so a user function of that name keeps shadowing it.
        Expr::Call { func, args } if func == "log" => {
            for arg in args {
                compile_value_expr(arg, out)?;
            }
            out.push_operand(Operand::Count(args.len() as u32));
            out.push_operand(Operand::Name(func.as_str().into()));
            out.emit(Op::Call);
        }

        // `zip \`k1\`k2!v1 v2` — a dict of named lists. A top-level cast on a
        // column value keeps its exact width via `CastList`; a non-dict argument or an empty dict
        // falls back to (respectively) a generic verb call / a runtime error.
        // A user function named `zip` is not consulted here when the
        // argument is syntactically a dict literal.
        Expr::Call { func, args } if func == "zip" && args.len() == 1 => match &args[0] {
            Expr::Dict(pairs) if !pairs.is_empty() => {
                for (name, expr) in pairs {
                    if let Expr::Cast {
                        target,
                        expr: inner,
                    } = expr
                    {
                        compile_value_expr(inner, out)?;
                        out.push_operand(Operand::Cast(target.clone()));
                        out.push_operand(Operand::Name(name.as_str().into()));
                        out.emit(Op::CastList);
                    } else {
                        compile_value_expr(expr, out)?;
                    }
                }
                let names: Vec<String> = pairs.iter().map(|(n, _)| n.clone()).collect();
                out.push_operand(Operand::Names(names.into()));
                out.push_operand(Operand::Count(pairs.len() as u32));
                out.emit(Op::Zip);
            }
            Expr::Dict(_) => {
                return Err(QplError::Runtime("'zip' needs at least one column".into()));
            }
            other => {
                return Err(QplError::Runtime(format!(
                    "'zip' expects a dict (`col1`col2!v1 v2`), got {other:?}"
                )));
            }
        },

        // A one-column `select` / `` table`col `` collapses to a list; any
        // other table expression stays a frame.
        Expr::Table(te) => {
            compile_tbl_expr(te, out)?;
            if is_column_select(te) {
                out.emit(Op::Column);
            }
        }

        // `<n>#<expr>` — head/tail slice of a frame or list.
        Expr::Take { n, expr } => {
            compile_value_expr(n, out)?;
            compile_value_expr(expr, out)?;
            out.emit(Op::Take);
        }

        // `f[x]` where `f` is (syntactically) a bare name or a closure
        // literal applied in place — resolved to a call vs. positional
        // indexing at run time, since only `Op::LoadFn`/`vm.is_callable` know
        // which. Anything else is unambiguously positional indexing.
        Expr::Index { expr, idx } => {
            match expr.as_ref() {
                Expr::ColRef(name) => {
                    out.push_operand(Operand::Name(name.as_str().into()));
                    out.emit(Op::LoadFn);
                }
                _ => compile_value_expr(expr, out)?,
            }
            compile_value_expr(idx, out)?;
            out.push_operand(Operand::Count(1));
            out.emit(Op::Index);
        }

        // `{[p..] body}` — a function literal: push a placeholder `Func` operand now (the literal's
        // position in the operand stream is fixed by execution order), queue
        // the body to be compiled and appended after the enclosing program's
        // main code once it's fully compiled, and patch this operand with the
        // real entry point then (see `finish_pending`).
        Expr::Lambda(func) => {
            let display = format!("{{[{}] ..}}", func.params.join(","));
            let operand_index = out.operands.len();
            out.push_operand(Operand::Func(Arc::new(FuncProto {
                params: func.params.clone(),
                entry: (0, 0),
                display: display.clone(),
            })));
            out.pending_closures.push(PendingClosure {
                operand_index,
                params: func.params.clone(),
                body: func.body.clone(),
                display,
            });
        }

        // `f[a;b]` / `f[]` — apply a function to a (possibly empty)
        // semicolon-separated argument list.
        // `func` resolves at run time exactly like `Op::Index`'s callable
        // target: `LOAD_FN` for a bare name (deferred if callable), otherwise
        // compiled as an ordinary value expression (a param holding a
        // function, another call's result, an anonymous literal applied in
        // place, …). Args are compiled — and thus evaluated — before `func`
        // is resolved, matching `f[x]`'s existing `Index` lowering
        // (arguments are evaluated in the caller's frame, before the callee
        // is resolved).
        Expr::Apply { func, args } => {
            for arg in args {
                compile_value_expr(arg, out)?;
            }
            out.push_operand(Operand::Count(args.len() as u32));
            match func.as_ref() {
                Expr::ColRef(name) => {
                    out.push_operand(Operand::Name(name.as_str().into()));
                    out.emit(Op::LoadFn);
                }
                _ => compile_value_expr(func, out)?,
            }
            out.emit(Op::Call);
        }

        // `<conn> dispatch <cmd>` / `<conn> async dispatch <cmd>`.
        Expr::Dispatch {
            conn,
            command,
            is_async,
        } => {
            compile_value_expr(conn, out)?;
            out.push_operand(Operand::Text(command.as_str().into()));
            out.push_operand(Operand::Value(Value::Bool(*is_async)));
            out.emit(Op::Dispatch);
        }

        // `<list-expr> where <predicate>...`.
        Expr::ListWhere { list, where_ } => {
            compile_value_expr(list, out)?;
            out.emit(Op::ListWhereFrame);
            for pred in where_ {
                compile_expr(pred, out)?;
            }
            out.push_operand(Operand::Count(where_.len() as u32));
            out.emit(Op::Filter);
            out.emit(Op::Column);
        }

        // A generic call: `sum trades\`price`, `2 shift px`, `2 round px`,
        // `til 5`, `log a b`, `hopen 5001`, a user-function call by bare name
        // (`f x`), … `Op::Call`'s runtime dispatch (`ops::call_by_name`)
        // decides between a bound closure/builtin, a handful of shadowable
        // keywords, and a generic column verb.
        Expr::Call { func, args } if (1..=2).contains(&args.len()) => {
            // the verb's source (`args[0]`) stays an uncollapsed `Frame` when
            // it's a `` table`col `` / one-column `select` — collapsing it to
            // a list first (as the ordinary `Expr::Table` arm would) forces
            // an eager materialise of the *raw* column ahead of the reducer,
            // which would reject a column with nulls a reducer would simply
            // skip (`ops::value_verb` does the reduction inside the lazy
            // plan directly).
            match &args[0] {
                Expr::Table(te) => compile_tbl_expr(te, out)?,
                other => compile_value_expr(other, out)?,
            }
            for arg in &args[1..] {
                compile_value_expr(arg, out)?;
            }
            out.push_operand(Operand::Count(args.len() as u32));
            out.push_operand(Operand::Name(func.as_str().into()));
            out.emit(Op::Call);
        }

        // `?[c1;v1;…;d]` in value context. See `compile_case_value`'s doc comment for the shape.
        Expr::Case { branches, default } => compile_case_value(branches, default, out)?,

        // `while[test; s1; ...; sn]`. See `compile_while`.
        Expr::While { cond, body } => compile_while(cond, body, out)?,

        // `noop`: nothing.
        Expr::Noop => out.emit(Op::Noop),

        // A bare `Dict`/`IColRef`/`Window` (never meaningful outside a
        // `zip`/select) or a `Call` of an arity nothing above recognises —
        // was never supported outside a select/`zip`, raised here at
        // compile time.
        other => {
            return Err(QplError::Runtime(format!(
                "not supported in scalar context: {other:?}"
            )));
        }
    }
    Ok(())
}

/// Lower value-context `?[c1;v1;c2;v2;…;d]`.
/// Each branch condition is checked in
/// turn: a boolean *atom* short-circuits (only the taken branch's bytecode
/// ever runs, which is what makes `?[n<2;1;n*fac[n-1]]`-style conditional
/// recursion terminate); the first boolean *vector* condition diverts to a
/// per-`k` tail that evaluates every remaining condition/branch/default
/// eagerly and folds them with `CASE_VEC` (`ops::case_vec`'s
/// semantics, unchanged). Each branch compiles two mutually-exclusive
/// runtime paths (the atom path and the vector-diversion tail) — only one
/// ever executes per branch per run, so recompiling `val` into both is not a
/// double evaluation, just a shared code shape.
///
/// ```text
/// c1; PUSH Lvec1; JUMP_IF_VEC; PUSH Lnext1; PUSH msg; JUMP_IF_FALSE
///     v1; PUSH Lend; JUMP
/// Lvec1: v1; c2; v2; …; d; PUSH n; CASE_VEC; PUSH Lend; JUMP
/// Lnext1: c2; PUSH Lvec2; JUMP_IF_VEC; PUSH Lnext2; PUSH msg; JUMP_IF_FALSE
///     …
/// Lnextk: d
/// Lend:
/// ```
fn compile_case_value(
    branches: &[(Expr, Expr)],
    default: &Expr,
    out: &mut Program,
) -> Result<(), QplError> {
    const COND_MSG: &str =
        "a `?[..]` condition must be a boolean scalar or vector in value context";
    let mut end_patches: Vec<usize> = Vec::new();

    for (k, (cond, val)) in branches.iter().enumerate() {
        compile_value_expr(cond, out)?;

        let vec_idx = out.operands.len();
        out.push_operand(Operand::Target { ip: 0, cp: 0 }); // Lvec_k, patched below
        out.emit(Op::JumpIfVec);

        let next_idx = out.operands.len();
        out.push_operand(Operand::Target { ip: 0, cp: 0 }); // Lnext_k, patched below
        out.push_operand(Operand::Text(COND_MSG.into()));
        out.emit(Op::JumpIfFalse);

        // atom-true path: this branch's value is the whole expression's result
        compile_value_expr(val, out)?;
        let end_idx = out.operands.len();
        out.push_operand(Operand::Target { ip: 0, cp: 0 }); // Lend, patched once known
        out.emit(Op::Jump);
        end_patches.push(end_idx);

        // Lvec_k: the mask is still on the stack (JUMP_IF_VEC only peeked it)
        patch_target(out, vec_idx);
        compile_value_expr(val, out)?; // v_k
        let mut count: u32 = 1;
        for (cond2, val2) in &branches[k + 1..] {
            compile_value_expr(cond2, out)?;
            compile_value_expr(val2, out)?;
            count += 2;
        }
        compile_value_expr(default, out)?;
        count += 1;
        out.push_operand(Operand::Count(count));
        out.emit(Op::CaseVec);
        let end_idx = out.operands.len();
        out.push_operand(Operand::Target { ip: 0, cp: 0 });
        out.emit(Op::Jump);
        end_patches.push(end_idx);

        // Lnext_k: the next branch's own condition starts right here
        patch_target(out, next_idx);
    }
    // every branch's condition was a false atom
    compile_value_expr(default, out)?;

    let end_ip = out.code.len() as u32;
    let end_cp = out.operands.len() as u32;
    for idx in end_patches {
        out.operands[idx] = Operand::Target {
            ip: end_ip,
            cp: end_cp,
        };
    }
    Ok(())
}

/// Back-patch a placeholder `Operand::Target` at `idx` (pushed with a dummy
/// `(0, 0)` when the jump was first emitted, before its destination's
/// position was known) to the *current* end of `out` — i.e. call this
/// exactly when `out`'s next byte/operand is the label's own destination.
fn patch_target(out: &mut Program, idx: usize) {
    out.operands[idx] = Operand::Target {
        ip: out.code.len() as u32,
        cp: out.operands.len() as u32,
    };
}

/// Lower `while[test; s1; ...; sn]`:
/// re-tests `cond` before each iteration; the body's statements run in the
/// *current* scope (no call frame — an assignment binds wherever the loop
/// itself sits: globals at the top level, locals inside a function body).
/// The loop's own value is always `Noop`. The backward jump to `top` is
/// already known when emitted (no back-patch needed); only the exit jump
/// (`end_idx`, taken when `cond` is `false`) is a forward reference.
fn compile_while(cond: &Expr, body: &[Stmt], out: &mut Program) -> Result<(), QplError> {
    const WHILE_MSG: &str = "a `while` condition must be a boolean scalar in value context";
    let top_ip = out.code.len() as u32;
    let top_cp = out.operands.len() as u32;

    compile_value_expr(cond, out)?;
    let end_idx = out.operands.len();
    out.push_operand(Operand::Target { ip: 0, cp: 0 }); // Lend, patched below
    out.push_operand(Operand::Text(WHILE_MSG.into()));
    out.emit(Op::JumpIfFalse);

    for st in body {
        compile_stmt_for_effect(st, out)?;
    }

    out.push_operand(Operand::Target {
        ip: top_ip,
        cp: top_cp,
    });
    out.emit(Op::Jump);
    patch_target(out, end_idx);
    out.emit(Op::Noop);
    Ok(())
}

/// Compile one statement purely for its side effect, leaving the stack no
/// taller than before it ran: an assignment nets to zero via `STORE`
/// already; an expression statement's value is discarded with `POP`. Shared
/// by a function body's non-final statements (`compile_function_body`) and a
/// `while` body's statements (`compile_while`) — both run a list of
/// statements purely for effect, only the *last* one's handling differs
/// (a function body's last statement supplies the return value instead).
fn compile_stmt_for_effect(st: &Stmt, out: &mut Program) -> Result<(), QplError> {
    match st {
        Stmt::Assign { .. } | Stmt::ScalarAssign { .. } => compile_stmt(st, out),
        Stmt::SingleVar(expr) => {
            compile_value_expr(expr, out)?;
            out.emit(Op::Pop);
            Ok(())
        }
        Stmt::RetTable(te) => {
            compile_tbl_expr(te, out)?;
            out.emit(Op::Pop);
            Ok(())
        }
        Stmt::Log(_) | Stmt::Cfg(_) | Stmt::System { .. } => Err(QplError::Compile(
            "a `log`/`.qpl.cfg`/`\\`-command statement is only valid at the top level".into(),
        )),
    }
}

fn compile_expr(node: &Expr, out: &mut Program) -> Result<(), QplError> {
    match node {
        Expr::Lit(v) => out.push_operand(Operand::Value(v.clone())),
        // symbols are string-typed in Polars
        Expr::Sym(s) => out.push_operand(Operand::Value(Value::Str(s.clone()))),
        Expr::ColRef(name) => {
            out.push_operand(Operand::Name(name.as_str().into()));
            out.emit(Op::LoadCol);
        }
        Expr::IColRef => out.emit(Op::LoadRowIdx),
        Expr::BinOp { left, op, right } => {
            compile_expr(left, out)?;
            compile_expr(right, out)?;
            out.push_operand(Operand::BinOp(BinOpKind::from_op_str(op)));
            out.emit(Op::BinOp);
        }
        // `<precision> round <col>` — parser hands us args = [value, precision].
        // Precision must be a literal (it becomes part of the instruction); the
        // rounding mode is resolved from VM config at run time.
        Expr::Call { func, args } if func == "round" => {
            let [value, precision] = args.as_slice() else {
                return Err(QplError::Compile(
                    "round expects `<precision> round <column>`".into(),
                ));
            };
            let decimals = match precision {
                Expr::Lit(Value::Int(n)) if *n >= 0 => *n as u32,
                _ => {
                    return Err(QplError::Compile(
                        "round precision must be a non-negative integer literal".into(),
                    ));
                }
            };
            compile_expr(value, out)?;
            out.push_operand(Operand::Count(decimals));
            out.emit(Op::Round);
        }
        Expr::Call { func, args } => {
            for arg in args {
                compile_expr(arg, out)?;
            }
            out.push_operand(Operand::Count(args.len() as u32));
            out.push_operand(Operand::Verb(func.as_str().into()));
            out.emit(Op::Verb);
        }
        Expr::Cast { target, expr } => {
            compile_expr(expr, out)?;
            out.push_operand(Operand::Cast(target.clone()));
            out.emit(Op::Cast);
        }
        Expr::Case { branches, default } => {
            for (condition, value) in branches {
                compile_expr(condition, out)?;
                compile_expr(value, out)?;
            }
            compile_expr(default, out)?;
            out.push_operand(Operand::Count(branches.len() as u32));
            out.emit(Op::Case);
        }
        Expr::Window {
            func,
            partition,
            order,
            rolling,
        } => {
            if partition.is_empty() {
                return Err(QplError::Compile(
                    "`over` needs at least one partition symbol".into(),
                ));
            }
            // `<agg> <col> <n>!rolling over ...` — push the *raw* column and carry
            // the aggregate name in the operand; the VM applies the rolling
            // reduction instead of the plain aggregate.
            if let Some(window) = rolling {
                let (agg, column) = match func.as_ref() {
                    Expr::Call { func: agg, args } if args.len() == 1 => (agg.clone(), &args[0]),
                    _ => return Err(QplError::Compile(
                        "`rolling` must wrap a plain aggregate, e.g. `sum px 5!rolling over `k`"
                            .into(),
                    )),
                };
                compile_expr(column, out)?;
                out.push_operand(Operand::Window(
                    WindowSpec {
                        func: WindowFn::Over,
                        partition: partition.clone(),
                        order: order.clone(),
                        rolling: Some((agg, *window)),
                    }
                    .into(),
                ));
                out.emit(Op::Window);
                return Ok(());
            }
            // bare ranking verbs (`rn` / `rank` / `drank`) synthesise their own
            // expression from the window order; everything else is a column
            // expression applied per partition.
            let ranking = match func.as_ref() {
                Expr::ColRef(name) => match name.as_str() {
                    "rn" => Some(WindowFn::RowNumber),
                    "rank" => Some(WindowFn::Rank),
                    "drank" => Some(WindowFn::DenseRank),
                    _ => None,
                },
                _ => None,
            };
            match ranking {
                Some(_) => {
                    if order.is_empty() {
                        return Err(QplError::Compile(
                            "`rn` / `rank` / `drank` need an `order` sub-clause".into(),
                        ));
                    }
                }
                None => compile_expr(func, out)?,
            }
            out.push_operand(Operand::Window(
                WindowSpec {
                    func: ranking.unwrap_or(WindowFn::Over),
                    partition: partition.clone(),
                    order: order.clone(),
                    rolling: None,
                }
                .into(),
            ));
            out.emit(Op::Window);
        }
        Expr::Dict(_) => {
            return Err(QplError::Runtime(
                "Dict expressions are not supported in select statements (yet)".into(),
            ));
        }
        // column expressions / slices / indexing are value-context only —
        // compiled by `compile_value_expr`, never lowered into a projection.
        Expr::Table(_) => return Err(QplError::Compile(
            "a `table`col` / `select` column expression cannot appear inside a select projection"
                .into(),
        )),
        Expr::Take { .. } => {
            return Err(QplError::Compile(
                "`n#…` slicing is only valid outside a select projection".into(),
            ));
        }
        Expr::Index { .. } => {
            return Err(QplError::Compile(
                "positional indexing is only valid outside a select projection".into(),
            ));
        }
        Expr::Apply { .. } => {
            return Err(QplError::Compile(
                "a user function call `f[..]` is only valid outside a select projection".into(),
            ));
        }
        Expr::Lambda(_) => {
            return Err(QplError::Compile(
                "a function literal is only valid outside a select projection".into(),
            ));
        }
        Expr::Dispatch { .. } => {
            return Err(QplError::Compile(
                "`dispatch` is only valid outside a select projection".into(),
            ));
        }
        Expr::ListWhere { .. } => {
            return Err(QplError::Compile(
                "a list `where` is only valid outside a select projection".into(),
            ));
        }
        Expr::While { .. } | Expr::Noop => {
            return Err(QplError::Compile(
                "`while` / `noop` are only valid outside a select projection".into(),
            ));
        }
    }
    Ok(())
}

// Per spec: implicit alias is the leftmost column name in the expression, else "x".
fn implicit_alias(node: &Expr) -> Option<String> {
    match leftmost_leaf(node) {
        Expr::ColRef(name) if name != "i" => Some(name.clone()),
        Expr::Sym(name) => Some(name.clone()),
        _ => Some("x".into()),
    }
}

fn leftmost_leaf(node: &Expr) -> &Expr {
    let mut n = node;
    loop {
        match n {
            Expr::BinOp { left, .. } => n = left,
            Expr::Call { args, .. } if !args.is_empty() => n = &args[0],
            Expr::Window { func, .. } => n = func,
            _ => break,
        }
    }
    n
}

// ── Whole-program compilation ───────────────
//
// A script (or one REPL
// submission) is parsed in full by `parser::parse_program` before any of it
// compiles or runs; `compile_program` below turns that list of top-level
// statements into a single `Program`, resolving `\l`/`\i` targets (read,
// parsed and compiled right here, recursively) and namespace qualification
// (`qualify_program`) at compile time.

/// Whether a whole-program compile prints each top-level statement's result
/// (`Script`, used for a real script and for one REPL/`\port` submission) or
/// leaves only the *last* statement's value on the stack for the caller to
/// interpret as an [`crate::vm::EvalResult`] (`Result`, used by the IPC
/// server's `eval_for_dispatch` and by tests that want a single expression's
/// value back without printing).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CompileMode {
    Script,
    Result,
}

/// Compile-time context threaded through [`compile_program`] and its
/// recursive `\l`/`\i` embedding. `script_path` is what `\l`/`\i`'s relative
/// paths resolve against and what `Program::lines` records for error
/// messages; `ns` is `Some(".lib")` while compiling a script reached via
/// `\i "lib.qpl"` (namespace qualification); `including` is the chain
/// of canonicalized paths currently being compiled, for `\l`/`\i` cycle
/// detection.
#[derive(Clone)]
pub struct CompileCtx {
    pub mode: CompileMode,
    pub script_path: String,
    pub ns: Option<String>,
    including: Vec<String>,
}

impl CompileCtx {
    /// A top-level script or REPL/`\port` submission: prints each statement's
    /// result, no active namespace.
    pub fn script(path: &str) -> Self {
        Self {
            mode: CompileMode::Script,
            script_path: path.to_string(),
            ns: None,
            including: vec![canonical_path(path)],
        }
    }

    /// A single-expression evaluation (the IPC server, tests): only the last
    /// statement's value survives, for the caller to turn into an
    /// [`crate::vm::EvalResult`].
    pub fn result(path: &str) -> Self {
        Self {
            mode: CompileMode::Result,
            script_path: path.to_string(),
            ns: None,
            including: vec![canonical_path(path)],
        }
    }
}

/// Best-effort canonical form of `path`, for `\l`/`\i` cycle detection —
/// falls back to `path` itself (e.g. for `"<main>"`, or a file that doesn't
/// exist yet) since cycle detection only needs *some* stable key, not a real
/// filesystem round-trip.
fn canonical_path(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Wrap `e` with a `path:line:` prefix: a script/import path
/// gets `"{path}:{line+1}: {e}"` re-wrapped as a fresh [`QplError::Runtime`]
/// (so `{e}`'s own `Display` — itself already prefixed with `'` for a
/// `Runtime`/`Interrupted` error, or `"ParseError: "`/`"CompileError: "` for
/// the others — appears verbatim after the prefix, matching every existing
/// golden/error-path test byte-for-byte); `"<main>"` (typed at the REPL) and
/// an `Interrupted` error are never wrapped.
pub fn wrap_line_error(e: QplError, path: &str, line: u32) -> QplError {
    if path == "<main>" || matches!(e, QplError::Interrupted) {
        return e;
    }
    QplError::Runtime(format!("{path}:{}: {e}", line + 1))
}

/// A `\l`/`\i` path written inside a script is relative to that script's own
/// directory, so a library can pull in its neighbours wherever qpl was
/// started from. Typed at the prompt (`from` is `"<main>"`), or absolute,
/// it's used as written — relative to the working directory.
pub(crate) fn script_relative(target: &str, from: &str) -> String {
    let target_path = std::path::Path::new(target);
    if from == "<main>" || target_path.is_absolute() {
        return target.to_string();
    }
    match std::path::Path::new(from).parent() {
        Some(dir) => dir.join(target_path).to_string_lossy().into_owned(),
        None => target.to_string(),
    }
}

/// Derive a namespace (`.utils`, `.my_lib`) from a `\i`-imported script's
/// file stem: non-identifier characters become `_`, and a leading digit gets
/// an `_` prefix so the result always lexes as a valid namespaced name.
pub(crate) fn namespace_from_path(path: &str) -> String {
    let stem = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("ns");
    let mut cleaned: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.starts_with(|c: char| c.is_ascii_digit()) {
        cleaned.insert(0, '_');
    }
    format!(".{cleaned}")
}

/// Compile a whole program: `stmts` (as returned by
/// [`crate::parser::parse_program`]) into one [`Program`], per `ctx.mode`
/// Namespace-qualifies the whole statement list first, then compiles each statement in turn, recording a
/// [`LineEntry`] at its starting `ip` so a runtime failure anywhere in it
/// (including inside a function defined by it, appended later in the same
/// `Program`) can be reported at the right source line.
pub fn compile_program(stmts: Vec<(u32, Stmt)>, ctx: CompileCtx) -> Result<Program, QplError> {
    let mut stmts = stmts;
    if let Some(ns) = ctx.ns.clone() {
        let names = collect_ns_names(&stmts, &ctx.script_path)?;
        for (_, stmt) in stmts.iter_mut() {
            qualify_top_level(stmt, &ns, &names);
        }
    }
    let mut out = Program::new();
    let path: Arc<str> = Arc::from(ctx.script_path.as_str());
    let last = stmts.len().saturating_sub(1);
    for (i, (lineno, stmt)) in stmts.iter().enumerate() {
        out.lines.push(LineEntry {
            ip: out.code.len() as u32,
            path: path.clone(),
            line: *lineno,
        });
        let leave_value = ctx.mode == CompileMode::Result && i == last;
        compile_program_stmt(stmt, &mut out, &ctx, leave_value)
            .map_err(|e| wrap_line_error(e, &ctx.script_path, *lineno))?;
    }
    if ctx.mode == CompileMode::Result {
        out.emit(Op::Halt);
    }
    finish_pending(&mut out)?;
    Ok(out)
}

/// Is `expr` exactly a `log[..]` call — checked purely syntactically (not
/// whether `log` is actually bound/shadowed).
/// A top-level bracketed `log[..]` statement: its return value is never printed, since the write to stdout
/// already happened as a side effect.
fn is_bracket_log_call(expr: &Expr) -> bool {
    matches!(expr, Expr::Call { func, .. } if func == "log")
}

/// Compile one top-level statement, finishing it with `STORE` (already
/// internal to `compile_stmt` for `Assign`/`ScalarAssign`), `EMIT`, or `POP`
/// — except when `leave_value` (the final statement of a
/// `CompileMode::Result` program), which leaves an expression statement's
/// value on the stack instead of `EMIT`-ing it (an assignment still just
/// `STORE`s: its net stack effect is already zero, which is exactly what
/// `Vm::run_compiled` reduces to `EvalResult::Stored`, so no special case is
/// needed there for either mode).
fn compile_program_stmt(
    stmt: &Stmt,
    out: &mut Program,
    ctx: &CompileCtx,
    leave_value: bool,
) -> Result<(), QplError> {
    match stmt {
        // `sink` is the one `TableExpr` shape that's terminal: it consumes
        // the frame (writes it to a file) and leaves *nothing* on the stack
        // (`Op::Sink`, unlike every other frame op, pushes no result) — so
        // unlike an ordinary table-producing statement there is no value
        // here for `EMIT` to print (or for `leave_value` to preserve).
        Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Sink { .. })) => {
            compile_stmt(stmt, out)?;
        }
        Stmt::RetTable(_) => {
            compile_stmt(stmt, out)?;
            if !leave_value {
                out.emit(Op::Emit);
            }
        }
        Stmt::Assign { .. } | Stmt::ScalarAssign { .. } => {
            compile_stmt(stmt, out)?;
        }
        Stmt::SingleVar(expr) if is_bracket_log_call(expr) => {
            compile_value_expr(expr, out)?;
            out.emit(Op::Pop);
        }
        Stmt::SingleVar(_) => {
            compile_stmt(stmt, out)?;
            if !leave_value {
                out.emit(Op::Emit);
            }
        }
        Stmt::Log(args) => {
            compile_value_expr(
                &Expr::Call {
                    func: "log".to_string(),
                    args: args.clone(),
                },
                out,
            )?;
            out.emit(Op::Pop);
        }
        Stmt::Cfg(args) => {
            out.push_operand(Operand::Value(Value::Str(args.clone())));
            out.push_operand(Operand::Count(1));
            out.push_operand(Operand::Native(NativeId::Cfg));
            out.emit(Op::Call);
            out.emit(Op::Pop);
        }
        Stmt::System { cmd, arg } => {
            if ctx.mode == CompileMode::Result {
                return Err(QplError::Compile(format!(
                    "'\\{cmd}' is not available here"
                )));
            }
            compile_system(*cmd, arg, out, ctx)?;
            out.emit(Op::Pop);
        }
    }
    Ok(())
}

/// Compile a `\`-system command (`Stmt::System`) into a native call that
/// leaves a `Noop` on the stack (popped by the caller, `compile_program_stmt`).
fn compile_system(
    cmd: char,
    arg: &str,
    out: &mut Program,
    ctx: &CompileCtx,
) -> Result<(), QplError> {
    match cmd {
        '1' => {
            out.push_operand(Operand::Value(Value::Str(arg.to_string())));
            out.push_operand(Operand::Count(1));
            out.push_operand(Operand::Native(NativeId::StdoutLog));
            out.emit(Op::Call);
        }
        'd' => {
            let inner_tokens = crate::lexer::tokenise(arg)?;
            let inner_stmt = crate::parser::parse(inner_tokens)?;
            let inner_prog = compile(&inner_stmt)?;
            let listing = crate::program::disassemble(&inner_prog).join("\n");
            out.push_operand(Operand::Value(Value::Str(listing)));
            out.push_operand(Operand::Count(1));
            out.push_operand(Operand::Native(NativeId::PrintText));
            out.emit(Op::Call);
        }
        'l' => {
            // `\l` loads flat into whatever scope is *currently* compiling —
            // the top level (no namespace) ordinarily, but the enclosing
            // namespace when this `\l` itself sits inside an `\i`-imported
            // script (the namespace applies to the whole imported script,
            // nested `\l`s included).
            let resolved = script_relative(arg, &ctx.script_path);
            let sub = compile_included_script(&resolved, ctx.ns.clone(), ctx)?;
            out.push_operand(Operand::Program(Arc::new(sub)));
            out.push_operand(Operand::Count(1));
            out.push_operand(Operand::Native(NativeId::LoadScript));
            out.emit(Op::Call);
        }
        'i' => {
            let resolved = script_relative(arg, &ctx.script_path);
            let ns = namespace_from_path(&resolved);
            let sub = compile_included_script(&resolved, Some(ns.clone()), ctx)?;
            out.push_operand(Operand::Program(Arc::new(sub)));
            out.push_operand(Operand::Value(Value::Str(ns)));
            out.push_operand(Operand::Count(2));
            out.push_operand(Operand::Native(NativeId::ImportScript));
            out.emit(Op::Call);
        }
        other => {
            return Err(QplError::Compile(format!(
                "unknown system command '\\{other}'"
            )));
        }
    }
    Ok(())
}

/// Read, parse and compile a `\l`/`\i` target at compile time, detecting
/// an inclusion cycle before ever reading the file a second time: `path` is
/// canonicalized and checked against `ctx.including` (the chain of scripts
/// currently being compiled, root first).
fn compile_included_script(
    path: &str,
    ns: Option<String>,
    ctx: &CompileCtx,
) -> Result<Program, QplError> {
    let canon = canonical_path(path);
    if ctx.including.contains(&canon) {
        let mut chain = ctx.including.clone();
        chain.push(canon);
        return Err(QplError::Compile(format!(
            "\\l/\\i cycle detected: {}",
            chain.join(" -> ")
        )));
    }
    let src = std::fs::read_to_string(path)
        .map_err(|e| QplError::Compile(format!("cannot read '{path}': {e}")))?;
    let stmts = crate::parser::parse_program(&src, path)?;
    let mut including = ctx.including.clone();
    including.push(canon);
    let sub_ctx = CompileCtx {
        mode: CompileMode::Script,
        script_path: path.to_string(),
        ns,
        including,
    };
    compile_program(stmts, sub_ctx)
}

// ── Compile-time namespace qualification ─────────────────────────────

/// The names a `\i`-imported file binds at its own top level (functions
/// included — a function is an ordinary global holding a `Value::Closure`).
/// Every bare reference to one of these, anywhere in the file (including
/// inside function bodies, unless shadowed by a param/local there), gets
/// qualified to `.ns.name`; an already-namespaced name (`.other.x`) is
/// skipped.
fn collect_top_level_names(stmts: &[(u32, Stmt)], out: &mut HashSet<String>) {
    for (_, s) in stmts {
        if let Stmt::Assign { name, .. } | Stmt::ScalarAssign { name, .. } = s
            && !name.starts_with('.')
        {
            out.insert(name.clone());
        }
    }
}

/// The full set of names that end up namespaced under the current `ns`: this
/// file's own top-level assignments, plus — recursively — any `\l` target's
/// own top-level assignments (a bare `\l` inherits whatever namespace is
/// already active, see `compile_system`'s `'l'` arm, so its bindings are part
/// of the *same* namespace and must be qualifiable from anywhere in the
/// including file, even from a reference written before the `\l` line).
/// `\i` targets are never walked here — each gets its own fresh namespace,
/// entirely separate from `ns`.
fn collect_ns_names(stmts: &[(u32, Stmt)], script_path: &str) -> Result<HashSet<String>, QplError> {
    let mut out = HashSet::new();
    collect_ns_names_into(stmts, script_path, &mut out)?;
    Ok(out)
}

fn collect_ns_names_into(
    stmts: &[(u32, Stmt)],
    script_path: &str,
    out: &mut HashSet<String>,
) -> Result<(), QplError> {
    collect_top_level_names(stmts, out);
    for (_, s) in stmts {
        if let Stmt::System { cmd: 'l', arg } = s {
            let resolved = script_relative(arg, script_path);
            let src = std::fs::read_to_string(&resolved)
                .map_err(|e| QplError::Compile(format!("cannot read '{resolved}': {e}")))?;
            let sub_stmts = crate::parser::parse_program(&src, &resolved)?;
            collect_ns_names_into(&sub_stmts, &resolved, out)?;
        }
    }
    Ok(())
}

/// Qualify one top-level statement: its own assignment target (if it has
/// one) gets renamed to `.ns.name`, and every bare reference anywhere inside
/// it (there is no enclosing function at the top level, so `locals` is
/// empty) gets qualified per [`qualify_name`].
fn qualify_top_level(stmt: &mut Stmt, ns: &str, names: &HashSet<String>) {
    let empty = HashSet::new();
    qualify_stmt_refs(stmt, ns, names, &empty);
    if let Stmt::Assign { name, .. } | Stmt::ScalarAssign { name, .. } = stmt
        && !name.starts_with('.')
    {
        *name = format!("{ns}.{name}");
    }
}

/// Qualify `name` in place iff it names a top-level binding of the imported
/// file (`names`) and isn't shadowed by a param/local of the enclosing
/// function (`locals`) An already-namespaced name is left alone.
fn qualify_name(name: &mut String, ns: &str, names: &HashSet<String>, locals: &HashSet<String>) {
    if !name.starts_with('.') && !locals.contains(name.as_str()) && names.contains(name.as_str()) {
        *name = format!("{ns}.{name}");
    }
}

fn qualify_stmt_refs(stmt: &mut Stmt, ns: &str, names: &HashSet<String>, locals: &HashSet<String>) {
    match stmt {
        Stmt::RetTable(te) => qualify_table_expr(te, ns, names, locals),
        Stmt::Assign { body, .. } => qualify_stmt_refs(body, ns, names, locals),
        Stmt::ScalarAssign { expr, .. } => qualify_expr(expr, ns, names, locals),
        Stmt::SingleVar(e) => qualify_expr(e, ns, names, locals),
        Stmt::Log(args) => {
            for a in args {
                qualify_expr(a, ns, names, locals);
            }
        }
        Stmt::Cfg(_) | Stmt::System { .. } => {}
    }
}

fn qualify_table_expr(
    te: &mut TableExpr,
    ns: &str,
    names: &HashSet<String>,
    locals: &HashSet<String>,
) {
    match te {
        TableExpr::Select(sel) => qualify_select(sel, ns, names, locals),
        TableExpr::BuiltIn(b) => qualify_builtin(b, ns, names, locals),
        TableExpr::Source(src) => qualify_source(src, ns, names, locals),
    }
}

fn qualify_source(
    src: &mut TableSource,
    ns: &str,
    names: &HashSet<String>,
    locals: &HashSet<String>,
) {
    match src {
        TableSource::InMem(name) => qualify_name(name, ns, names, locals),
        TableSource::Load(e) => qualify_expr(e, ns, names, locals),
    }
}

fn qualify_select(
    sel: &mut SelectStmt,
    ns: &str,
    names: &HashSet<String>,
    locals: &HashSet<String>,
) {
    for alias in &mut sel.cols {
        qualify_expr(&mut alias.expr, ns, names, locals);
    }
    qualify_table_expr(&mut sel.from, ns, names, locals);
    if let Some(by) = &mut sel.by {
        for alias in by {
            qualify_expr(&mut alias.expr, ns, names, locals);
        }
    }
    if let Some(preds) = &mut sel.where_ {
        for e in preds {
            qualify_expr(e, ns, names, locals);
        }
    }
    if let Some((te, _left_keys, _right_keys, _kind)) = &mut sel.join {
        // the join keys are column-name symbols (like `Sort`'s column list),
        // never variable references, so only the joined table expression
        // itself needs qualifying.
        qualify_table_expr(te, ns, names, locals);
    }
}

fn qualify_builtin(b: &mut BuiltIn, ns: &str, names: &HashSet<String>, locals: &HashSet<String>) {
    match b {
        BuiltIn::Cols(te)
        | BuiltIn::Sort(te, _)
        | BuiltIn::Distinct(te)
        | BuiltIn::DropNull(_, te)
        | BuiltIn::Drop(_, te)
        | BuiltIn::Lazy(te)
        | BuiltIn::Collect(te) => qualify_table_expr(te, ns, names, locals),
        BuiltIn::Sink { src, path } => {
            qualify_table_expr(src, ns, names, locals);
            qualify_expr(path, ns, names, locals);
        }
        BuiltIn::Limit(te, n) => {
            qualify_table_expr(te, ns, names, locals);
            qualify_expr(n, ns, names, locals);
        }
    }
}

fn qualify_expr(e: &mut Expr, ns: &str, names: &HashSet<String>, locals: &HashSet<String>) {
    match e {
        Expr::Lit(_) | Expr::Sym(_) | Expr::IColRef | Expr::Noop => {}
        Expr::ColRef(name) => qualify_name(name, ns, names, locals),
        Expr::Dict(pairs) => {
            for (_, v) in pairs {
                qualify_expr(v, ns, names, locals);
            }
        }
        Expr::BinOp { left, right, .. } => {
            qualify_expr(left, ns, names, locals);
            qualify_expr(right, ns, names, locals);
        }
        Expr::Call { func, args } => {
            qualify_name(func, ns, names, locals);
            for a in args {
                qualify_expr(a, ns, names, locals);
            }
        }
        Expr::Cast { target, expr } => {
            if let CastTarget::Enum(name) = target {
                qualify_name(name, ns, names, locals);
            }
            qualify_expr(expr, ns, names, locals);
        }
        Expr::Case { branches, default } => {
            for (c, v) in branches {
                qualify_expr(c, ns, names, locals);
                qualify_expr(v, ns, names, locals);
            }
            qualify_expr(default, ns, names, locals);
        }
        Expr::Window { func, .. } => qualify_expr(func, ns, names, locals),
        Expr::Table(te) => qualify_table_expr(te, ns, names, locals),
        Expr::Take { n, expr } => {
            qualify_expr(n, ns, names, locals);
            qualify_expr(expr, ns, names, locals);
        }
        Expr::Index { expr, idx } => {
            qualify_expr(expr, ns, names, locals);
            qualify_expr(idx, ns, names, locals);
        }
        Expr::Apply { func, args } => {
            qualify_expr(func, ns, names, locals);
            for a in args {
                qualify_expr(a, ns, names, locals);
            }
        }
        Expr::Lambda(f) => qualify_function(f, ns, names),
        Expr::Dispatch { conn, .. } => qualify_expr(conn, ns, names, locals),
        Expr::While { cond, body } => {
            qualify_expr(cond, ns, names, locals);
            for st in body {
                qualify_stmt_refs(st, ns, names, locals);
            }
        }
        Expr::ListWhere { list, where_ } => {
            qualify_expr(list, ns, names, locals);
            for e in where_ {
                qualify_expr(e, ns, names, locals);
            }
        }
    }
}

/// Qualify a function literal's body against its *own* fresh scope: params
/// plus every name the body itself assigns (at any depth, excluding a nested
/// lambda's own body — a lambda never captures an enclosing scope, see
/// `ast::Function`'s doc comment) are locals, so a reference to one of them
/// is never rewritten even if it happens to share a name with the imported
/// file's own top-level binding (the explicit "a param shadowing a
/// namespaced name" case from the plan's test list).
fn qualify_function(f: &mut Function, ns: &str, names: &HashSet<String>) {
    let mut locals: HashSet<String> = f.params.iter().cloned().collect();
    collect_locals(&f.body, &mut locals);
    for st in &mut f.body {
        qualify_stmt_refs(st, ns, names, &locals);
    }
}

fn collect_locals(body: &[Stmt], out: &mut HashSet<String>) {
    for st in body {
        collect_locals_stmt(st, out);
    }
}

fn collect_locals_stmt(st: &Stmt, out: &mut HashSet<String>) {
    match st {
        Stmt::Assign { name, body } => {
            out.insert(name.clone());
            collect_locals_stmt(body, out);
        }
        Stmt::ScalarAssign { name, expr } => {
            out.insert(name.clone());
            collect_locals_expr(expr, out);
        }
        Stmt::RetTable(_) => {}
        Stmt::SingleVar(e) => collect_locals_expr(e, out),
        Stmt::Log(args) => {
            for a in args {
                collect_locals_expr(a, out);
            }
        }
        Stmt::Cfg(_) | Stmt::System { .. } => {}
    }
}

/// Only `While`'s body (which shares its enclosing function's scope) and
/// nested sub-expressions can introduce further assignments; a `Lambda` gets
/// its own fresh scope (see [`qualify_function`]) so it's never descended
/// into here.
fn collect_locals_expr(e: &Expr, out: &mut HashSet<String>) {
    match e {
        Expr::While { cond, body } => {
            collect_locals_expr(cond, out);
            collect_locals(body, out);
        }
        Expr::Lambda(_) | Expr::Lit(_) | Expr::Sym(_) | Expr::IColRef | Expr::Noop => {}
        Expr::ColRef(_) => {}
        Expr::Dict(pairs) => {
            for (_, v) in pairs {
                collect_locals_expr(v, out);
            }
        }
        Expr::BinOp { left, right, .. } => {
            collect_locals_expr(left, out);
            collect_locals_expr(right, out);
        }
        Expr::Call { args, .. } => {
            for a in args {
                collect_locals_expr(a, out);
            }
        }
        Expr::Cast { expr, .. } => collect_locals_expr(expr, out),
        Expr::Case { branches, default } => {
            for (c, v) in branches {
                collect_locals_expr(c, out);
                collect_locals_expr(v, out);
            }
            collect_locals_expr(default, out);
        }
        Expr::Window { func, .. } => collect_locals_expr(func, out),
        Expr::Table(_) => {}
        Expr::Take { n, expr } => {
            collect_locals_expr(n, out);
            collect_locals_expr(expr, out);
        }
        Expr::Index { expr, idx } => {
            collect_locals_expr(expr, out);
            collect_locals_expr(idx, out);
        }
        Expr::Apply { func, args } => {
            collect_locals_expr(func, out);
            for a in args {
                collect_locals_expr(a, out);
            }
        }
        Expr::Dispatch { conn, .. } => collect_locals_expr(conn, out),
        Expr::ListWhere { list, where_ } => {
            collect_locals_expr(list, out);
            for e in where_ {
                collect_locals_expr(e, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenise;
    use crate::parser::parse;
    use crate::program::disassemble;

    fn dis(src: &str) -> Vec<String> {
        let tokens = tokenise(src).expect("lex error");
        let stmt = parse(tokens).expect("parse error");
        let prog = compile(&stmt).expect("compile error");
        disassemble(&prog)
    }

    fn compile_err(src: &str) -> QplError {
        let tokens = tokenise(src).expect("lex error");
        let stmt = parse(tokens).expect("parse error");
        compile(&stmt).expect_err("expected a compile error")
    }

    fn has_op(lines: &[String], op: &str) -> bool {
        lines.iter().any(|l| l.ends_with(&format!("  {op}")))
    }

    fn last_is(lines: &[String], op: &str) -> bool {
        lines
            .last()
            .is_some_and(|l| l.ends_with(&format!("  {op}")))
    }

    // --- simple selects ---

    #[test]
    fn select_single_col() {
        assert_eq!(
            dis("select px from trades"),
            vec![
                "0000  PUSH       Name(trades)",
                "0001  SOURCE",
                "0002  PUSH       Name(px)",
                "0003  LOAD_COL",
                "0004  PUSH       Name(px)",
                "0005  ALIAS",
                "0006  PUSH       Count(1)",
                "0007  LIST",
                "0008  SELECT",
            ]
        );
    }

    #[test]
    fn select_all_cols() {
        // empty select phrase = return all columns
        assert_eq!(
            dis("select from t"),
            vec![
                "0000  PUSH       Name(t)",
                "0001  SOURCE",
                "0002  PUSH       Count(0)",
                "0003  LIST",
                "0004  SELECT",
            ]
        );
    }

    #[test]
    fn explicit_alias() {
        assert_eq!(
            dis("select p: price from trades"),
            vec![
                "0000  PUSH       Name(trades)",
                "0001  SOURCE",
                "0002  PUSH       Name(price)",
                "0003  LOAD_COL",
                "0004  PUSH       Name(p)",
                "0005  ALIAS",
                "0006  PUSH       Count(1)",
                "0007  LIST",
                "0008  SELECT",
            ]
        );
    }

    // --- virtual column i ---

    #[test]
    fn select_icol_inserts_row_index_after_source() {
        // i is virtual, never a real column name → implicit alias is "x".
        // The query references `i`, so the compiler inserts a `RowIndex`
        // right after the `Source`.
        assert_eq!(
            dis("select i from t"),
            vec![
                "0000  PUSH       Name(t)",
                "0001  SOURCE",
                "0002  ROW_INDEX",
                "0003  LOAD_ROWIDX",
                "0004  PUSH       Name(x)",
                "0005  ALIAS",
                "0006  PUSH       Count(1)",
                "0007  LIST",
                "0008  SELECT",
            ]
        );
    }

    #[test]
    fn no_row_index_when_i_is_not_referenced() {
        assert!(!dis("select px from trades").contains(&"0002  ROW_INDEX".to_string()));
    }

    // --- where clause ---

    #[test]
    fn where_single_pred() {
        assert_eq!(
            dis("select px from trades where qty > 0"),
            vec![
                "0000  PUSH       Name(trades)",
                "0001  SOURCE",
                "0002  PUSH       Name(qty)",
                "0003  LOAD_COL",
                "0004  PUSH       Value(Int(0))",
                "0005  PUSH       BinOp(Gt)",
                "0006  BINOP",
                "0007  PUSH       Count(1)",
                "0008  FILTER",
                "0009  PUSH       Name(px)",
                "0010  LOAD_COL",
                "0011  PUSH       Name(px)",
                "0012  ALIAS",
                "0013  PUSH       Count(1)",
                "0014  LIST",
                "0015  SELECT",
            ]
        );
    }

    #[test]
    fn order_multiple_columns() {
        let lines = dis("select from trades order `col1 asc, `col2 desc");
        assert!(last_is(&lines, "SORT"), "expected trailing SORT op");
    }

    #[test]
    fn distinct_and_limit() {
        assert!(has_op(&dis("distinct select from trades"), "DISTINCT"));
        assert!(has_op(&dis("10 limit select from trades"), "LIMIT"));
    }

    #[test]
    fn update_preserves_table_shape() {
        assert_eq!(
            dis("update price: price * 2 from trades"),
            vec![
                "0000  PUSH       Name(trades)",
                "0001  SOURCE",
                "0002  PUSH       Count(0)",
                "0003  LIST",
                "0004  PUSH       Name(price)",
                "0005  LOAD_COL",
                "0006  PUSH       Value(Int(2))",
                "0007  PUSH       BinOp(Mul)",
                "0008  BINOP",
                "0009  PUSH       Name(price)",
                "0010  ALIAS",
                "0011  PUSH       Names([\"price\"])",
                "0012  PUSH       Count(0)",
                "0013  PUSH       Count(1)",
                "0014  UPDATE",
            ]
        );
    }

    #[test]
    fn delete_rows_uses_negated_filter_and_update() {
        let lines = dis("delete from trades where size > 100");
        assert!(has_op(&lines, "FILTER"));
        assert!(has_op(&lines, "UPDATE"));
        assert!(lines.iter().any(|l| l.contains("Verb(not)")));
    }

    #[test]
    fn lazy_prefixes_the_plan_and_stays_uncollected() {
        let lines = dis("l: lazy select from trades");
        assert!(has_op(&lines, "LAZY"));
        assert!(last_is(&lines, "STORE"));
    }

    #[test]
    fn collect_appends_a_collect_instruction() {
        let lines = dis("tm: collect t");
        assert!(has_op(&lines, "COLLECT"));
    }

    #[test]
    fn func_def_compiles_directly_as_a_literal_value() {
        // a closure literal parses to `Expr::Lambda` (see ast.rs), compiled
        // to `PUSH Func(proto)`, with the body appended after the main 3-instruction
        // sequence (`PUSH Func; PUSH Name(f); STORE`).
        let lines = dis("f: {[x,y] x+y}");
        assert!(lines[0].contains("PUSH       Func({[x,y] ..})"));
        assert_eq!(lines[1], "0001  PUSH       Name(f)");
        assert_eq!(lines[2], "0002  STORE");
        // the body (x+y; RET) is appended after the 3 main instructions.
        assert!(has_op(&lines, "RET"));
        assert!(lines.len() > 3);
        // a statement that defines a closure ends its own main code with a
        // `HALT` before the appended body, so execution never falls off the
        // end of `STORE` straight into it (see `Op::Halt`'s doc comment).
        assert_eq!(lines[3], "0003  HALT");
    }

    #[test]
    fn apply_with_two_args_compiles_to_load_fn_and_call() {
        // `f[a;b]`: args are compiled first,
        // then the callee (`LOAD_FN` for a bare name, deferring to `CALL`'s
        // own runtime dispatch), then `CALL`.
        let lines = dis("f[1;2]");
        assert_eq!(
            lines,
            vec![
                "0000  PUSH       Value(Int(1))",
                "0001  PUSH       Value(Int(2))",
                "0002  PUSH       Count(2)",
                "0003  PUSH       Name(f)",
                "0004  LOAD_FN",
                "0005  CALL",
            ]
        );
    }

    #[test]
    fn apply_with_no_args_compiles_to_call() {
        let lines = dis("f[]");
        assert_eq!(
            lines,
            vec![
                "0000  PUSH       Count(0)",
                "0001  PUSH       Name(f)",
                "0002  LOAD_FN",
                "0003  CALL",
            ]
        );
    }

    #[test]
    fn func_body_ending_in_an_assignment_is_rejected() {
        assert!(matches!(
            parse(tokenise("f: {[x] y: x+1}").unwrap()),
            Err(QplError::Parse(_)),
        ));
    }

    #[test]
    fn sink_is_terminal_without_trailing_select_or_result() {
        let lines = dis("t sink \"out.parquet\"");
        assert!(last_is(&lines, "SINK"));
    }

    #[test]
    fn sink_after_a_select_compiles() {
        let lines = dis("select price from trades sink \"out.parquet\"");
        assert!(last_is(&lines, "SINK"));
    }

    #[test]
    fn from_compiles_a_nested_select() {
        // the outer select's own `select`/`from` compiles around whatever
        // instructions the inner select compiles to.
        let lines = dis("select from select price from trades");
        assert_eq!(lines.iter().filter(|l| l.ends_with("  SELECT")).count(), 2);
    }

    #[test]
    fn join_right_side_compiles_a_parenthesised_table_expr() {
        // a parenthesised join RHS recurses through compile_tbl_expr, so
        // `(distinct quotes)` compiles its own DISTINCT before the join is
        // built, same as any other nested table expression.
        let lines = dis("select price from trades `sym lj (distinct quotes) `sym");
        assert!(has_op(&lines, "DISTINCT"));
        assert!(has_op(&lines, "JOIN"));
    }

    #[test]
    fn case_expression_compiles() {
        let lines = dis("select bin: ?[c2>20;`high;c2>10;`mid;`low] from t");
        let case_idx = lines
            .iter()
            .position(|l| l.ends_with("  CASE"))
            .expect("CASE opcode emitted");
        // the operand pushed right before CASE carries the branch count (2)
        assert!(lines[case_idx - 1].contains("Count(2)"));
    }

    #[test]
    fn round_compiles_to_round_instruction_with_precision() {
        assert_eq!(
            dis("select r: 2 round px from t"),
            vec![
                "0000  PUSH       Name(t)",
                "0001  SOURCE",
                "0002  PUSH       Name(px)",
                "0003  LOAD_COL",
                "0004  PUSH       Count(2)",
                "0005  ROUND",
                "0006  PUSH       Name(r)",
                "0007  ALIAS",
                "0008  PUSH       Count(1)",
                "0009  LIST",
                "0010  SELECT",
            ]
        );
    }

    #[test]
    fn round_with_non_literal_precision_is_compile_error() {
        assert!(matches!(
            compile_err("select r: sz round px from t"),
            QplError::Compile(_)
        ));
    }

    // --- window functions ---

    #[test]
    fn window_over_compiles_the_target_then_a_window_instruction() {
        let lines = dis("select m: max px over `s from t");
        let window_idx = lines
            .iter()
            .position(|l| l.ends_with("  WINDOW"))
            .expect("WINDOW opcode emitted");
        assert!(lines[window_idx - 1].contains("Window("));
        // the target aggregate (`max px`) is compiled ahead of the window op
        assert!(lines.iter().any(|l| l.contains("Verb(max)")));
    }

    #[test]
    fn window_ranking_verb_emits_only_the_window_instruction() {
        // a bare ranking verb (`rn`) synthesises its own expression from the
        // window order, so nothing but the operand precedes WINDOW itself.
        let lines = dis("select r: rn over `s order `px desc from t");
        assert_eq!(lines[0], "0000  PUSH       Name(t)");
        assert_eq!(lines[1], "0001  SOURCE");
        assert!(lines[2].starts_with("0002  PUSH       Window("));
        assert_eq!(lines[3], "0003  WINDOW");
        assert_eq!(lines[4], "0004  PUSH       Name(r)");
        assert_eq!(lines[5], "0005  ALIAS");
        assert_eq!(lines[6], "0006  PUSH       Count(1)");
        assert_eq!(lines[7], "0007  LIST");
        assert_eq!(lines[8], "0008  SELECT");
    }

    #[test]
    fn window_ranking_verb_without_order_is_compile_error() {
        assert!(matches!(
            compile_err("select r: rank over `s from t"),
            QplError::Compile(_)
        ));
    }

    #[test]
    fn rolling_window_pushes_raw_column_and_carries_agg_name() {
        let lines = dis("select r: sum px over `s order `ts asc rolling 3 from t");
        assert!(lines.iter().any(|l| l.contains("rolling: Some((\"sum\"")));
    }

    // --- by clause ---

    #[test]
    fn by_single_key() {
        let lines = dis("select sum px by sym from trades");
        assert!(last_is(&lines, "SELECT_BY"));
        assert!(lines.iter().any(|l| l.contains("Verb(sum)")));
    }

    #[test]
    fn by_key_reprojected_by_name_is_deduped() {
        // `group_by(keys).agg(proj)` already carries the key columns through;
        // re-projecting `sym` under its own name would hand Polars two columns
        // named `sym`, so the compiler drops it from the projection phase.
        let lines = dis("select sym, price, ret: 1 diff price by sym from trades");
        // projection list should build only 2 columns (price, ret), not 3;
        // the operand right before the trailing SELECT_BY's LIST is that count.
        let list_idx = lines
            .iter()
            .rposition(|l| l.ends_with("  LIST"))
            .expect("LIST opcode emitted");
        assert!(lines[list_idx - 1].contains("Count(2)"));
    }

    // --- full example from spec ---

    #[test]
    fn full_query() {
        // select dbl: c3*2 by c1 from t where c2>15
        let lines = dis("select dbl: c3*2 by c1 from t where c2>15");
        assert!(last_is(&lines, "SELECT_BY"));
        assert!(has_op(&lines, "FILTER"));
    }

    // --- value context ---

    #[test]
    fn scalar_assign_of_a_literal_compiles_directly() {
        assert_eq!(
            dis("x: 1"),
            vec![
                "0000  PUSH       Value(Int(1))",
                "0001  PUSH       Name(x)",
                "0002  STORE",
            ]
        );
    }

    #[test]
    fn scalar_assign_of_a_binop_compiles_to_binop_not_eval() {
        assert_eq!(
            dis("x: 1+2"),
            vec![
                "0000  PUSH       Value(Int(1))",
                "0001  PUSH       Value(Int(2))",
                "0002  PUSH       BinOp(Add)",
                "0003  BINOP",
                "0004  PUSH       Name(x)",
                "0005  STORE",
            ]
        );
    }

    #[test]
    fn scalar_assign_of_a_bare_name_compiles_to_load() {
        assert_eq!(
            dis("x: y"),
            vec![
                "0000  PUSH       Name(y)",
                "0001  LOAD",
                "0002  PUSH       Name(x)",
                "0003  STORE",
            ]
        );
    }

    #[test]
    fn value_context_cast_compiles_directly() {
        assert_eq!(
            dis("x: int$1.5"),
            vec![
                "0000  PUSH       Value(Float(1.5))",
                "0001  PUSH       Cast(Prim(\"int\"))",
                "0002  CAST",
                "0003  PUSH       Name(x)",
                "0004  STORE",
            ]
        );
    }

    #[test]
    fn binop_falls_back_only_for_its_call_operand() {
        // `n * fac[n-1]` compiles the multiplication directly; `fac[n-1]`
        // parses as `Index` (a single bracketed argument, no `;`), which
        // compiles to `LOAD_FN`/`INDEX`, since `fac` might be callable.
        let lines = dis("x: n * fac[n-1]");
        assert_eq!(lines[0], "0000  PUSH       Name(n)");
        assert_eq!(lines[1], "0001  LOAD");
        assert_eq!(lines[2], "0002  PUSH       Name(fac)");
        assert_eq!(lines[3], "0003  LOAD_FN");
        assert!(has_op(&lines, "INDEX"));
        assert!(!has_op(&lines, "EVAL"));
        assert!(has_op(&lines, "BINOP"));
    }

    #[test]
    fn while_and_noop_are_rejected_inside_a_select() {
        for src in ["select noop from t", "select a: while[1b; 2] from t"] {
            let err = compile_err(src);
            assert!(
                err.to_string().contains("only valid outside a select"),
                "{err}"
            );
        }
    }

    // --- natives / verbs in value context ---

    #[test]
    fn enlist_compiles_to_a_native_call() {
        // a literal `enlist 5` is constant-folded by the parser, so use a
        // non-literal operand to see the actual lowering.
        let lines = dis("x: enlist n");
        assert_eq!(
            lines,
            vec![
                "0000  PUSH       Name(n)",
                "0001  LOAD",
                "0002  PUSH       Count(1)",
                "0003  PUSH       Native(Enlist)",
                "0004  CALL",
                "0005  PUSH       Name(x)",
                "0006  STORE",
            ]
        );
    }

    #[test]
    fn roll_compiles_to_a_native_call() {
        let lines = dis("x: 3?6");
        assert_eq!(
            lines,
            vec![
                "0000  PUSH       Value(Int(6))",
                "0001  PUSH       Value(Int(3))",
                "0002  PUSH       Count(2)",
                "0003  PUSH       Native(Roll)",
                "0004  CALL",
                "0005  PUSH       Name(x)",
                "0006  STORE",
            ]
        );
    }

    #[test]
    fn generic_call_compiles_a_name_operand_not_ast() {
        // `til 5` — `til` has no dedicated opcode: `Op::Call` resolves it by
        // name at run time (`ops::call_by_name`), so a user function named
        // `til` can shadow it (see the precedence test in `vm.rs`).
        let lines = dis("x: til 5");
        assert_eq!(
            lines,
            vec![
                "0000  PUSH       Value(Int(5))",
                "0001  PUSH       Count(1)",
                "0002  PUSH       Name(til)",
                "0003  CALL",
                "0004  PUSH       Name(x)",
                "0005  STORE",
            ]
        );
    }

    #[test]
    fn take_compiles_directly() {
        let lines = dis("x: 3#lst");
        assert_eq!(
            lines,
            vec![
                "0000  PUSH       Value(Int(3))",
                "0001  PUSH       Name(lst)",
                "0002  LOAD",
                "0003  TAKE",
                "0004  PUSH       Name(x)",
                "0005  STORE",
            ]
        );
    }

    #[test]
    fn index_of_a_non_callable_expression_skips_load_fn() {
        // `(1 2 3)[1]` — the target isn't a bare name, so there's nothing to
        // defer: it compiles straight through, no `LOAD_FN`.
        let lines = dis("x: (1 2 3)[1]");
        assert!(!has_op(&lines, "LOAD_FN"));
        assert!(last_is(&lines, "STORE"));
        assert!(has_op(&lines, "INDEX"));
    }

    #[test]
    fn table_column_expression_appends_column() {
        let lines = dis("x: t`price");
        assert!(last_is(&lines, "STORE"));
        // `COLUMN` immediately precedes the `PUSH Name(x); STORE` pair.
        assert!(lines[lines.len() - 3].ends_with("  COLUMN"));
    }

    #[test]
    fn multi_column_table_expression_stays_a_frame() {
        // `` t`c1`c2 `` selects two columns — not a column expression, so no
        // trailing `COLUMN`.
        let lines = dis("x: t`c1`c2");
        assert!(!has_op(&lines, "COLUMN"));
    }

    #[test]
    fn zip_compiles_to_a_zip_instruction() {
        let lines = dis("x: zip `a`b!(1 2) (3 4)");
        assert!(has_op(&lines, "ZIP"));
        assert!(lines.iter().any(|l| l.contains("Names([\"a\", \"b\"])")));
    }

    #[test]
    fn zip_with_a_top_level_cast_uses_cast_list() {
        let lines = dis("x: zip `a!(i8$1 2 3)");
        assert!(has_op(&lines, "CAST_LIST"));
        assert!(has_op(&lines, "ZIP"));
    }

    #[test]
    fn zip_of_a_non_dict_is_a_compile_error() {
        assert!(matches!(compile_err("x: zip 5"), QplError::Runtime(_)));
    }

    #[test]
    fn list_where_compiles_to_the_documented_lowering() {
        let lines = dis("x: lst where x>1");
        assert!(has_op(&lines, "LIST_WHERE_FRAME"));
        assert!(has_op(&lines, "FILTER"));
        assert!(last_is(&lines, "STORE"));
        // `COLUMN` immediately precedes the `PUSH Name(x); STORE` pair.
        assert!(lines[lines.len() - 3].ends_with("  COLUMN"));
    }

    #[test]
    fn case_value_context_compiles_to_jumps_not_eval() {
        // value-context `?[..]` is jump-based bytecode sharing the VM's
        // ordinary stack.
        let lines = dis("x: ?[1b; 2; 3]");
        assert!(!has_op(&lines, "EVAL"));
        assert!(has_op(&lines, "JUMP_IF_VEC"));
        assert!(has_op(&lines, "JUMP_IF_FALSE"));
        assert!(has_op(&lines, "CASE_VEC"));
        assert!(has_op(&lines, "JUMP"));
        assert!(last_is(&lines, "STORE"));
    }

    #[test]
    fn case_value_context_exact_disassembly() {
        assert_eq!(
            dis("?[1b; 2; 3]"),
            vec![
                "0000  PUSH       Value(Bool(true))",
                "0001  PUSH       Target(ip=9,cp=6)",
                "0002  JUMP_IF_VEC",
                "0003  PUSH       Target(ip=15,cp=10)",
                "0004  PUSH       Text(a `?[..]` condition must be a boolean scalar or vector in value context)",
                "0005  JUMP_IF_FALSE",
                "0006  PUSH       Value(Int(2))",
                "0007  PUSH       Target(ip=16,cp=11)",
                "0008  JUMP",
                "0009  PUSH       Value(Int(2))",
                "0010  PUSH       Value(Int(3))",
                "0011  PUSH       Count(2)",
                "0012  CASE_VEC",
                "0013  PUSH       Target(ip=16,cp=11)",
                "0014  JUMP",
                "0015  PUSH       Value(Int(3))",
            ]
        );
    }

    #[test]
    fn while_compiles_to_jumps_not_eval() {
        // `while` is jump-based too: the loop is a backward `JUMP`, the test a
        // `JUMP_IF_FALSE`, and its own value a `NOOP`.
        let lines = dis("while[c>0; c: c-1]");
        assert!(!has_op(&lines, "EVAL"));
        assert!(has_op(&lines, "JUMP_IF_FALSE"));
        assert!(has_op(&lines, "JUMP"));
        assert!(has_op(&lines, "STORE"));
        assert!(last_is(&lines, "NOOP"));
    }

    #[test]
    fn while_exact_disassembly() {
        assert_eq!(
            dis("while[c>0; c: c-1]"),
            vec![
                "0000  PUSH       Name(c)",
                "0001  LOAD",
                "0002  PUSH       Value(Int(0))",
                "0003  PUSH       BinOp(Gt)",
                "0004  BINOP",
                "0005  PUSH       Target(ip=17,cp=10)",
                "0006  PUSH       Text(a `while` condition must be a boolean scalar in value context)",
                "0007  JUMP_IF_FALSE",
                "0008  PUSH       Name(c)",
                "0009  LOAD",
                "0010  PUSH       Value(Int(1))",
                "0011  PUSH       BinOp(Sub)",
                "0012  BINOP",
                "0013  PUSH       Name(c)",
                "0014  STORE",
                "0015  PUSH       Target(ip=0,cp=0)",
                "0016  JUMP",
                "0017  NOOP",
            ]
        );
    }

    #[test]
    fn dispatch_compiles_directly() {
        let lines = dis(r#"x: conn dispatch "select from t""#);
        assert_eq!(
            lines,
            vec![
                "0000  PUSH       Name(conn)",
                "0001  LOAD",
                "0002  PUSH       Text(\"select from t\")",
                "0003  PUSH       Value(Bool(false))",
                "0004  DISPATCH",
                "0005  PUSH       Name(x)",
                "0006  STORE",
            ]
        );
    }
}
