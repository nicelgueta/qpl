
use crate::ast::{Expr, TableExpr, SelectStmt, Stmt, Value};
use crate::enums::{PolarsStackArg, WindowFn};
use crate::builtins::BuiltIn;
use crate::enums::PolarsFrameExpr;
use crate::errors::QplError;
use crate::opcodes::Instruction;

pub fn compile(stmt: &Stmt) -> Result<Vec<Instruction>, QplError> {
    let mut out = Vec::new();
    compile_stmt(stmt, &mut out)?;
    Ok(out)
}

fn compile_stmt(stmt: &Stmt, out: &mut Vec<Instruction>) -> Result<(), QplError> {
    match stmt {
        Stmt::RetTable(tbl_expr) => {
            compile_tbl_expr(tbl_expr, out)?;
            // `sink` is terminal: it consumes the frame and leaves no result.
            if matches!(tbl_expr, TableExpr::BuiltIn(BuiltIn::Sink { .. })) {
                return Ok(());
            }
            out.push(Instruction::Result);
            Ok(())
        },
        // assignment: compile the body; workspace binding is handled by the VM
        Stmt::Assign { name, body, .. } => { compile_stmt(body, out)?; out.push(Instruction::Assign(name.clone())); Ok(()) },
        // scalar assigns are evaluated by the REPL before reaching the compiler
        Stmt::ScalarAssign { name, expr } => { out.push(Instruction::Eval(expr.clone())); out.push(Instruction::Assign(name.clone())); Ok(()) },
        Stmt::SingleVar(expr) => { out.push(Instruction::Eval(expr.clone())); Ok(())}
        // `name: {[..] ..}` — register a user function. The body must end in an
        // expression (its return value); a trailing assignment is rejected here.
        Stmt::FuncDef { name, params, body } => {
            if !matches!(body.last(), Some(Stmt::SingleVar(_) | Stmt::RetTable(_))) {
                return Err(QplError::Compile(
                    "a function body must end with an expression, not an assignment".into(),
                ));
            }
            out.push(Instruction::DefFunc {
                name: name.clone(),
                params: params.clone(),
                body: body.clone(),
            });
            Ok(())
        }
    }
}

pub(crate) fn compile_tbl_expr(tbl_expr: &TableExpr, out: &mut Vec<Instruction>) -> Result<(), QplError> {
    match tbl_expr {
        TableExpr::Select(sel) => compile_select(sel, out),
        TableExpr::BuiltIn(func) => compile_builtin(func, out),
        TableExpr::Source(src) => {
            out.push(Instruction::FromSrc(src.clone()));
            Ok(())
        }
    }
}

fn compile_builtin(builtin: &BuiltIn, out: &mut Vec<Instruction>) -> Result<(), QplError> {
    match builtin {
        BuiltIn::Cols(tbl_expr) => {
            compile_tbl_expr(tbl_expr, out)?;
            out.push(Instruction::FrameExpr(PolarsFrameExpr::Cols));
            Ok(())
        }
        BuiltIn::Sink { src, path } => {
            compile_tbl_expr(src.as_ref(), out)?;
            out.push(Instruction::Eval(path.clone()));
            out.push(Instruction::Sink);
            Ok(())
        }
        BuiltIn::Sort(tbl_expr, sort_map) => {
            compile_tbl_expr(tbl_expr.as_ref(), out)?;
            out.push(Instruction::FrameExpr(PolarsFrameExpr::Sort(sort_map.clone())));
            Ok(())
        }
        BuiltIn::Distinct(tbl_expr) => {
            compile_tbl_expr(tbl_expr.as_ref(), out)?;
            out.push(Instruction::FrameExpr(PolarsFrameExpr::Distinct));
            Ok(())
        }
        BuiltIn::Limit(tbl_expr, limit) => {
            compile_tbl_expr(tbl_expr.as_ref(), out)?;
            out.push(Instruction::FrameExpr(PolarsFrameExpr::Limit(*limit)));
            Ok(())
        }
        BuiltIn::Drop(columns, tbl_expr) => {
            compile_tbl_expr(tbl_expr.as_ref(), out)?;
            out.push(Instruction::FrameExpr(PolarsFrameExpr::Drop(columns.clone())));
            Ok(())
        }
        BuiltIn::Lazy(tbl_expr) => {
            out.push(Instruction::Lazy);
            compile_tbl_expr(tbl_expr.as_ref(), out)?;
            Ok(())
        }
        BuiltIn::Collect(tbl_expr) => {
            compile_tbl_expr(tbl_expr.as_ref(), out)?;
            out.push(Instruction::Collect);
            Ok(())
        }
    }
}

fn compile_select(sel: &SelectStmt, out: &mut Vec<Instruction>) -> Result<(), QplError> {
    if sel.delete {
        compile_tbl_expr(&sel.from, out)?;
        if let Some(preds) = &sel.where_ {
            for (index, expr) in preds.iter().enumerate() {
                compile_expr(expr, out)?;
                if index > 0 {
                    out.push(Instruction::BinOp("&".into()));
                }
            }
            out.push(Instruction::Call { func: "not".into(), args_count: 1 });
            out.push(Instruction::FrameExpr(PolarsFrameExpr::Filter(1)));
        }
        let columns = sel.cols.iter().map(delete_column_name).collect::<Result<Vec<_>, _>>()?;
        out.push(Instruction::BuildProj { count: 0, exclude: columns, predicates: 0 });
        out.push(Instruction::Select);
        return Ok(());
    }

    if sel.update {
        compile_tbl_expr(&sel.from, out)?;
        if let Some(preds) = &sel.where_ {
            for expr in preds {
                compile_expr(expr, out)?;
            }
        }
        if let Some(keys) = &sel.by {
            for alias in keys {
                compile_expr(&alias.expr, out)?;
                out.push(Instruction::Alias {
                    name: alias.name.clone().or_else(|| implicit_alias(&alias.expr)),
                });
            }
            out.push(Instruction::BuildKeys(keys.len()));
        }
        for alias in &sel.cols {
            compile_expr(&alias.expr, out)?;
            out.push(Instruction::Alias { name: alias.name.clone() });
        }
        let columns = sel.cols.iter().map(|alias| {
            alias.name.clone().ok_or_else(|| QplError::Compile("update expressions require column aliases".into()))
        }).collect::<Result<Vec<_>, _>>()?;
        out.push(Instruction::BuildProj {
            count: sel.cols.len(),
            exclude: columns,
            predicates: sel.where_.as_ref().map_or(0, Vec::len),
        });
        out.push(Instruction::Select);
        return Ok(());
    }
    
    // Join phrase - done first so that the join is applied before any where clause filters
    if let Some((join_src, left_on, right_on, join_type)) = &sel.join {
        compile_tbl_expr(&sel.from, out)?;
        out.push(Instruction::Result); // get the left frame onto the stack for the join
        out.push(Instruction::PushPolarsArg(PolarsStackArg::Join(join_type.clone())));
        let left_count = if let Value::SymVec(s) = left_on {
            for s in s {
                out.push(Instruction::PushColRef(s.clone()));
            };
            s.len()
        } else {
            return Err(QplError::Runtime(format!("Expected symbol for left_on, got {:?}", left_on)));
        };
        let right_count = if let Value::SymVec(s) = right_on {
            for s in s {
                out.push(Instruction::PushColRef(s.clone()));
            };
            s.len()
        } else {
            return Err(QplError::Runtime(format!("Expected symbol for right_on, got {:?}", right_on)));
        };
        compile_tbl_expr(join_src, out)?;
        out.push(Instruction::Result); // get the right frame onto the stack for the join
        out.push(Instruction::FrameExpr(PolarsFrameExpr::Join { l: left_count, r: right_count }));
    } else {
        // From phrase
        compile_tbl_expr(&sel.from, out)?;
    }

    // Where phrase: each subphrase is a successive filter (spec: evaluated left-to-right)
    if let Some(preds) = &sel.where_ {
        let n = preds.len();
        for expr in preds {
            compile_expr(expr, out)?;
        }
        out.push(Instruction::FrameExpr(PolarsFrameExpr::Filter(n)));
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
            out.push(Instruction::Alias {
                name: alias.name.clone().or_else(|| implicit_alias(&alias.expr)),
            });
        }
        out.push(Instruction::BuildKeys(keys.len()));
    }

    // Select phrase — `group_by(keys).agg(proj)` already carries the key
    // columns through, so re-projecting a column under the same name as a
    // `by` key would hand Polars two columns with one name; skip it.
    let mut proj_count = 0;
    for alias in &sel.cols {
        let name = alias.name.clone().or_else(|| implicit_alias(&alias.expr));
        if has_by && name.as_deref().is_some_and(|n| by_names.iter().any(|k| k == n)) {
            continue;
        }
        compile_expr(&alias.expr, out)?;
        out.push(Instruction::Alias { name });
        proj_count += 1;
    }
    out.push(Instruction::BuildProj { count: proj_count, exclude: vec![], predicates: 0 });

    out.push(if has_by { Instruction::SelectBy } else { Instruction::Select });
    if let Some(order) = &sel.order {
        out.push(Instruction::FrameExpr(PolarsFrameExpr::Sort(order.clone())));
    }
    Ok(())
}

fn delete_column_name(alias: &crate::ast::Alias) -> Result<String, QplError> {
    match &alias.expr {
        Expr::ColRef(name) | Expr::Sym(name) => Ok(name.clone()),
        other => Err(QplError::Compile(format!("delete expects column names, got {other:?}"))),
    }
}

fn compile_expr(node: &Expr, out: &mut Vec<Instruction>) -> Result<(), QplError> {
    match node {
        Expr::Lit(v) => out.push(Instruction::PushConst(v.clone())),
        // symbols are string-typed in Polars
        Expr::Sym(s) => out.push(Instruction::PushConst(Value::Str(s.clone()))),
        Expr::ColRef(name) => out.push(Instruction::PushColRef(name.clone())),
        Expr::IColRef => out.push(Instruction::PushIColRef),
        Expr::BinOp { left, op, right } => {
            compile_expr(left, out)?;
            compile_expr(right, out)?;
            out.push(Instruction::BinOp(op.clone()));
        }
        // `<precision> round <col>` — parser hands us args = [value, precision].
        // Precision must be a literal (it becomes part of the instruction); the
        // rounding mode is resolved from VM config at run time.
        Expr::Call { func, args } if func == "round" => {
            let [value, precision] = args.as_slice() else {
                return Err(QplError::Compile("round expects `<precision> round <column>`".into()));
            };
            let decimals = match precision {
                Expr::Lit(Value::Int(n)) if *n >= 0 => *n as u32,
                _ => return Err(QplError::Compile(
                    "round precision must be a non-negative integer literal".into())),
            };
            compile_expr(value, out)?;
            out.push(Instruction::Round { decimals });
        }
        // `.qpl.d` / `.qpl.t` / `.qpl.p` / `.qpl.n` — nullary now-functions,
        // folded to a constant here (qpl recompiles every line, so compile time
        // is effectively evaluation time).
        Expr::Call { func, args } if func.starts_with(".qpl.") && args.is_empty() => {
            out.push(Instruction::PushConst(crate::temporal::now_value(func)?));
        }
        Expr::Call { func, args } => {
            for arg in args {
                compile_expr(arg, out)?;
            }
            out.push(Instruction::Call { func: func.clone(), args_count: args.len() });
        }
        Expr::Cast { target, expr } => {
            compile_expr(expr, out)?;
            out.push(Instruction::Cast(target.clone()));
        }
        Expr::Case { branches, default } => {
            for (condition, value) in branches {
                compile_expr(condition, out)?;
                compile_expr(value, out)?;
            }
            compile_expr(default, out)?;
            out.push(Instruction::Case { branches: branches.len() });
        }
        Expr::Window { func, partition, order, rolling } => {
            if partition.is_empty() {
                return Err(QplError::Compile("`over` needs at least one partition symbol".into()));
            }
            // `<agg> <col> <n>!rolling over ...` — push the *raw* column and carry
            // the aggregate name in the instruction; the VM applies the rolling
            // reduction instead of the plain aggregate.
            if let Some(window) = rolling {
                let (agg, column) = match func.as_ref() {
                    Expr::Call { func: agg, args } if args.len() == 1 => (agg.clone(), &args[0]),
                    _ => return Err(QplError::Compile(
                        "`rolling` must wrap a plain aggregate, e.g. `sum px 5!rolling over `k`".into())),
                };
                compile_expr(column, out)?;
                out.push(Instruction::Window {
                    func: WindowFn::Over,
                    partition: partition.clone(),
                    order: order.clone(),
                    rolling: Some((agg, *window)),
                });
                return Ok(());
            }
            // bare ranking verbs (`rn` / `rank` / `drank`) synthesise their own
            // expression from the window order; everything else is a column
            // expression applied per partition.
            let ranking = match func.as_ref() {
                Expr::ColRef(name) => match name.as_str() {
                    "rn"    => Some(WindowFn::RowNumber),
                    "rank"  => Some(WindowFn::Rank),
                    "drank" => Some(WindowFn::DenseRank),
                    _ => None,
                },
                _ => None,
            };
            match ranking {
                Some(_) => {
                    if order.is_empty() {
                        return Err(QplError::Compile(
                            "`rn` / `rank` / `drank` need an `order` sub-clause".into()));
                    }
                }
                None => compile_expr(func, out)?,
            }
            out.push(Instruction::Window {
                func: ranking.unwrap_or(WindowFn::Over),
                partition: partition.clone(),
                order: order.clone(),
                rolling: None,
            });
        }
        Expr::Dict(_) => return Err(QplError::Runtime("Dict expressions are not supported in select statements (yet)".into())),
        // column expressions / slices / indexing are value-context only — they are
        // tree-walked by `resolve::eval_value`, never lowered into a projection.
        Expr::Table(_) => return Err(QplError::Compile(
            "a `table`col` / `select` column expression cannot appear inside a select projection".into())),
        Expr::Take { .. } => return Err(QplError::Compile(
            "`n#…` slicing is only valid outside a select projection".into())),
        Expr::Index { .. } => return Err(QplError::Compile(
            "positional indexing is only valid outside a select projection".into())),
        Expr::Apply { .. } => return Err(QplError::Compile(
            "a user function call `f[..]` is only valid outside a select projection".into())),
        Expr::Dispatch { .. } => return Err(QplError::Compile(
            "`dispatch` is only valid outside a select projection".into())),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Value, TableSource};
    use crate::lexer::tokenise;
    use crate::opcodes::Instruction::{self, *};
    use crate::parser::parse;

    fn compile_src(src: &str) -> Vec<Instruction> {
        let tokens = tokenise(src).expect("lex error");
        let stmt = parse(tokens).expect("parse error");
        compile(&stmt).expect("compile error")
    }

    fn alias(name: &str) -> Instruction { Alias { name: Some(name.into()) } }
    // fn no_alias() -> Instruction       { Alias { name: None } }
    fn from_table(t: &str) -> Instruction    { FromSrc(TableSource::InMem(t.into())) }
    fn col(c: &str) -> Instruction     { PushColRef(c.into()) }
    fn int(n: i64) -> Instruction      { PushConst(Value::Int(n)) }
    fn sym(s: &str) -> Instruction     { PushConst(Value::Str(s.into())) }
    fn op(o: &str) -> Instruction      { BinOp(o.into()) }
    fn call(f: &str, n: usize) -> Instruction { Call { func: f.into(), args_count: n } }

    // --- simple selects ---

    #[test]
    fn select_single_col() {
        assert_eq!(compile_src("select px from trades"), vec![
            from_table("trades"), col("px"), alias("px"), BuildProj { count: 1, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    #[test]
    fn select_multi_col() {
        assert_eq!(compile_src("select px, qty from trades"), vec![
            from_table("trades"),
            col("px"),  alias("px"),
            col("qty"), alias("qty"),
            BuildProj { count: 2, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    #[test]
    fn select_all_cols() {
        // empty select phrase = return all columns
        assert_eq!(compile_src("select from t"), vec![
            from_table("t"), BuildProj { count: 0, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    // --- aliases ---

    #[test]
    fn explicit_alias() {
        assert_eq!(compile_src("select p: price from trades"), vec![
            from_table("trades"), col("price"), alias("p"), BuildProj { count: 1, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    #[test]
    fn implicit_alias_from_colref() {
        // no alias: column name becomes the implicit alias
        assert_eq!(compile_src("select price from trades"), vec![
            from_table("trades"), col("price"), alias("price"), BuildProj { count: 1, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    // --- computed columns ---

    #[test]
    fn explicit_alias_binop() {
        assert_eq!(compile_src("select dbl: c3*2 from t"), vec![
            from_table("t"), col("c3"), int(2), op("*"), alias("dbl"), BuildProj { count: 1, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    #[test]
    fn implicit_alias_binop_uses_leftmost_leaf() {
        // per spec: leftmost term (c3) becomes the implicit alias
        assert_eq!(compile_src("select c3*2 from t"), vec![
            from_table("t"), col("c3"), int(2), op("*"), alias("c3"), BuildProj { count: 1, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    #[test]
    fn implicit_alias_call_uses_leftmost_arg() {
        // sum price: leftmost arg is price → alias "price"
        assert_eq!(compile_src("select sum price from t"), vec![
            from_table("t"), col("price"), call("sum", 1), alias("price"), BuildProj { count: 1, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    // --- virtual column i ---

    #[test]
    fn select_icol() {
        // i is virtual, never a real column name → implicit alias is "x"
        assert_eq!(compile_src("select i from t"), vec![
            from_table("t"), PushIColRef, alias("x"), BuildProj { count: 1, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    // --- where clause ---

    #[test]
    fn where_single_pred() {
        assert_eq!(compile_src("select px from trades where qty > 0"), vec![
            from_table("trades"),
            col("qty"), int(0), op(">"), FrameExpr(PolarsFrameExpr::Filter(1)),
            col("px"), alias("px"), BuildProj { count: 1, exclude: vec![], predicates: 0 },
            Select, Result,
        ]);
    }

    #[test]
    fn where_symbol_eq() {
        assert_eq!(compile_src("select px from trades where sym = `AAPL"), vec![
            from_table("trades"),
            col("sym"), sym("AAPL"), op("="), FrameExpr(PolarsFrameExpr::Filter(1)),
            col("px"), alias("px"), BuildProj { count: 1, exclude: vec![], predicates: 0 },
            Select, Result,
        ]);
    }

    #[test]
    fn where_like() {
        assert_eq!(compile_src(r#"select px from trades where sym like "AA*""#), vec![
            from_table("trades"),
            col("sym"), sym("AA*"), op("like"), FrameExpr(PolarsFrameExpr::Filter(1)),
            col("px"), alias("px"), BuildProj { count: 1, exclude: vec![], predicates: 0 },
            Select, Result,
        ]);
    }

    #[test]
    fn where_multiple_subphrases() {
        // successive filters: spec says each subphrase applied to result of previous
        assert_eq!(compile_src("select px from trades where sym=`AAPL, qty>0"), vec![
            from_table("trades"),
            col("sym"), sym("AAPL"), op("="),
            col("qty"), int(0), op(">"),
            FrameExpr(PolarsFrameExpr::Filter(2)),
            col("px"), alias("px"), BuildProj { count: 1, exclude: vec![], predicates: 0 },
            Select, Result,
        ]);
    }

    #[test]
    fn order_multiple_columns() {
        assert_eq!(compile_src("select from trades order `col1 asc, `col2 desc"), vec![
            from_table("trades"),
            BuildProj { count: 0, exclude: vec![], predicates: 0 }, Select,
            FrameExpr(PolarsFrameExpr::Sort(vec![("col1".into(), false), ("col2".into(), true)])),
            Result,
        ]);
    }

    #[test]
    fn distinct_and_limit() {
        assert_eq!(compile_src("distinct select from trades"), vec![
            from_table("trades"), BuildProj { count: 0, exclude: vec![], predicates: 0 }, Select,
            FrameExpr(PolarsFrameExpr::Distinct), Result,
        ]);
        assert_eq!(compile_src("10 limit select from trades"), vec![
            from_table("trades"), BuildProj { count: 0, exclude: vec![], predicates: 0 }, Select,
            FrameExpr(PolarsFrameExpr::Limit(10)), Result,
        ]);
        // `n#…` is a value expression, tree-walked from a single Eval
        assert!(matches!(compile_src("10#select from trades").as_slice(), [Eval(_)]));
    }

    #[test]
    fn update_preserves_table_shape() {
        assert_eq!(compile_src("update price: price * 2 from trades"), vec![
            from_table("trades"),
            col("price"), int(2), op("*"), alias("price"),
            BuildProj { count: 1, exclude: vec!["price".into()], predicates: 0 }, Select,
            Result,
        ]);
    }

    #[test]
    fn delete_rows_uses_negated_filter_and_select() {
        assert_eq!(compile_src("delete from trades where size > 100"), vec![
            from_table("trades"),
            col("size"), int(100), op(">"), call("not", 1),
            FrameExpr(PolarsFrameExpr::Filter(1)),
            BuildProj { count: 0, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    #[test]
    fn lazy_prefixes_the_plan_and_stays_uncollected() {
        assert_eq!(compile_src("l: lazy select from trades"), vec![
            Lazy,
            from_table("trades"),
            BuildProj { count: 0, exclude: vec![], predicates: 0 }, Select,
            Result, Assign("l".into()),
        ]);
    }

    #[test]
    fn collect_appends_a_collect_instruction() {
        assert_eq!(compile_src("tm: collect t"), vec![
            from_table("t"),
            Collect, Result, Assign("tm".into()),
        ]);
    }

    #[test]
    fn func_def_compiles_to_a_single_def_func() {
        use crate::ast::{Expr, Stmt};
        assert_eq!(compile_src("f: {[x,y] x+y}"), vec![DefFunc {
            name: "f".into(),
            params: vec!["x".into(), "y".into()],
            body: vec![Stmt::SingleVar(Expr::BinOp {
                left: Box::new(Expr::ColRef("x".into())),
                op: "+".into(),
                right: Box::new(Expr::ColRef("y".into())),
            })],
        }]);
    }

    #[test]
    fn func_body_ending_in_an_assignment_is_a_compile_error() {
        let stmt = parse(tokenise("f: {[x] y: x+1}").unwrap()).unwrap();
        assert!(matches!(compile(&stmt), Err(QplError::Compile(_))));
    }

    #[test]
    fn sink_is_terminal_without_trailing_result() {
        let prog = compile_src("t sink \"out.parquet\"");
        assert_eq!(prog, vec![
            from_table("t"),
            Eval(Expr::Lit(Value::Str("out.parquet".into()))), Sink,
        ]);
    }

    #[test]
    fn sink_after_a_select_compiles() {
        let prog = compile_src("select price from trades sink \"out.parquet\"");
        assert!(prog.last() == Some(&Sink));
        assert!(!prog.contains(&Result));
    }

    #[test]
    fn from_compiles_a_nested_select() {
        // the outer select's own `select`/`from` compiles around whatever
        // instructions the inner select compiles to.
        let prog = compile_src("select from select price from trades");
        assert_eq!(prog, vec![
            from_table("trades"), col("price"), alias("price"),
            BuildProj { count: 1, exclude: vec![], predicates: 0 },
            Select,
            BuildProj { count: 0, exclude: vec![], predicates: 0 },
            Select, Result,
        ]);
    }

    #[test]
    fn join_right_side_compiles_a_parenthesised_table_expr() {
        // a parenthesised join RHS recurses through compile_tbl_expr, so
        // `(distinct quotes)` compiles its own FrameExpr(Distinct) before
        // the join is built, same as any other nested table expression.
        let prog = compile_src("select price from trades `sym lj (distinct quotes) `sym");
        assert!(prog.contains(&FrameExpr(PolarsFrameExpr::Distinct)));
        assert!(matches!(
            prog.iter().find(|i| matches!(i, FrameExpr(PolarsFrameExpr::Join { .. }))),
            Some(FrameExpr(PolarsFrameExpr::Join { .. })),
        ));
    }

    #[test]
    fn case_expression_compiles() {
        let program = compile_src("select bin: ?[c2>20;`high;c2>10;`mid;`low] from t");
        assert!(program.iter().any(|instruction| matches!(instruction, Case { branches: 2 })));
    }

    #[test]
    fn round_compiles_to_round_instruction_with_precision() {
        assert_eq!(compile_src("select r: 2 round px from t"), vec![
            from_table("t"),
            col("px"), Round { decimals: 2 }, alias("r"),
            BuildProj { count: 1, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    #[test]
    fn round_with_non_literal_precision_is_compile_error() {
        let stmt = parse(tokenise("select r: sz round px from t").unwrap()).unwrap();
        assert!(matches!(compile(&stmt), Err(QplError::Compile(_))));
    }

    // --- window functions ---

    #[test]
    fn window_over_compiles_the_target_then_a_window_instruction() {
        use crate::enums::WindowFn;
        assert_eq!(compile_src("select m: max px over `s from t"), vec![
            from_table("t"),
            col("px"), call("max", 1),
            Window { func: WindowFn::Over, partition: vec!["s".into()], order: vec![], rolling: None },
            alias("m"),
            BuildProj { count: 1, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    #[test]
    fn window_ranking_verb_emits_only_the_window_instruction() {
        use crate::enums::WindowFn;
        assert_eq!(compile_src("select r: rn over `s order `px desc from t"), vec![
            from_table("t"),
            Window { func: WindowFn::RowNumber, partition: vec!["s".into()], order: vec![("px".into(), true)], rolling: None },
            alias("r"),
            BuildProj { count: 1, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    #[test]
    fn window_ranking_verb_without_order_is_compile_error() {
        let stmt = parse(tokenise("select r: rank over `s from t").unwrap()).unwrap();
        assert!(matches!(compile(&stmt), Err(QplError::Compile(_))));
    }

    #[test]
    fn window_order_on_a_plain_aggregate_is_allowed() {
        use crate::enums::WindowFn;
        assert_eq!(compile_src("select c: cumsum px over `s order `px asc from t"), vec![
            from_table("t"),
            col("px"), call("cumsum", 1),
            Window { func: WindowFn::Over, partition: vec!["s".into()], order: vec![("px".into(), false)], rolling: None },
            alias("c"),
            BuildProj { count: 1, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    #[test]
    fn rolling_window_pushes_raw_column_and_carries_agg_name() {
        use crate::enums::WindowFn;
        assert_eq!(compile_src("select r: sum px over `s order `ts asc rolling 3 from t"), vec![
            from_table("t"),
            col("px"),
            Window {
                func: WindowFn::Over,
                partition: vec!["s".into()],
                order: vec![("ts".into(), false)],
                rolling: Some(("sum".into(), 3)),
            },
            alias("r"),
            BuildProj { count: 1, exclude: vec![], predicates: 0 }, Select, Result,
        ]);
    }

    // --- by clause ---

    #[test]
    fn by_single_key() {
        assert_eq!(compile_src("select sum px by sym from trades"), vec![
            from_table("trades"),
            col("sym"), alias("sym"), BuildKeys(1),
            col("px"), call("sum", 1), alias("px"), BuildProj { count: 1, exclude: vec![], predicates: 0 },
            SelectBy, Result,
        ]);
    }

    #[test]
    fn by_explicit_alias() {
        assert_eq!(compile_src("select sum px by s: sym from trades"), vec![
            from_table("trades"),
            col("sym"), alias("s"), BuildKeys(1),
            col("px"), call("sum", 1), alias("px"), BuildProj { count: 1, exclude: vec![], predicates: 0 },
            SelectBy, Result,
        ]);
    }

    #[test]
    fn by_key_reprojected_by_name_is_deduped() {
        // `group_by(keys).agg(proj)` already carries the key columns through;
        // re-projecting `sym` under its own name would hand Polars two columns
        // named `sym`, so the compiler drops it from the projection phase.
        assert_eq!(compile_src("select sym, price, ret: 1 diff price by sym from trades"), vec![
            from_table("trades"),
            col("sym"), alias("sym"), BuildKeys(1),
            col("price"), alias("price"),
            col("price"), int(1), call("diff", 2), alias("ret"),
            BuildProj { count: 2, exclude: vec![], predicates: 0 },
            SelectBy, Result,
        ]);
    }

    #[test]
    fn by_key_reprojected_under_an_alias_is_kept() {
        // only a name collision with the key is dropped — an *aliased*
        // reprojection of the key column is a distinct output name and stays.
        assert_eq!(compile_src("select s: sym, price by sym from trades"), vec![
            from_table("trades"),
            col("sym"), alias("sym"), BuildKeys(1),
            col("sym"), alias("s"),
            col("price"), alias("price"),
            BuildProj { count: 2, exclude: vec![], predicates: 0 },
            SelectBy, Result,
        ]);
    }

    // --- full example from spec ---

    #[test]
    fn full_query() {
        // select dbl: c3*2 by c1 from t where c2>15
        assert_eq!(compile_src("select dbl: c3*2 by c1 from t where c2>15"), vec![
            from_table("t"),
            col("c2"), int(15), op(">"), FrameExpr(PolarsFrameExpr::Filter(1)),
            col("c1"), alias("c1"), BuildKeys(1),
            col("c3"), int(2), op("*"), alias("dbl"), BuildProj { count: 1, exclude: vec![], predicates: 0 },
            SelectBy, Result,
        ]);
    }
}