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

/// Append each queued function body after the code compiled so far and patch
/// its placeholder `Operand::Func` with the real entry point. Bodies may queue
/// nested lambdas; the loop runs until the queue is empty. Called once per
/// `Program` built from scratch.
pub(crate) fn finish_pending(out: &mut Program) -> Result<(), QplError> {
    if out.pending_closures.is_empty() {
        return Ok(());
    }
    // never fall off the statement's code into an appended body
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

/// A function body: leading statements for effect, then the final
/// expression's value and `RET`.
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
        // unreachable: the parser rejects a body ending in an assignment
        Stmt::Assign { .. } | Stmt::ScalarAssign { .. } => {
            return Err(QplError::Compile(
                "a function body must end with an expression".into(),
            ));
        }
        // unreachable: these only come from top-level parsing
        Stmt::Log(_) | Stmt::Cfg(_) | Stmt::System { .. } | Stmt::WriteFile { .. } => {
            return Err(QplError::Compile(
                "a `log`/`.qpl.cfg`/`\\`-command/`write0`/`write1` statement cannot end a function body".into(),
            ));
        }
    }
    out.emit(Op::Ret);
    Ok(())
}

fn compile_stmt(stmt: &Stmt, out: &mut Program) -> Result<(), QplError> {
    match stmt {
        // leaves the `Frame` on the stack (nothing for a terminal `sink`)
        Stmt::RetTable(tbl_expr) => compile_tbl_expr(tbl_expr, out),
        Stmt::Assign { name, body, .. } => {
            compile_stmt(body, out)?;
            out.push_operand(Operand::Name(name.as_str().into()));
            out.emit(Op::Store);
            Ok(())
        }
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
        Stmt::WriteFile { bytes, value, path } => compile_write_file(*bytes, value, path, out),
        // top-level only; handled by `compile_program_stmt`
        Stmt::Log(_) | Stmt::Cfg(_) | Stmt::System { .. } => Err(QplError::Compile(
            "a `log`/`.qpl.cfg`/`\\`-command statement is only valid at the top level".into(),
        )),
    }
}

/// Compile a table expression, leaving one `Frame` on the stack (`sink`
/// consumes it). If the tree references the virtual column `i`, a `RowIndex`
/// is inserted after each `Source`/`LoadFile`. Inserting a no-operand opcode
/// is safe since only `Push` reads the operand stream.
/// `<value> write0/write1 <path>`: pushes the native's `Noop` result, like a
/// call to any other niladic-returning native.
fn compile_write_file(
    bytes: bool,
    value: &Expr,
    path: &Expr,
    out: &mut Program,
) -> Result<(), QplError> {
    compile_value_expr(value, out)?;
    compile_value_expr(path, out)?;
    out.push_operand(Operand::Count(2));
    let id = if bytes {
        NativeId::Write1
    } else {
        NativeId::Write0
    };
    out.push_operand(Operand::Native(id));
    out.emit(Op::Call);
    Ok(())
}

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
            // the frame must be on the stack before `Lazy` flags it
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
        // `Update` expects a keys `List` (empty here) below its predicates
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
        // always emit a keys `List`, empty without `by`, so `Update`'s stack
        // shape is fixed
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

    // join first, so `where` filters the joined rows
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
        compile_tbl_expr_inner(&sel.from, out)?;
    }

    // each predicate is a successive filter, left to right
    if let Some(preds) = &sel.where_ {
        let n = preds.len();
        for expr in preds {
            compile_expr(expr, out)?;
        }
        out.push_operand(Operand::Count(n as u32));
        out.emit(Op::Filter);
    }

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

    // `group_by(keys).agg(proj)` already carries the keys, so skip a
    // projection that repeats a key under the same name
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

/// Emit `UPDATE`'s trailing operands (count, predicates, names) once the
/// exprs/list/preds/frame are on the stack.
fn emit_update(out: &mut Program, count: usize, names: &[String], predicates: usize) {
    out.push_operand(Operand::Names(names.to_vec().into()));
    out.push_operand(Operand::Count(predicates as u32));
    out.push_operand(Operand::Count(count as u32));
    out.emit(Op::Update);
}

/// `None` emits nothing.
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

/// A one-column `select` without `by`: a column expression that becomes a list.
pub(crate) fn is_column_select(te: &TableExpr) -> bool {
    matches!(te, TableExpr::Select(sel)
        if sel.cols.len() == 1 && sel.by.is_none() && !sel.update && !sel.delete)
}

/// Lower a value-context expression. Calls whose target isn't known until
/// run time go through `Op::Call`. A bare `Dict`/`IColRef`/`Window`, or a
/// call of an unsupported arity, is a compile error.
pub(crate) fn compile_value_expr(node: &Expr, out: &mut Program) -> Result<(), QplError> {
    match node {
        Expr::Lit(v) => out.push_operand(Operand::Value(v.clone())),
        // a symbol is its own value here (in a query it's a string literal)
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

        // `enlist x`: by id, so nothing can shadow it
        Expr::Call { func, args } if func == "enlist" && args.len() == 1 => {
            compile_value_expr(&args[0], out)?;
            out.push_operand(Operand::Count(1));
            out.push_operand(Operand::Native(NativeId::Enlist));
            out.emit(Op::Call);
        }

        // `3?6` / `2?10 20 30`: roll, also by id
        Expr::Call { func, args } if func == "?" && args.len() == 2 => {
            compile_value_expr(&args[0], out)?;
            compile_value_expr(&args[1], out)?;
            out.push_operand(Operand::Count(2));
            out.push_operand(Operand::Native(NativeId::Roll));
            out.emit(Op::Call);
        }

        // `log a b c` / `log[..]`: any arity. Dispatched by name at run time,
        // so a user function called `log` still wins.
        Expr::Call { func, args } if func == "log" => {
            for arg in args {
                compile_value_expr(arg, out)?;
            }
            out.push_operand(Operand::Count(args.len() as u32));
            out.push_operand(Operand::Name(func.as_str().into()));
            out.emit(Op::Call);
        }

        // `` zip `k1`k2!v1 v2 ``. A top-level cast on a column keeps its exact
        // width via `CastList`. A dict literal always means the builtin `zip`.
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

        // a one-column select becomes a list; anything else stays a frame
        Expr::Table(te) => {
            compile_tbl_expr(te, out)?;
            if is_column_select(te) {
                out.emit(Op::Column);
            }
        }

        // `<n>#<expr>`
        Expr::Take { n, expr } => {
            compile_value_expr(n, out)?;
            compile_value_expr(expr, out)?;
            out.emit(Op::Take);
        }

        // `f[x]` on a bare name or lambda: call or index, decided at run time.
        // Anything else is positional indexing.
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

        // push a placeholder `Func` now and queue the body; `finish_pending`
        // patches in the entry point
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

        // `f[a;b]` / `f[]`. Args are evaluated first, in the caller's frame,
        // then `func` is resolved like `Index`'s callable target.
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

        // `<conn> [async] dispatch <cmd>`
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

        // `<list> where <predicate>...`
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

        // a generic call (`sum t`c`, `2 shift px`, `til 5`, `f x`, ...),
        // resolved at run time by `ops::call_by_name`
        Expr::Call { func, args } if (1..=2).contains(&args.len()) => {
            // keep a `` t`col `` source as a frame so the verb reduces inside
            // the lazy plan; materialising it first would reject nulls a
            // reducer skips
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

        // `?[c1;v1;...;d]` (see `compile_case_value`)
        Expr::Case { branches, default } => compile_case_value(branches, default, out)?,

        Expr::While { cond, body } => compile_while(cond, body, out)?,

        Expr::Noop => out.emit(Op::Noop),

        // a bare `Dict`/`IColRef`/`Window`, or an unsupported call arity
        other => {
            return Err(QplError::Runtime(format!(
                "not supported in scalar context: {other:?}"
            )));
        }
    }
    Ok(())
}

/// Lower value-context `?[c1;v1;c2;v2;...;d]`. An atom condition
/// short-circuits (only the taken branch runs, so recursion terminates). The
/// first vector condition diverts to a tail that evaluates everything
/// remaining and folds it with `CASE_VEC`. Each branch value is compiled into
/// both paths, but only one runs.
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

        // atom true: this branch's value is the result
        compile_value_expr(val, out)?;
        let end_idx = out.operands.len();
        out.push_operand(Operand::Target { ip: 0, cp: 0 }); // Lend, patched once known
        out.emit(Op::Jump);
        end_patches.push(end_idx);

        // Lvec_k: the mask is still on the stack (JUMP_IF_VEC only peeks)
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

        // Lnext_k
        patch_target(out, next_idx);
    }
    // every condition was a false atom
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

/// Back-patch the placeholder `Operand::Target` at `idx` to the current end
/// of `out`. Call it exactly at the label's destination.
fn patch_target(out: &mut Program, idx: usize) {
    out.operands[idx] = Operand::Target {
        ip: out.code.len() as u32,
        cp: out.operands.len() as u32,
    };
}

/// Lower `while[test; s1; ...]`: re-test before each iteration, run the body
/// in the current scope, and yield `Noop`. Only the exit jump needs
/// back-patching.
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

/// Compile a statement for effect only, leaving the stack as it was (`STORE`
/// nets to zero; an expression is `POP`ped). Used by function and `while`
/// bodies.
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
        Stmt::WriteFile { bytes, value, path } => {
            compile_write_file(*bytes, value, path, out)?;
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
        // `<precision> round <col>`: args = [value, precision]. Precision must
        // be a literal; the rounding mode is read from config at run time.
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
            // `... rolling n`: push the raw column and carry the aggregate in
            // the operand, so the VM applies a rolling reduction
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
            // ranking verbs build their own expression from the window order;
            // anything else is applied per partition
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
        // value context only (see `compile_value_expr`)
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

// implicit alias: the leftmost column name in the expression, else "x"
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
// `compile_program` turns a parsed script (or REPL submission) into one
// `Program`, compiling `\l`/`\i` targets recursively and qualifying
// namespaces along the way.

/// `Script` prints each top-level statement's result (scripts, REPL lines).
/// `Result` leaves only the last value on the stack for an
/// [`crate::vm::EvalResult`] (IPC requests, tests).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CompileMode {
    Script,
    Result,
}

/// Context for [`compile_program`] and its recursive `\l`/`\i` embedding:
/// the path that relative includes and line numbers use, the active
/// namespace (`Some(".lib")` inside `\i "lib.qpl"`), and the chain of
/// canonical paths being compiled, for cycle detection.
#[derive(Clone)]
pub struct CompileCtx {
    pub mode: CompileMode,
    pub script_path: String,
    pub ns: Option<String>,
    including: Vec<String>,
}

impl CompileCtx {
    /// A script or REPL submission: prints each result, no namespace.
    pub fn script(path: &str) -> Self {
        Self {
            mode: CompileMode::Script,
            script_path: path.to_string(),
            ns: None,
            including: vec![canonical_path(path)],
        }
    }

    /// Keeps only the last statement's value (IPC server, tests).
    pub fn result(path: &str) -> Self {
        Self {
            mode: CompileMode::Result,
            script_path: path.to_string(),
            ns: None,
            including: vec![canonical_path(path)],
        }
    }
}

/// A stable key for cycle detection: the canonical path, or `path` itself if
/// it can't be canonicalised (e.g. `"<main>"`).
fn canonical_path(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Prefix `e` with `path:line:` as a `Runtime` error. Errors from `"<main>"`
/// (the REPL) and `Interrupted` are left alone.
pub fn wrap_line_error(e: QplError, path: &str, line: u32) -> QplError {
    if path == "<main>" || matches!(e, QplError::Interrupted) {
        return e;
    }
    QplError::Runtime(format!("{path}:{}: {e}", line + 1))
}

/// A `\l`/`\i` path inside a script is relative to that script's directory.
/// At the prompt (`"<main>"`), or if absolute, it's used as written.
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

/// A namespace from a file stem (`.utils`, `.my_lib`): non-identifier chars
/// become `_`, and a leading digit gets an `_` prefix.
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

/// Compile parsed top-level statements into one [`Program`]: qualify
/// namespaces, then compile each statement, recording a [`LineEntry`] so
/// runtime errors (including inside functions it defines) report the right
/// line.
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

/// Whether `expr` is syntactically a `log[..]` call. Its result isn't printed
/// at the top level, since the write already happened.
fn is_bracket_log_call(expr: &Expr) -> bool {
    matches!(expr, Expr::Call { func, .. } if func == "log")
}

/// Compile one top-level statement, ending in `STORE`, `EMIT` or `POP`. With
/// `leave_value` (the last statement in `Result` mode) an expression's value
/// stays on the stack instead.
fn compile_program_stmt(
    stmt: &Stmt,
    out: &mut Program,
    ctx: &CompileCtx,
    leave_value: bool,
) -> Result<(), QplError> {
    match stmt {
        // `sink` consumes the frame and leaves nothing to print
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
                    "'\\{}' is not available here",
                    crate::ast::system_cmd_name(*cmd)
                )));
            }
            compile_system(*cmd, arg, out, ctx)?;
            out.emit(Op::Pop);
        }
        Stmt::WriteFile { bytes, value, path } => {
            compile_write_file(*bytes, value, path, out)?;
            out.emit(Op::Pop);
        }
    }
    Ok(())
}

/// Compile a `\` command into a native call that leaves a `Noop`.
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
            // `\l` loads flat into the scope being compiled, so inside an
            // `\i` import it lands in that namespace
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
        'p' => {
            // `\port [<expr>]`: bare `\port` closes (an empty `Str`, like
            // `.qpl.cfg`); otherwise the argument is an ordinary expression
            // (`\port p`, `\port base+1`). See `Vm::native_port`.
            if arg.is_empty() {
                out.push_operand(Operand::Value(Value::Str(String::new())));
            } else {
                let toks = crate::lexer::tokenise(arg)?;
                let expr = match crate::parser::parse(toks)? {
                    Stmt::SingleVar(expr) => expr,
                    _ => {
                        return Err(QplError::Compile(format!(
                            "\\port expects an expression, got '{arg}'"
                        )));
                    }
                };
                compile_value_expr(&expr, out)?;
            }
            out.push_operand(Operand::Count(1));
            out.push_operand(Operand::Native(NativeId::Port));
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

/// Read, parse and compile a `\l`/`\i` target, erroring if its canonical path
/// is already in `ctx.including`.
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

/// The names a `\i`-imported file binds at top level (functions included).
/// Bare references to these are qualified to `.ns.name` throughout the file
/// unless shadowed; already-namespaced names are skipped.
fn collect_top_level_names(stmts: &[(u32, Stmt)], out: &mut HashSet<String>) {
    for (_, s) in stmts {
        if let Stmt::Assign { name, .. } | Stmt::ScalarAssign { name, .. } = s
            && !name.starts_with('.')
        {
            out.insert(name.clone());
        }
    }
}

/// Every name namespaced under the current `ns`: this file's top-level
/// assignments plus, recursively, those of any `\l` target (which shares the
/// namespace, so references before the `\l` line qualify too). `\i` targets
/// get their own namespace and aren't walked.
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

/// Qualify one top-level statement: its assignment target and every bare
/// reference inside it.
fn qualify_top_level(stmt: &mut Stmt, ns: &str, names: &HashSet<String>) {
    let empty = HashSet::new();
    qualify_stmt_refs(stmt, ns, names, &empty);
    if let Stmt::Assign { name, .. } | Stmt::ScalarAssign { name, .. } = stmt
        && !name.starts_with('.')
    {
        *name = format!("{ns}.{name}");
    }
}

/// Qualify `name` in place if it's one of the file's top-level `names` and not
/// shadowed by `locals`. Namespaced names are left alone.
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
        Stmt::WriteFile { value, path, .. } => {
            qualify_expr(value, ns, names, locals);
            qualify_expr(path, ns, names, locals);
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
        // join keys are column names, not variables
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

/// Qualify a function body in its own scope: params and every name the body
/// assigns (outside nested lambdas) are locals and never rewritten, even if
/// they match a top-level name.
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
        Stmt::WriteFile { value, path, .. } => {
            collect_locals_expr(value, out);
            collect_locals_expr(path, out);
        }
        Stmt::Cfg(_) | Stmt::System { .. } => {}
    }
}

/// Assignments inside `While` bodies share the function's scope; a nested
/// `Lambda` has its own scope and isn't descended into.
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
        // `i` gets the implicit alias "x", and a `RowIndex` follows the `Source`
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
        // the body is appended after the main code (`PUSH Func; PUSH Name(f); STORE`)
        let lines = dis("f: {[x,y] x+y}");
        assert!(lines[0].contains("PUSH       Func({[x,y] ..})"));
        assert_eq!(lines[1], "0001  PUSH       Name(f)");
        assert_eq!(lines[2], "0002  STORE");
        assert!(has_op(&lines, "RET"));
        assert!(lines.len() > 3);
        // `HALT` ends the main code so it never falls into the body
        assert_eq!(lines[3], "0003  HALT");
    }

    #[test]
    fn apply_with_two_args_compiles_to_load_fn_and_call() {
        // args first, then the callee (`LOAD_FN`), then `CALL`
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
        let lines = dis("select from select price from trades");
        assert_eq!(lines.iter().filter(|l| l.ends_with("  SELECT")).count(), 2);
    }

    #[test]
    fn join_right_side_compiles_a_parenthesised_table_expr() {
        // a parenthesised join RHS compiles like any nested table expression
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
        // a ranking verb needs nothing but the operand before WINDOW
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
        // a projection repeating a `by` key is dropped (group_by carries it)
        let lines = dis("select sym, price, ret: 1 diff price by sym from trades");
        // the count before SELECT_BY's LIST is 2 (price, ret), not 3
        let list_idx = lines
            .iter()
            .rposition(|l| l.ends_with("  LIST"))
            .expect("LIST opcode emitted");
        assert!(lines[list_idx - 1].contains("Count(2)"));
    }

    // --- full example ---

    #[test]
    fn full_query() {
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
        // `fac[n-1]` is an `Index`, compiled to `LOAD_FN`/`INDEX` since `fac`
        // might be callable
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
        // the parser constant-folds `enlist 5`, so use a variable
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
        // `til` is resolved by name at run time, so a user function can shadow it
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
        // not a bare name, so no `LOAD_FN`
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
        // two columns: not a column expression, so no `COLUMN`
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
        // backward `JUMP`, `JUMP_IF_FALSE` test, `NOOP` result
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
