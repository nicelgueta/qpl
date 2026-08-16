use crate::ast::{Expr, SelectStmt, Stmt, Value};
use crate::errors::QplError;
use crate::opcodes::Instruction;

pub fn compile(stmt: &Stmt) -> Result<Vec<Instruction>, QplError> {
    let mut out = Vec::new();
    compile_stmt(stmt, &mut out)?;
    Ok(out)
}

fn compile_stmt(stmt: &Stmt, out: &mut Vec<Instruction>) -> Result<(), QplError> {
    match stmt {
        Stmt::Select(sel)       => compile_select(sel, out),
        Stmt::Cols(name)        => { out.push(Instruction::ColsOf(name.clone())); out.push(Instruction::Result); Ok(()) }
        // assignment: compile the body; workspace binding is handled by the VM
        Stmt::Assign { name, body, .. } => { compile_stmt(body, out)?; out.push(Instruction::Assign(name.clone())); Ok(()) },
        // scalar assigns are evaluated by the REPL before reaching the compiler
        Stmt::ScalarAssign { name, expr } => { out.push(Instruction::Eval(expr.clone())); out.push(Instruction::Assign(name.clone())); Ok(()) }
    }
}

fn compile_select(sel: &SelectStmt, out: &mut Vec<Instruction>) -> Result<(), QplError> {
    // From phrase
    out.push(Instruction::FromSrc(sel.from.clone()));

    // Where phrase: each subphrase is a successive filter (spec: evaluated left-to-right)
    if let Some(preds) = &sel.where_ {
        let n = preds.len();
        for expr in preds {
            compile_expr(expr, out)?;
        }
        out.push(Instruction::Filter(n));
    }

    // By phrase
    let has_by = sel.by.is_some();
    if let Some(keys) = &sel.by {
        for alias in keys {
            compile_expr(&alias.expr, out)?;
            out.push(Instruction::Alias {
                name: alias.name.clone().or_else(|| implicit_alias(&alias.expr)),
            });
        }
        out.push(Instruction::BuildKeys(keys.len()));
    }

    // Select phrase
    for alias in &sel.cols {
        compile_expr(&alias.expr, out)?;
        out.push(Instruction::Alias {
            name: alias.name.clone().or_else(|| implicit_alias(&alias.expr)),
        });
    }
    out.push(Instruction::BuildProj(sel.cols.len()));

    out.push(if has_by { Instruction::SelectBy } else { Instruction::Select });
    out.push(Instruction::Result);
    Ok(())
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
        Expr::Call { func, args } => {
            for arg in args {
                compile_expr(arg, out)?;
            }
            out.push(Instruction::Call { func: func.clone(), args_count: args.len() });
        }
        Expr::Cast { dtype, expr } => {
            compile_expr(expr, out)?;
            out.push(Instruction::Cast(dtype.clone()));
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
            from_table("trades"), col("px"), alias("px"), BuildProj(1), Select, Result,
        ]);
    }

    #[test]
    fn select_multi_col() {
        assert_eq!(compile_src("select px, qty from trades"), vec![
            from_table("trades"),
            col("px"),  alias("px"),
            col("qty"), alias("qty"),
            BuildProj(2), Select, Result,
        ]);
    }

    #[test]
    fn select_all_cols() {
        // empty select phrase = return all columns
        assert_eq!(compile_src("select from t"), vec![
            from_table("t"), BuildProj(0), Select, Result,
        ]);
    }

    // --- aliases ---

    #[test]
    fn explicit_alias() {
        assert_eq!(compile_src("select p: price from trades"), vec![
            from_table("trades"), col("price"), alias("p"), BuildProj(1), Select, Result,
        ]);
    }

    #[test]
    fn implicit_alias_from_colref() {
        // no alias: column name becomes the implicit alias
        assert_eq!(compile_src("select price from trades"), vec![
            from_table("trades"), col("price"), alias("price"), BuildProj(1), Select, Result,
        ]);
    }

    // --- computed columns ---

    #[test]
    fn explicit_alias_binop() {
        assert_eq!(compile_src("select dbl: c3*2 from t"), vec![
            from_table("t"), col("c3"), int(2), op("*"), alias("dbl"), BuildProj(1), Select, Result,
        ]);
    }

    #[test]
    fn implicit_alias_binop_uses_leftmost_leaf() {
        // per spec: leftmost term (c3) becomes the implicit alias
        assert_eq!(compile_src("select c3*2 from t"), vec![
            from_table("t"), col("c3"), int(2), op("*"), alias("c3"), BuildProj(1), Select, Result,
        ]);
    }

    #[test]
    fn implicit_alias_call_uses_leftmost_arg() {
        // sum price: leftmost arg is price → alias "price"
        assert_eq!(compile_src("select sum price from t"), vec![
            from_table("t"), col("price"), call("sum", 1), alias("price"), BuildProj(1), Select, Result,
        ]);
    }

    // --- virtual column i ---

    #[test]
    fn select_icol() {
        // i is virtual, never a real column name → implicit alias is "x"
        assert_eq!(compile_src("select i from t"), vec![
            from_table("t"), PushIColRef, alias("x"), BuildProj(1), Select, Result,
        ]);
    }

    // --- where clause ---

    #[test]
    fn where_single_pred() {
        assert_eq!(compile_src("select px from trades where qty > 0"), vec![
            from_table("trades"),
            col("qty"), int(0), op(">"), Filter(1),
            col("px"), alias("px"), BuildProj(1),
            Select, Result,
        ]);
    }

    #[test]
    fn where_symbol_eq() {
        assert_eq!(compile_src("select px from trades where sym = `AAPL"), vec![
            from_table("trades"),
            col("sym"), sym("AAPL"), op("="), Filter(1),
            col("px"), alias("px"), BuildProj(1),
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
            Filter(2),
            col("px"), alias("px"), BuildProj(1),
            Select, Result,
        ]);
    }

    // --- by clause ---

    #[test]
    fn by_single_key() {
        assert_eq!(compile_src("select sum px by sym from trades"), vec![
            from_table("trades"),
            col("sym"), alias("sym"), BuildKeys(1),
            col("px"), call("sum", 1), alias("px"), BuildProj(1),
            SelectBy, Result,
        ]);
    }

    #[test]
    fn by_explicit_alias() {
        assert_eq!(compile_src("select sum px by s: sym from trades"), vec![
            from_table("trades"),
            col("sym"), alias("s"), BuildKeys(1),
            col("px"), call("sum", 1), alias("px"), BuildProj(1),
            SelectBy, Result,
        ]);
    }

    // --- full example from spec ---

    #[test]
    fn full_query() {
        // select dbl: c3*2 by c1 from t where c2>15
        assert_eq!(compile_src("select dbl: c3*2 by c1 from t where c2>15"), vec![
            from_table("t"),
            col("c2"), int(15), op(">"), Filter(1),
            col("c1"), alias("c1"), BuildKeys(1),
            col("c3"), int(2), op("*"), alias("dbl"), BuildProj(1),
            SelectBy, Result,
        ]);
    }
}