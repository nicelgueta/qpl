use polars::prelude::*;
use crate::ast::{self, TableSource};
use crate::errors::QplError;
use crate::opcodes::Instruction;
use std::collections::HashMap;

pub struct Vm {
    pub tables: HashMap<String, DataFrame>,
    pub globals: HashMap<String, ast::Value>,
}

impl Vm {
    pub fn new() -> Self {
        Self { tables: HashMap::new(), globals: HashMap::new() }
    }

    /// Returns a two-column table: `column` (name) and `dtype` for every field in `table_name`.
    pub fn schema(&self, table_name: &str) -> Result<DataFrame, QplError> {
        let df = self.tables.get(table_name)
            .ok_or_else(|| QplError::Runtime(format!("unknown table '{table_name}'")))?;
        let names: Vec<String> = df.get_column_names().iter().map(|s| s.to_string()).collect();
        let types: Vec<String> = df.dtypes().iter().map(|d| d.to_string()).collect();
        df!["column" => names, "dtype" => types]
            .map_err(|e| QplError::Runtime(e.to_string()))
    }

    /// Evaluates a constant expression using only literals and globals (no DataFrame).
    pub fn eval_scalar(&self, expr: &ast::Expr) -> Result<ast::Value, QplError> {
        match expr {
            ast::Expr::Lit(v) => Ok(v.clone()),
            ast::Expr::ColRef(name) => self.globals.get(name)
                .cloned()
                .ok_or_else(|| QplError::Runtime(format!("undefined variable '{name}'"))),
            ast::Expr::BinOp { left, op, right } => {
                scalar_binop(self.eval_scalar(left)?, self.eval_scalar(right)?, op)
            }
            ast::Expr::Cast { dtype, expr } => {
                scalar_cast(self.eval_scalar(expr)?, dtype)
            }
            other => Err(QplError::Runtime(format!("not supported in scalar context: {other:?}"))),
        }
    }

    /// Executes a compiled program. Builds a LazyFrame plan for every
    /// instruction and materialises it only at `Result`.
    pub fn eval(&self, program: Vec<Instruction>) -> Result<DataFrame, QplError> {
        let needs_i = program.iter().any(|i| matches!(i, Instruction::PushIColRef));

        let mut stack: Vec<Expr> = Vec::new();
        let mut frame: Option<LazyFrame> = None;
        let mut keys:  Vec<Expr> = Vec::new();
        let mut proj:  Vec<Expr> = Vec::new();

        for instr in program {
            match instr {
                Instruction::FromSrc(tbl_source) => {
                    match tbl_source {
                        TableSource::InMem(name) => {
                            let lf = self.tables
                                .get(&name)
                                .ok_or_else(|| QplError::Runtime(format!("unknown table '{name}'")))?
                                .clone()
                                .lazy();
                            frame = Some(if needs_i { lf.with_row_index("i", None) } else { lf });
                        }
                        TableSource::Scan(path) => {
                            let lf = scan_file(&path)?;
                            frame = Some(if needs_i { lf.with_row_index("i", None) } else { lf });
                        }

                    }
                }

                Instruction::PushConst(val) => {
                    stack.push(ast_val_to_expr(val)?);
                }

                Instruction::PushColRef(name) => {
                    // globals shadow column names, substituting a literal into the lazy plan
                    if let Some(val) = self.globals.get(&name) {
                        stack.push(ast_val_to_expr(val.clone())?);
                    } else {
                        stack.push(col(name.as_str()));
                    }
                }

                Instruction::PushIColRef => {
                    stack.push(col("i"));
                }

                Instruction::BinOp(op) => {
                    let right = pop1(&mut stack)?;
                    let left  = pop1(&mut stack)?;
                    stack.push(apply_binop(left, right, &op)?);
                }

                Instruction::Call { func, args_count } => {
                    let args = popn(&mut stack, args_count)?;
                    stack.push(apply_call(&func, args)?);
                }

                Instruction::Alias { name } => {
                    let expr = pop1(&mut stack)?;
                    stack.push(match name {
                        Some(n) => expr.alias(n.as_str()),
                        None    => expr,
                    });
                }

                // per spec: subphrases are successive filters (not a single AND)
                Instruction::Filter(n) => {
                    let preds = popn(&mut stack, n)?;
                    let mut lf = require_frame(&mut frame)?;
                    for pred in preds {
                        lf = lf.filter(pred);
                    }
                    frame = Some(lf);
                }

                Instruction::BuildKeys(n) => {
                    keys = popn(&mut stack, n)?;
                }

                Instruction::BuildProj(n) => {
                    proj = popn(&mut stack, n)?;
                }

                Instruction::Select => {
                    let lf = require_frame(&mut frame)?;
                    frame = Some(if proj.is_empty() {
                        lf // empty projection = all columns
                    } else {
                        lf.select(std::mem::take(&mut proj))
                    });
                }

                // group_by().agg(): keys become keyed columns, proj holds agg exprs
                Instruction::SelectBy => {
                    let lf = require_frame(&mut frame)?;
                    frame = Some(
                        lf.group_by(std::mem::take(&mut keys))
                          .agg(std::mem::take(&mut proj))
                    );
                }

                Instruction::ColsOf(name) => {
                    frame = Some(self.schema(&name)?.lazy());
                }

                Instruction::Cast(dtype) => {
                    let expr = pop1(&mut stack)?;
                    stack.push(expr.cast(polars_dtype(&dtype)?));
                }

                Instruction::Result => {
                    let lf = require_frame(&mut frame)?;
                    return lf.collect()
                        .map_err(|e| QplError::Runtime(e.to_string()));
                }
            }
        }

        Err(QplError::Runtime("program ended without Result instruction".into()))
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

fn require_frame(f: &mut Option<LazyFrame>) -> Result<LazyFrame, QplError> {
    f.take().ok_or_else(|| QplError::Runtime("no active frame".into()))
}

fn pop1(stack: &mut Vec<Expr>) -> Result<Expr, QplError> {
    stack.pop().ok_or_else(|| QplError::Runtime("stack underflow".into()))
}

fn popn(stack: &mut Vec<Expr>, n: usize) -> Result<Vec<Expr>, QplError> {
    if stack.len() < n {
        return Err(QplError::Runtime(format!(
            "stack underflow: need {n}, have {}",
            stack.len()
        )));
    }
    let at = stack.len() - n;
    Ok(stack.drain(at..).collect()) // drain preserves push order
}

fn ast_val_to_expr(val: ast::Value) -> Result<Expr, QplError> {
    Ok(match val {
        ast::Value::Int(n)     => lit(n),
        ast::Value::Float(n)   => lit(n),
        ast::Value::Str(s)     => lit(s),
        ast::Value::Bool(b)    => lit(b),
        ast::Value::IntVec(v)  => Series::new("".into(), v.as_slice()).lit(),
        ast::Value::FloatVec(v)=> Series::new("".into(), v.as_slice()).lit(),
        ast::Value::BoolVec(v) => Series::new("".into(), v.as_slice()).lit(),
        ast::Value::SymVec(v)  => {
            let strs: Vec<&str> = v.iter().map(String::as_str).collect();
            Series::new("".into(), strs.as_slice()).lit()
        }
    })
}

fn scalar_binop(l: ast::Value, r: ast::Value, op: &str) -> Result<ast::Value, QplError> {
    use ast::Value::*;
    // promote int to float when mixed
    let (l, r) = match (l, r) {
        (Int(a),   Float(b)) => (Float(a as f64), Float(b)),
        (Float(a), Int(b))   => (Float(a), Float(b as f64)),
        pair => pair,
    };
    Ok(match (l, r, op) {
        (Int(a),   Int(b),   "+")        => Int(a + b),
        (Int(a),   Int(b),   "-")        => Int(a - b),
        (Int(a),   Int(b),   "*")        => Int(a * b),
        (Int(a),   Int(b),   "%")        => Int(a / b),
        (Int(a),   Int(b),   "=")        => Bool(a == b),
        (Int(a),   Int(b),   "<")        => Bool(a < b),
        (Int(a),   Int(b),   ">")        => Bool(a > b),
        (Int(a),   Int(b),   "<=")       => Bool(a <= b),
        (Int(a),   Int(b),   ">=")       => Bool(a >= b),
        (Int(a),   Int(b),   "<>" | "!=")=> Bool(a != b),
        (Float(a), Float(b), "+")        => Float(a + b),
        (Float(a), Float(b), "-")        => Float(a - b),
        (Float(a), Float(b), "*")        => Float(a * b),
        (Float(a), Float(b), "%")        => Float(a / b),
        (Float(a), Float(b), "=")        => Bool(a == b),
        (Float(a), Float(b), "<")        => Bool(a < b),
        (Float(a), Float(b), ">")        => Bool(a > b),
        (Float(a), Float(b), "<=")       => Bool(a <= b),
        (Float(a), Float(b), ">=")       => Bool(a >= b),
        (Str(a),   Str(b),   "+")        => Str(a + &b),
        (l, r, op) => return Err(QplError::Runtime(
            format!("cannot apply '{op}' to {l:?} and {r:?}"))),
    })
}

fn scalar_cast(val: ast::Value, dtype: &str) -> Result<ast::Value, QplError> {
    use ast::Value::*;
    Ok(match (val, dtype) {
        (Int(n),   "f64" | "float" | "f32") => Float(n as f64),
        (Float(f), "i64" | "int"  | "i32" | "i16" | "i8") => Int(f as i64),
        (Int(n),   "str" | "string") => Str(n.to_string()),
        (Float(f), "str" | "string") => Str(f.to_string()),
        (Bool(b),  "str" | "string") => Str(b.to_string()),
        (v, t) => return Err(QplError::Runtime(format!("cannot cast {v:?} to '{t}'"))),
    })
}

fn polars_dtype(name: &str) -> Result<DataType, QplError> {
    Ok(match name {
        "f64" | "float"  => DataType::Float64,
        "f32"            => DataType::Float32,
        "i64" | "int"    => DataType::Int64,
        "i32"            => DataType::Int32,
        "i16"            => DataType::Int16,
        "i8"             => DataType::Int8,
        "u64"            => DataType::UInt64,
        "u32"            => DataType::UInt32,
        "u16"            => DataType::UInt16,
        "u8"             => DataType::UInt8,
        "bool"           => DataType::Boolean,
        "str" | "string" => DataType::String,
        _ => return Err(QplError::Runtime(format!("unknown cast type '{name}'"))),
    })
}

fn scan_file(path: &str) -> Result<LazyFrame, QplError> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "parquet" => LazyFrame::scan_parquet(path.into(), ScanArgsParquet::default())
            .map_err(|e| QplError::Runtime(e.to_string())),
        "csv" => LazyCsvReader::new(path.into()).finish()
            .map_err(|e| QplError::Runtime(e.to_string())),
        other => Err(QplError::Runtime(format!("unsupported file format '.{other}' (supported: parquet, csv)"))),
    }
}

fn apply_binop(left: Expr, right: Expr, op: &str) -> Result<Expr, QplError> {
    Ok(match op {
        "+"        => left + right,
        "-"        => left - right,
        "*"        => left * right,
        "%"        => left / right, // q uses % for division
        "="        => left.eq(right),
        "!=" | "<>"=> left.neq(right),
        "<"        => left.lt(right),
        "<="       => left.lt_eq(right),
        ">"        => left.gt(right),
        ">="       => left.gt_eq(right),
        "&"        => left.and(right),
        "|"        => left.or(right),
        _          => return Err(QplError::Runtime(format!("unknown operator '{op}'"))),
    })
}

fn apply_call(func: &str, mut args: Vec<Expr>) -> Result<Expr, QplError> {
    if args.is_empty() {
        return Err(QplError::Runtime(format!("'{func}' called with no args")));
    }
    let arg = args.remove(0); // all current builtins are unary
    Ok(match func {
        "sum"                   => arg.sum(),
        "avg" | "mean"          => arg.mean(),
        "min"                   => arg.min(),
        "max"                   => arg.max(),
        "count"                 => arg.count(),
        "first"                 => arg.first(),
        "last"                  => arg.last(),
        "std"  | "dev"          => arg.std(1),
        "var"                   => arg.var(1),
        "median" | "med"        => arg.median(),
        "abs"                   => arg.abs(),
        "neg"                   => -arg,
        "not"                   => arg.not(),
        "string"                => arg.cast(DataType::String),
        "distinct" | "n_unique" => arg.n_unique(),
        _ => return Err(QplError::Runtime(format!("unknown function '{func}'"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::compile;
    use crate::lexer::tokenise;
    use crate::parser::parse;

    fn make_vm() -> Vm {
        let df = df![
            "c1" => ["a", "b", "a", "c"],
            "c2" => [10i64, 20, 30, 15],
            "c3" => [1.0f64, 2.0, 3.0, 4.0],
        ].unwrap();
        let mut vm = Vm::new();
        vm.tables.insert("t".into(), df);
        vm
    }

    fn run(vm: &Vm, src: &str) -> DataFrame {
        let tokens = tokenise(src).expect("lex");
        let stmt   = parse(tokens).expect("parse");
        let prog   = compile(&stmt).expect("compile");
        vm.eval(prog).expect("eval")
    }

    fn i64s(df: &DataFrame, name: &str) -> Vec<i64> {
        df.column(name).unwrap().i64().unwrap().into_no_null_iter().collect()
    }

    fn f64s(df: &DataFrame, name: &str) -> Vec<f64> {
        df.column(name).unwrap().f64().unwrap().into_no_null_iter().collect()
    }

    fn strs(df: &DataFrame, name: &str) -> Vec<String> {
        df.column(name).unwrap().str().unwrap()
            .iter().flatten().map(str::to_owned).collect()
    }

    fn sorted(df: DataFrame, by: &str) -> DataFrame {
        df.sort([by], SortMultipleOptions::default()).unwrap()
    }

    // --- basic projections ---

    #[test]
    fn select_single_col() {
        let df = run(&make_vm(), "select c2 from t");
        assert_eq!(df.width(), 1);
        assert_eq!(i64s(&df, "c2"), vec![10, 20, 30, 15]);
    }

    #[test]
    fn select_multi_col() {
        let df = run(&make_vm(), "select c1, c2 from t");
        assert_eq!(df.width(), 2);
        assert_eq!(strs(&df, "c1"), vec!["a", "b", "a", "c"]);
        assert_eq!(i64s(&df, "c2"), vec![10, 20, 30, 15]);
    }

    #[test]
    fn select_all_cols() {
        let df = run(&make_vm(), "select from t");
        assert_eq!(df.height(), 4);
        assert_eq!(df.width(), 3);
    }

    // --- aliases ---

    #[test]
    fn explicit_alias() {
        let df = run(&make_vm(), "select x: c2 from t");
        assert!(df.column("x").is_ok(), "column 'x' missing");
        assert_eq!(i64s(&df, "x"), vec![10, 20, 30, 15]);
    }

    #[test]
    fn implicit_alias_colref() {
        let df = run(&make_vm(), "select c2 from t");
        assert!(df.column("c2").is_ok());
    }

    #[test]
    fn implicit_alias_binop_leftmost_leaf() {
        // leftmost leaf of c3*2.0 is c3 → result column is named "c3"
        let df = run(&make_vm(), "select c3*2.0 from t");
        assert!(df.column("c3").is_ok());
    }

    // --- arithmetic operators ---

    #[test]
    fn op_mul() {
        let df = run(&make_vm(), "select dbl: c3*2.0 from t");
        assert_eq!(f64s(&df, "dbl"), vec![2.0, 4.0, 6.0, 8.0]);
    }

    #[test]
    fn op_add() {
        let df = run(&make_vm(), "select s: c2+c2 from t");
        assert_eq!(i64s(&df, "s"), vec![20, 40, 60, 30]);
    }

    #[test]
    fn op_sub() {
        let df = run(&make_vm(), "select r: c2-5 from t");
        assert_eq!(i64s(&df, "r"), vec![5, 15, 25, 10]);
    }

    #[test]
    fn op_div() {
        // q uses % for division
        let df = run(&make_vm(), "select h: c2%2 from t");
        assert_eq!(i64s(&df, "h"), vec![5, 10, 15, 7]);
    }

    // --- where clause ---

    #[test]
    fn where_gt() {
        let df = run(&make_vm(), "select c1 from t where c2>15");
        assert_eq!(strs(&df, "c1"), vec!["b", "a"]);
    }

    #[test]
    fn where_lt() {
        let df = run(&make_vm(), "select c1 from t where c2<20");
        assert_eq!(strs(&df, "c1"), vec!["a", "c"]);
    }

    #[test]
    fn where_eq_string_literal() {
        let df = run(&make_vm(), r#"select c2 from t where c1="a""#);
        assert_eq!(i64s(&df, "c2"), vec![10, 30]);
    }

    #[test]
    fn where_eq_symbol() {
        // backtick symbol compiles to the same string literal at runtime
        let df = run(&make_vm(), "select c2 from t where c1=`a");
        assert_eq!(i64s(&df, "c2"), vec![10, 30]);
    }

    #[test]
    fn where_multiple_successive() {
        // c2>10 removes the row where c2=10; c2<30 then removes c2=30
        let df = run(&make_vm(), "select c2 from t where c2>10, c2<30");
        let mut v = i64s(&df, "c2");
        v.sort();
        assert_eq!(v, vec![15, 20]);
    }

    // --- whole-table aggregates (no by) ---

    #[test]
    fn agg_sum() {
        let df = run(&make_vm(), "select n: sum c2 from t");
        assert_eq!(i64s(&df, "n"), vec![75]);
    }

    #[test]
    fn agg_min() {
        let df = run(&make_vm(), "select n: min c2 from t");
        assert_eq!(i64s(&df, "n"), vec![10]);
    }

    #[test]
    fn agg_max() {
        let df = run(&make_vm(), "select n: max c2 from t");
        assert_eq!(i64s(&df, "n"), vec![30]);
    }

    #[test]
    fn agg_mean() {
        let df = run(&make_vm(), "select n: avg c2 from t");
        assert_eq!(f64s(&df, "n"), vec![18.75]);
    }

    #[test]
    fn agg_first() {
        let df = run(&make_vm(), "select n: first c1 from t");
        assert_eq!(strs(&df, "n"), vec!["a"]);
    }

    #[test]
    fn agg_last() {
        let df = run(&make_vm(), "select n: last c1 from t");
        assert_eq!(strs(&df, "n"), vec!["c"]);
    }

    // --- by clause (group-by + agg) ---

    #[test]
    fn by_sum() {
        let df = sorted(run(&make_vm(), "select total: sum c2 by c1 from t"), "c1");
        assert_eq!(strs(&df, "c1"),    vec!["a", "b", "c"]);
        assert_eq!(i64s(&df, "total"), vec![40, 20, 15]);
    }

    #[test]
    fn by_count() {
        let df = sorted(run(&make_vm(), "select n: count c2 by c1 from t"), "c1");
        let ns: Vec<u32> = df.column("n").unwrap().u32().unwrap()
            .into_no_null_iter().collect();
        assert_eq!(strs(&df, "c1"), vec!["a", "b", "c"]);
        assert_eq!(ns, vec![2, 1, 1]);
    }

    #[test]
    fn by_max() {
        let df = sorted(run(&make_vm(), "select hi: max c2 by c1 from t"), "c1");
        assert_eq!(strs(&df, "c1"), vec!["a", "b", "c"]);
        assert_eq!(i64s(&df, "hi"),  vec![30, 20, 15]);
    }

    // --- virtual column i ---

    #[test]
    fn select_icol() {
        let df = run(&make_vm(), "select i from t");
        // IColRef gets implicit alias "x"; polars adds it as u32
        let xs: Vec<u32> = df.column("x").unwrap().u32().unwrap()
            .into_no_null_iter().collect();
        assert_eq!(xs, vec![0, 1, 2, 3]);
    }

    // --- full query ---

    #[test]
    fn full_query() {
        // select total: sum c2 by c1 from t where c2>15
        // rows passing c2>15: b/20, a/30  →  grouped: a→30, b→20
        let df = sorted(
            run(&make_vm(), "select total: sum c2 by c1 from t where c2>15"),
            "c1",
        );
        assert_eq!(strs(&df, "c1"),    vec!["a", "b"]);
        assert_eq!(i64s(&df, "total"), vec![30, 20]);
    }

    // --- error cases ---

    #[test]
    fn unknown_table_is_runtime_error() {
        let vm = make_vm();
        let prog = compile(&parse(tokenise("select c1 from nope").unwrap()).unwrap()).unwrap();
        assert!(matches!(vm.eval(prog), Err(QplError::Runtime(_))));
    }
}

