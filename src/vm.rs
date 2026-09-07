use polars::io::utils::sync_on_close::SyncOnCloseType;
use polars::prelude::*;
use crate::ast::{self, TableSource, Value};
use crate::enums::{PolarsFrameExpr, PolarsStackArg};
use crate::lexer::tokenise;
use crate::parser::parse;
use crate::compiler::compile;
use crate::errors::QplError;
use crate::opcodes::Instruction;
use std::collections::HashMap;

pub struct Vm {
    pub tables: HashMap<String, DataFrame>,
    pub lazy_frames: HashMap<String, LazyFrame>,
    pub globals: HashMap<String, ast::Value>,
    /// When set (via the `\1 <path>` command), every line printed through
    /// [`Vm::emit`] is also appended here — kdb-style stdout redirection.
    pub stdout_log: Option<std::fs::File>,
}

enum StackObj {
    Expr(Expr),
    Frame(LazyFrame),
    Scalar(ast::Value),
    PolarsArg(PolarsStackArg)
}

impl StackObj {
    fn unwrap_expr(&self) -> Result<Expr, QplError> {
        match self {
            StackObj::Expr(e) => Ok(e.clone()),
            _ => Err(QplError::Runtime(format!("Expected Expr on stack, got {}", self.type_name()))),
        }
    }
    fn unwrap_frame(&self) -> Result<LazyFrame, QplError> {
        match self {
            StackObj::Frame(f) => Ok(f.clone()),
            _ => Err(QplError::Runtime(format!("Expected Frame on stack, got {}", self.type_name()))),
        }
    }
    fn unwrap_scalar(&self) -> Result<ast::Value, QplError> {
        match self {
            StackObj::Scalar(s) => Ok(s.clone()),
            _ => Err(QplError::Runtime(format!("Expected Scalar on stack, got {}", self.type_name()))),
        }
    }
    fn unwrap_polars_arg(&self) -> Result<PolarsStackArg, QplError> {
        match self {
            StackObj::PolarsArg(a) => Ok(a.clone()),
            _ => Err(QplError::Runtime(format!("Expected PolarsArg on stack, got {}", self.type_name()))),
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            StackObj::Expr(_) => "Expr",
            StackObj::Frame(_) => "Frame",
            StackObj::Scalar(_) => "Scalar",
            StackObj::PolarsArg(_) => "PolarsArg",
        }
    }
}

impl Vm {
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
            lazy_frames: HashMap::new(),
            globals: HashMap::new(),
            stdout_log: None,
        }
    }

    /// Point stdout logging at `path` (created / appended). Passing an empty
    /// path detaches any current log.
    pub fn set_stdout_log(&mut self, path: &str) -> Result<(), QplError> {
        if path.is_empty() {
            self.stdout_log = None;
            return Ok(());
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| QplError::Runtime(format!("cannot open log file '{path}': {e}")))?;
        self.stdout_log = Some(file);
        Ok(())
    }

    /// Print `text` to stdout, mirroring it to the stdout log if one is set.
    pub fn emit(&mut self, text: &str) {
        println!("{text}");
        if let Some(file) = self.stdout_log.as_mut() {
            use std::io::Write;
            let _ = writeln!(file, "{text}");
            let _ = file.flush();
        }
    }

    /// Returns a two-column table: `column` (name) and `dtype` for every field in `table_name`.
    pub fn schema(&self, mut lf: LazyFrame) -> Result<DataFrame, QplError> {
        let schema = lf.collect_schema()
            .map_err(|e| QplError::Runtime(e.to_string()))?;
        let names = schema
            .iter_names_and_dtypes()
            .map(|(name, dtype)| (name.to_string(), dtype.to_string()))
            .collect::<Vec<(String, String)>>();
        let (names, types): (Vec<String>, Vec<String>) = names.into_iter().unzip();
        df!["column" => names, "dtype" => types]
            .map_err(|e| QplError::Runtime(e.to_string()))
    }

    /// Evaluates a constant expression using only literals and globals (no DataFrame).
    pub fn eval_scalar(&self, expr: &ast::Expr) -> Result<ast::Value, QplError> {
        match expr {
            ast::Expr::Lit(v) => Ok(v.clone()),
            // outside a table expression `` `foo `` is a symbol (a distinct value kind)
            ast::Expr::Sym(s) => Ok(ast::Value::Sym(s.clone())),
            ast::Expr::ColRef(name) => self.globals.get(name)
                .cloned()
                .ok_or_else(|| QplError::Runtime(format!("undefined variable '{name}'"))),
            ast::Expr::BinOp { left, op, right } => {
                scalar_binop(self.eval_scalar(left)?, self.eval_scalar(right)?, op)
            }
            ast::Expr::Cast { target, expr } => match target {
                ast::CastTarget::Prim(dtype) => scalar_cast(self.eval_scalar(expr)?, dtype),
                // `` `$expr `` — intern a string into a symbol
                ast::CastTarget::Sym => match self.eval_scalar(expr)? {
                    ast::Value::Str(s) | ast::Value::Sym(s) => Ok(ast::Value::Sym(s)),
                    v => Err(QplError::Runtime(format!("cannot make a symbol from {v:?}"))),
                },
                ast::CastTarget::SymPhysical(_) | ast::CastTarget::Enum(_) => Err(QplError::Runtime(
                    "categorical / enum casts apply to columns, not scalars".into(),
                )),
            },
            other => Err(QplError::Runtime(format!("not supported in scalar context: {other:?}"))),
        }
    }

    /// Resolves a column-context cast target to a concrete Polars `DataType`.
    fn resolve_cast_target(&self, target: &ast::CastTarget) -> Result<DataType, QplError> {
        match target {
            ast::CastTarget::Prim(name) => polars_dtype(name),
            // `` `$col `` — a Polars Categorical (interned string pool), default u32 physical
            ast::CastTarget::Sym => Ok(DataType::from_categories(Categories::global())),
            // `` u8!`$col `` — Categorical with an explicit physical width; one
            // process-global pool per width, keyed by (name, namespace, physical)
            ast::CastTarget::SymPhysical(width) => {
                let phys = match width.as_str() {
                    "u8"  => CategoricalPhysical::U8,
                    "u16" => CategoricalPhysical::U16,
                    "u32" => CategoricalPhysical::U32,
                    other => return Err(QplError::Runtime(format!(
                        "categorical physical width must be u8/u16/u32, got '{other}'"
                    ))),
                };
                Ok(DataType::from_categories(Categories::new(
                    "qpl".into(), "".into(), phys,
                )))
            }
            // `` name::`$col `` — a Polars Enum whose categories come, in order,
            // from the global symbol vector `name`
            ast::CastTarget::Enum(name) => {
                let cats = match self.globals.get(name) {
                    Some(Value::SymVec(v)) => v,
                    Some(other) => return Err(QplError::Runtime(format!(
                        "'{name}' is not an enum (expected a symbol vector, got {other:?})"
                    ))),
                    None => return Err(QplError::Runtime(format!("undefined enum '{name}'"))),
                };
                let fcats = FrozenCategories::new(cats.iter().map(String::as_str))
                    .map_err(|e| QplError::Runtime(format!("invalid enum '{name}': {e}")))?;
                Ok(DataType::from_frozen_categories(fcats))
            }
        }
    }

    /// Executes a compiled program. Builds a LazyFrame plan for every
    /// instruction and materialises it only at `Result`.
    pub fn eval(&mut self, program: Vec<Instruction>) -> Result<EvalResult, QplError> {
        let needs_i = program.iter().any(|i| matches!(i, Instruction::PushIColRef));

        let mut stack: Vec<StackObj> = Vec::new();
        let mut frame: Option<LazyFrame> = None;
        let mut keys:  Vec<Expr> = Vec::new();
        let mut proj:  Vec<Expr> = Vec::new();

        // `lazy_mode` means the final result stays a LazyFrame plan instead of
        // being collected into a DataFrame.
        let mut lazy_mode = false;

        for instr in program {
            match instr {
                Instruction::FromSrc(tbl_source) => {
                    match tbl_source {
                        TableSource::InMem(name) => {
                            let lf = if let Some(lf) = self.lazy_frames.get(&name) {
                                // reading from a lazy binding is contagious: the
                                // result stays lazy unless explicitly collected.
                                lazy_mode = true;
                                lf.clone()
                            } else {
                                self.tables
                                    .get(&name)
                                    .ok_or_else(|| QplError::Runtime(format!("unknown table '{name}'")))?
                                    .clone()
                                    .lazy()
                            };
                            frame = Some(if needs_i { lf.with_row_index("i", None) } else { lf });
                        }
                        TableSource::Load(path) => {
                            let lf = load_file(&path)?;
                            frame = Some(if needs_i { lf.with_row_index("i", None) } else { lf });
                        }

                    }
                }
                Instruction::Sink => {
                    let path = pop1(&mut stack)?.unwrap_scalar()?;
                    let path_str = match path {
                        Value::Sym(s) | Value::Str(s) => s,
                        _ => return Err(QplError::Runtime(format!("expected a symbol or string path for sink, got {path:?}"))),
                    };
                    let lf = require_frame(&mut frame)?;
                    sink_file(lf, &path_str)?
                }
                Instruction::Lazy => {
                    lazy_mode = true;
                }
                Instruction::Collect => {
                    lazy_mode = false;
                }

                Instruction::PushPolarsArg(arg) => {
                    stack.push(StackObj::PolarsArg(arg));
                }

                Instruction::PushConst(val) => {
                    stack.push(StackObj::Expr(ast_val_to_expr(val)?));
                }

                Instruction::PushColRef(name) => {
                    // globals shadow column names, substituting a literal into the lazy plan
                    if let Some(val) = self.globals.get(&name) {
                        stack.push(StackObj::Expr(ast_val_to_expr(val.clone())?));
                    } else {
                        stack.push(StackObj::Expr(col(name.as_str())));
                    }
                }

                Instruction::PushIColRef => {
                    stack.push(StackObj::Expr(col("i")));
                }

                Instruction::BinOp(op) => {
                    let right = pop1(&mut stack)?.unwrap_expr()?;
                    let left  = pop1(&mut stack)?.unwrap_expr()?;
                    stack.push(StackObj::Expr(apply_binop(left, right, &op)?));
                }

                Instruction::Call { func, args_count } => {
                    // TODO support calls on lazyframes
                    let args = popn(&mut stack, args_count)?.into_iter()
                        .map(|o| o.unwrap_expr())
                        .collect::<Result<Vec<_>, _>>()?;
                    stack.push(StackObj::Expr(apply_call(&func, args)?));
                }

                Instruction::Case { branches } => {
                    let values = popn(&mut stack, branches * 2 + 1)?
                        .into_iter()
                        .map(|obj| obj.unwrap_expr())
                        .collect::<Result<Vec<_>, _>>()?;
                    let default = values.last().cloned().ok_or_else(|| QplError::Runtime("case expression has no default".into()))?;
                    let mut case_expr = default;
                    for pair in values[..values.len() - 1].chunks_exact(2).rev() {
                        case_expr = when(pair[0].clone()).then(pair[1].clone()).otherwise(case_expr);
                    }
                    stack.push(StackObj::Expr(case_expr));
                }

                Instruction::Alias { name } => {
                    let expr = pop1(&mut stack)?;
                    stack.push(match name {
                        Some(n) => match expr {
                            StackObj::Expr(e) => StackObj::Expr(e.alias(n.as_str())),
                            _ => return Err(QplError::Runtime("Expected expression on stack".into())),
                        },
                        None => match expr {
                            StackObj::Expr(e) => StackObj::Expr(e),
                            _ => return Err(QplError::Runtime("Expected expression on stack".into())),
                        },
                    });
                }
                Instruction::FrameExpr(expr) => {
                    match expr {
                        PolarsFrameExpr::Filter(n) => {
                            let preds = popn(&mut stack, n)?;
                            let mut lf = require_frame(&mut frame)?;
                            for pred in preds {
                                lf = lf.filter(pred.unwrap_expr()?);
                            }
                            frame = Some(lf);
                        },
                        PolarsFrameExpr::Join{ l, r } => {
                            let right = pop1(&mut stack)?.unwrap_frame()?;
                            let right_on = popn(&mut stack, r)?
                            .into_iter()
                                .map(|o| o.unwrap_expr())
                                .collect::<Result<Vec<_>, _>>()?;
                            let left_on = popn(&mut stack, l)?.into_iter()
                                .map(|o| o.unwrap_expr())
                                .collect::<Result<Vec<_>, _>>()?;
                            let join_arg = pop1(&mut stack)?.unwrap_polars_arg()?;
                            let left = pop1(&mut stack)?.unwrap_frame()?;
                            match join_arg {
                                PolarsStackArg::Join(join_type) => {
                                    frame = Some(
                                        left.join(
                                            right, 
                                            left_on,
                                            right_on,
                                            JoinArgs::new(join_type)
                                        )
                                    );
                                }
                            }
                        }
                        PolarsFrameExpr::Sort(sort_map) => {
                            let cols = sort_map.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>();
                            let ascs = sort_map.iter().map(|(_, descending)| *descending).collect::<Vec<_>>();
                            let lf = require_frame(&mut frame)?;
                            let sorted_lf = lf.sort_by_exprs(
                                    cols.iter().map(|c| col(c.as_str())).collect::<Vec<_>>().as_slice(),
                                    SortMultipleOptions::new()
                                        .with_order_descending_multi(ascs)
                            );
                            frame = Some(sorted_lf);
                        }
                        PolarsFrameExpr::Distinct => {
                            let lf = require_frame(&mut frame)?;
                            frame = Some(lf.unique_stable(None, UniqueKeepStrategy::First));
                        }
                        PolarsFrameExpr::Limit(limit) => {
                            let lf = require_frame(&mut frame)?;
                            frame = Some(lf.limit(limit as IdxSize));
                        }
                        PolarsFrameExpr::Drop(columns) => {
                            let lf = require_frame(&mut frame)?;
                            frame = Some(lf.drop(cols(columns)));
                        }
                        PolarsFrameExpr::Cols => {
                            let df = self.schema(frame.take().unwrap())?;
                            frame = Some(df.lazy());
                            // `cols` fully resolves the schema; always show it as a table.
                            lazy_mode = false;
                        }
                    }
                }

                Instruction::BuildKeys(n) => {
                    keys = popn(&mut stack, n)?.into_iter()
                        .map(|o| o.unwrap_expr())
                        .collect::<Result<Vec<_>, _>>()?;
                }

                Instruction::BuildProj { count, exclude, predicates } => {
                    let expressions = popn(&mut stack, count)?.into_iter()
                        .map(|o| o.unwrap_expr())
                        .collect::<Result<Vec<_>, _>>()?;
                    let predicates = popn(&mut stack, predicates)?
                        .into_iter()
                        .map(|o| o.unwrap_expr())
                        .collect::<Result<Vec<_>, _>>()?;
                    if exclude.is_empty() {
                        proj = expressions;
                    } else {
                        let predicate = predicates.into_iter().reduce(|left, right| left.and(right));
                        // `update col: … where p` keeps the old value where `p` is
                        // false — but a brand-new column has no old value, so it
                        // gets null there instead of `col(name)` (which would fail
                        // to resolve).
                        let schema = match (&predicate, frame.as_mut()) {
                            (Some(_), Some(lf)) => Some(
                                lf.collect_schema().map_err(|e| QplError::Runtime(e.to_string()))?,
                            ),
                            _ => None,
                        };
                        proj = expressions.into_iter().zip(exclude.iter()).map(|(expr, name)| {
                            let expr = if keys.is_empty() {
                                expr
                            } else {
                                expr.over(keys.clone()).map_err(|e| QplError::Runtime(e.to_string()))?
                            };
                            Ok(match &predicate {
                                Some(predicate) => {
                                    let old = if schema.as_ref().is_none_or(|s| s.contains(name.as_str())) {
                                        col(name.as_str())
                                    } else {
                                        lit(NULL)
                                    };
                                    when(predicate.clone()).then(expr).otherwise(old).alias(name)
                                }
                                None => expr,
                            })
                        }).collect::<Result<Vec<_>, QplError>>()?;
                        let all_except = (all() - by_name(exclude.clone(), false, false)).as_expr();
                        proj.insert(0, all_except);
                    }
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

                Instruction::Cast(target) => {
                    let expr = pop1(&mut stack)?.unwrap_expr()?;
                    stack.push(StackObj::Expr(expr.cast(self.resolve_cast_target(&target)?)));
                }

                Instruction::Eval(expr) => {
                    let val = self.eval_scalar(&expr)?;
                    stack.push(StackObj::Scalar(val));
                }

                Instruction::Result => {
                    let lf = require_frame(&mut frame)?;
                    stack.push(StackObj::Frame(lf));
                    frame = None; // clear frame so we don't accidentally use it after Result
                }

                Instruction::Assign(name) => {
                    match pop1(&mut stack)? {
                        StackObj::Scalar(s) => {
                            self.globals.insert(name.clone(), s.clone());
                        }
                        StackObj::Frame(lf) => {
                            if lazy_mode {
                                // keep the plan lazy under this name
                                self.tables.remove(&name);
                                self.lazy_frames.insert(name, lf);
                            } else {
                                self.lazy_frames.remove(&name);
                                self.tables.insert(name, lf.collect()
                                    .map_err(|e| QplError::Runtime(e.to_string()))?);
                            }
                        }
                        typ => return Err(QplError::Runtime(format!("Cannot assign '{}' to type {}: expected a table or scalar on stack", name, typ.type_name()))),
                    }
                }
            }
        }
        if !stack.is_empty() {
            let last_item = stack.pop().unwrap();
            match last_item {
                StackObj::Frame(lf) => {
                    if lazy_mode {
                        return Ok(EvalResult::Lazy(explain_plan(&lf)));
                    }
                    Ok(EvalResult::Table(lf.collect().map_err(|e| QplError::Runtime(e.to_string()))?))
                }
                StackObj::Scalar(s) => Ok(EvalResult::Scalar(s)),
                typ => Err(QplError::Runtime(format!("Unexpected type on stack: {}", typ.type_name()))),
            }
        } else {
            Ok(EvalResult::Stored)
        }
    }
}

pub enum EvalResult {
    Table(DataFrame),
    Stored,
    Scalar(ast::Value),
    /// a bare lazy table expression: carries the (optimised) query plan text.
    Lazy(String),
}

fn explain_plan(lf: &LazyFrame) -> String {
    lf.clone()
        .explain(true)
        .unwrap_or_else(|e| format!("<could not explain plan: {e}>"))
}

pub fn run_vm(source: &str, vm: &mut Vm) -> Result<EvalResult, QplError> {
    let tokens  = tokenise(source)?;
    let stmt    = parse(tokens)?;
    let program = compile(&stmt)?;
    vm.eval(program)
}

// ── helpers ────────────────────────────────────────────────────────────────

fn require_frame(f: &mut Option<LazyFrame>) -> Result<LazyFrame, QplError> {
    f.take().ok_or_else(|| QplError::Runtime("no active frame".into()))
}

fn pop1(stack: &mut Vec<StackObj>) -> Result<StackObj, QplError> {
    stack.pop().ok_or_else(|| QplError::Runtime("stack underflow".into()))
}

fn popn(stack: &mut Vec<StackObj>, n: usize) -> Result<Vec<StackObj>, QplError> {
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
        ast::Value::Sym(s)     => lit(s),
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

fn load_file(path: &str) -> Result<LazyFrame, QplError> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "parquet" | "pq" | "parq" => LazyFrame::scan_parquet(path.into(), ScanArgsParquet::default())
            .map_err(|e| QplError::Runtime(e.to_string())),
        "csv" => LazyCsvReader::new(path.into()).finish()
            .map_err(|e| QplError::Runtime(e.to_string())),
        other => Err(QplError::Runtime(format!("unsupported file format '.{other}' (supported: parquet, csv)"))),
    }
}

fn sink_file(lf: LazyFrame, path: &str) -> Result<(), QplError> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let file_write_format = match ext.as_str() {
        "parquet" | "pq" | "parq" => FileWriteFormat::Parquet(Arc::new(ParquetWriteOptions::default())),
        "csv" => FileWriteFormat::Csv(Default::default()),
        other => return Err(QplError::Runtime(format!("unsupported file format '.{other}' (supported: parquet, csv)"))),
    };
    lf
    .sink(
        SinkDestination::File { target: SinkTarget::Path(path.into()) },
        file_write_format,
        UnifiedSinkArgs {
            mkdir: true,
            maintain_order: true,
            sync_on_close: SyncOnCloseType::None,
            cloud_options: None,
            sinked_paths_callback: None
        }
    )
    .map_err(|e| QplError::Runtime(e.to_string()))?
    .collect_with_engine(Engine::Streaming)
    .map_err(|e| QplError::Runtime(e.to_string()))?;
    Ok(())
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

// fn ap

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

    fn run_instructions(mut vm: Vm, src: &str) -> EvalResult {
        let tokens = tokenise(src).expect("lex");
        let stmt   = parse(tokens).expect("parse");
        let prog   = compile(&stmt).expect("compile");
        vm.eval(prog).expect("eval")
    }

    fn run(vm: Vm, src: &str) -> DataFrame {
        match run_instructions(vm, src) {
            EvalResult::Table(df) => df,
            EvalResult::Stored => panic!("expected table result"),
            EvalResult::Scalar(s) => panic!("expected table result, got scalar {s:?}"),
            EvalResult::Lazy(_) => panic!("expected table result, got lazy plan"),
        }
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

    // scalars

    #[test]
    fn eval_int() {
        let vm = make_vm();
        let val = vm.eval_scalar(&ast::Expr::Lit(ast::Value::Int(42))).expect("eval");
        assert_eq!(val, ast::Value::Int(42));
    }

    #[test]
    fn eval_bare_symbol_is_a_symbol_value() {
        let vm = make_vm();
        assert_eq!(
            vm.eval_scalar(&ast::Expr::Sym("trades".into())).expect("eval"),
            ast::Value::Sym("trades".into()),
        );
    }

    #[test]
    fn eval_cast_string_var_to_symbol() {
        // `\`$o` resolves the string global `o` and interns it into a symbol
        let mut vm = make_vm();
        vm.globals.insert("o".into(), ast::Value::Str("out.parquet".into()));
        let expr = ast::Expr::Cast {
            target: ast::CastTarget::Sym,
            expr: Box::new(ast::Expr::ColRef("o".into())),
        };
        assert_eq!(vm.eval_scalar(&expr).expect("eval"), ast::Value::Sym("out.parquet".into()));
    }

    // scalar eval and assignment via instructions
    #[test]
    fn eval_assign_scalar() {
        let src = "x: 42";
        let tokens = tokenise(src).expect("lex");
        let stmt   = parse(tokens).expect("parse");
        let prog   = compile(&stmt).expect("compile");
        let mut vm = make_vm();
        vm.eval(prog).expect("eval");
        let val = vm.globals.get("x").expect("x exists");
        assert_eq!(val, &ast::Value::Int(42));
    }

    // --- categorical casts ---

    #[test]
    fn sym_cast_makes_a_categorical_column() {
        let df = run(make_vm(), "select cat: `$c1 from t");
        assert!(df.column("cat").unwrap().dtype().is_categorical());
    }

    #[test]
    fn sym_physical_cast_sets_the_categorical_physical_width() {
        let df = run(make_vm(), "select cat: u8!`$c1 from t");
        let dt = df.column("cat").unwrap().dtype();
        assert!(dt.is_categorical());
        assert_eq!(dt.cat_physical().unwrap(), CategoricalPhysical::U8);
    }

    #[test]
    fn unsupported_categorical_physical_width_is_an_error() {
        let tokens = tokenise("select cat: u64!`$c1 from t").expect("lex");
        let stmt   = parse(tokens).expect("parse");
        let prog   = compile(&stmt).expect("compile");
        assert!(make_vm().eval(prog).is_err());
    }

    #[test]
    fn enum_cast_builds_an_enum_column_from_a_global() {
        let mut vm = make_vm();
        vm.globals.insert("e".into(), ast::Value::SymVec(vec!["a".into(), "b".into(), "c".into()]));
        let df = run(vm, "select lvl: e::`$c1 from t");
        assert!(df.column("lvl").unwrap().dtype().is_enum());
    }

    #[test]
    fn enum_cast_maps_unknown_labels_to_null() {
        let mut vm = make_vm();
        vm.globals.insert("e".into(), ast::Value::SymVec(vec!["a".into(), "b".into()]));
        let df = run(vm, "select lvl: e::`$c1 from t");
        // c1 = [a, b, a, c] — the "c" row is not in the enum
        assert_eq!(df.column("lvl").unwrap().null_count(), 1);
    }

    #[test]
    fn enum_cast_with_undefined_global_is_an_error() {
        let tokens = tokenise("select lvl: nope::`$c1 from t").expect("lex");
        let stmt   = parse(tokens).expect("parse");
        let prog   = compile(&stmt).expect("compile");
        assert!(make_vm().eval(prog).is_err());
    }

    #[test]
    fn symbol_vector_assignment_is_stored_as_a_global() {
        let mut vm = make_vm();
        let prog = {
            let tokens = tokenise("e: `low`mid`high").expect("lex");
            compile(&parse(tokens).expect("parse")).expect("compile")
        };
        vm.eval(prog).expect("eval");
        assert_eq!(
            vm.globals.get("e"),
            Some(&ast::Value::SymVec(vec!["low".into(), "mid".into(), "high".into()])),
        );
    }

    // --- basic projections ---

    #[test]
    fn select_single_col() {
        let df = run(make_vm(), "select c2 from t");
        assert_eq!(df.width(), 1);
        assert_eq!(i64s(&df, "c2"), vec![10, 20, 30, 15]);
    }

    #[test]
    fn select_multi_col() {
        let df = run(make_vm(), "select c1, c2 from t");
        assert_eq!(df.width(), 2);
        assert_eq!(strs(&df, "c1"), vec!["a", "b", "a", "c"]);
        assert_eq!(i64s(&df, "c2"), vec![10, 20, 30, 15]);
    }

    #[test]
    fn select_all_cols() {
        let df = run(make_vm(), "select from t");
        assert_eq!(df.height(), 4);
        assert_eq!(df.width(), 3);
    }

    #[test]
    fn order_multiple_columns() {
        let df = run(make_vm(), "select from t order `c1 asc, `c2 desc");
        assert_eq!(strs(&df, "c1"), vec!["a", "a", "b", "c"]);
        assert_eq!(i64s(&df, "c2"), vec![30, 10, 20, 15]);
    }

    #[test]
    fn distinct_select_removes_duplicate_rows() {
        let df = run(make_vm(), "distinct select c1 from t");
        assert_eq!(strs(&df, "c1"), vec!["a", "b", "c"]);
    }

    #[test]
    fn limit_keyword_and_hash() {
        for source in ["2 limit select c1 from t", "2#select c1 from t", "2#`t"] {
            let df = run(make_vm(), source);
            assert_eq!(df.height(), 2);
            assert_eq!(strs(&df, "c1"), vec!["a", "b"]);
        }
    }

    #[test]
    fn drop_single_symbol_keyword_and_shorthand() {
        for source in ["`c2 drop select from t", "`c2 _ `t"] {
            let df = run(make_vm(), source);
            let names: Vec<&str> = df.get_column_names().iter().map(|name| name.as_str()).collect();
            assert_eq!(names, vec!["c1", "c3"]);
        }
    }

    #[test]
    fn update_preserves_unmodified_columns() {
        let df = run(make_vm(), "update c2: c2 * 2 from t");
        assert_eq!(df.width(), 3);
        assert_eq!(strs(&df, "c1"), vec!["a", "b", "a", "c"]);
        assert_eq!(i64s(&df, "c2"), vec![20, 40, 60, 30]);
        assert_eq!(f64s(&df, "c3"), vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn update_can_add_new_case_column() {
        let df = run(make_vm(), "update band: ?[c2>20;`high;c2>10;`mid;`low] from t");
        assert_eq!(strs(&df, "band"), vec!["low", "mid", "high", "mid"]);
        assert_eq!(df.width(), 4);
    }

    #[test]
    fn update_where_preserves_nonmatching_rows() {
        let df = run(make_vm(), "update c2: c2 * 2 from t where c1 = `a");
        assert_eq!(i64s(&df, "c2"), vec![20, 20, 60, 15]);
    }

    #[test]
    fn update_new_column_with_where_is_null_on_nonmatching_rows() {
        // c1 = [a, b, a, c]; the b/c rows get null, not an error
        let df = run(make_vm(), "update flag: c2 * 10 from t where c1 = `a");
        let flag: Vec<Option<i64>> = df.column("flag").unwrap().i64().unwrap().iter().collect();
        assert_eq!(flag, vec![Some(100), None, Some(300), None]);
    }

    #[test]
    fn update_by_applies_expression_per_group() {
        let df = run(make_vm(), "update c2: max c2 by c1 from t");
        assert_eq!(i64s(&df, "c2"), vec![30, 20, 30, 15]);
    }

    #[test]
    fn delete_where_removes_matching_rows() {
        let df = run(make_vm(), "delete from t where c2 > 15");
        assert_eq!(i64s(&df, "c2"), vec![10, 15]);
    }

    #[test]
    fn delete_columns_reuses_projection_exclusion() {
        let df = run(make_vm(), "delete `c2 from t");
        let names: Vec<&str> = df.get_column_names().iter().map(|name| name.as_str()).collect();
        assert_eq!(names, vec!["c1", "c3"]);
    }

    #[test]
    fn case_expression_returns_first_matching_value() {
        let df = run(make_vm(), "select bin: ?[c2>20;`high;c2>10;`mid;`low] from t");
        assert_eq!(strs(&df, "bin"), vec!["low", "mid", "high", "mid"]);
    }

    #[test]
    fn dictionary_sort_order_survives_assignment() {
        let mut vm = make_vm();
        assert!(matches!(run_vm("t2: `c1`c2!01b `t", &mut vm), Ok(EvalResult::Stored)));
        let df = run(vm, "select from t2");
        assert_eq!(strs(&df, "c1"), vec!["a", "a", "b", "c"]);
        assert_eq!(i64s(&df, "c2"), vec![30, 10, 20, 15]);
    }

    // --- aliases ---

    #[test]
    fn explicit_alias() {
        let df = run(make_vm(), "select x: c2 from t");
        assert!(df.column("x").is_ok(), "column 'x' missing");
        assert_eq!(i64s(&df, "x"), vec![10, 20, 30, 15]);
    }

    #[test]
    fn implicit_alias_colref() {
        let df = run(make_vm(), "select c2 from t");
        assert!(df.column("c2").is_ok());
    }

    #[test]
    fn implicit_alias_binop_leftmost_leaf() {
        // leftmost leaf of c3*2.0 is c3 → result column is named "c3"
        let df = run(make_vm(), "select c3*2.0 from t");
        assert!(df.column("c3").is_ok());
    }

    // --- arithmetic operators ---

    #[test]
    fn op_mul() {
        let df = run(make_vm(), "select dbl: c3*2.0 from t");
        assert_eq!(f64s(&df, "dbl"), vec![2.0, 4.0, 6.0, 8.0]);
    }

    #[test]
    fn op_add() {
        let df = run(make_vm(), "select s: c2+c2 from t");
        assert_eq!(i64s(&df, "s"), vec![20, 40, 60, 30]);
    }

    #[test]
    fn op_sub() {
        let df = run(make_vm(), "select r: c2-5 from t");
        assert_eq!(i64s(&df, "r"), vec![5, 15, 25, 10]);
    }

    #[test]
    fn op_div() {
        // q uses % for division
        let df = run(make_vm(), "select h: c2%2 from t");
        assert_eq!(i64s(&df, "h"), vec![5, 10, 15, 7]);
    }

    // --- where clause ---

    #[test]
    fn where_gt() {
        let df = run(make_vm(), "select c1 from t where c2>15");
        assert_eq!(strs(&df, "c1"), vec!["b", "a"]);
    }

    #[test]
    fn where_lt() {
        let df = run(make_vm(), "select c1 from t where c2<20");
        assert_eq!(strs(&df, "c1"), vec!["a", "c"]);
    }

    #[test]
    fn where_eq_string_literal() {
        let df = run(make_vm(), r#"select c2 from t where c1="a""#);
        assert_eq!(i64s(&df, "c2"), vec![10, 30]);
    }

    #[test]
    fn where_eq_symbol() {
        // backtick symbol compiles to the same string literal at runtime
        let df = run(make_vm(), "select c2 from t where c1=`a");
        assert_eq!(i64s(&df, "c2"), vec![10, 30]);
    }

    #[test]
    fn where_multiple_successive() {
        // c2>10 removes the row where c2=10; c2<30 then removes c2=30
        let df = run(make_vm(), "select c2 from t where c2>10, c2<30");
        let mut v = i64s(&df, "c2");
        v.sort();
        assert_eq!(v, vec![15, 20]);
    }

    // --- whole-table aggregates (no by) ---

    #[test]
    fn agg_sum() {
        let df = run(make_vm(), "select n: sum c2 from t");
        assert_eq!(i64s(&df, "n"), vec![75]);
    }

    #[test]
    fn agg_min() {
        let df = run(make_vm(), "select n: min c2 from t");
        assert_eq!(i64s(&df, "n"), vec![10]);
    }

    #[test]
    fn agg_max() {
        let df = run(make_vm(), "select n: max c2 from t");
        assert_eq!(i64s(&df, "n"), vec![30]);
    }

    #[test]
    fn agg_mean() {
        let df = run(make_vm(), "select n: avg c2 from t");
        assert_eq!(f64s(&df, "n"), vec![18.75]);
    }

    #[test]
    fn agg_first() {
        let df = run(make_vm(), "select n: first c1 from t");
        assert_eq!(strs(&df, "n"), vec!["a"]);
    }

    #[test]
    fn agg_last() {
        let df = run(make_vm(), "select n: last c1 from t");
        assert_eq!(strs(&df, "n"), vec!["c"]);
    }

    // --- by clause (group-by + agg) ---

    #[test]
    fn by_sum() {
        let df = sorted(run(make_vm(), "select total: sum c2 by c1 from t"), "c1");
        assert_eq!(strs(&df, "c1"),    vec!["a", "b", "c"]);
        assert_eq!(i64s(&df, "total"), vec![40, 20, 15]);
    }

    #[test]
    fn by_count() {
        let df = sorted(run(make_vm(), "select n: count c2 by c1 from t"), "c1");
        let ns: Vec<u32> = df.column("n").unwrap().u32().unwrap()
            .into_no_null_iter().collect();
        assert_eq!(strs(&df, "c1"), vec!["a", "b", "c"]);
        assert_eq!(ns, vec![2, 1, 1]);
    }

    #[test]
    fn by_max() {
        let df = sorted(run(make_vm(), "select hi: max c2 by c1 from t"), "c1");
        assert_eq!(strs(&df, "c1"), vec!["a", "b", "c"]);
        assert_eq!(i64s(&df, "hi"),  vec![30, 20, 15]);
    }

    // --- virtual column i ---

    #[test]
    fn select_icol() {
        let df = run(make_vm(), "select i from t");
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
            run(make_vm(), "select total: sum c2 by c1 from t where c2>15"),
            "c1",
        );
        assert_eq!(strs(&df, "c1"),    vec!["a", "b"]);
        assert_eq!(i64s(&df, "total"), vec![30, 20]);
    }

    // --- lazy / collect ---

    #[test]
    fn lazy_assign_stores_a_plan_not_a_table() {
        let mut vm = make_vm();
        assert!(matches!(run_vm("l: lazy select from t", &mut vm), Ok(EvalResult::Stored)));
        assert!(vm.lazy_frames.contains_key("l"));
        assert!(!vm.tables.contains_key("l"));
    }

    #[test]
    fn bare_lazy_expr_returns_a_plan() {
        let mut vm = make_vm();
        run_vm("l: lazy select from t", &mut vm).unwrap();
        assert!(matches!(run_vm("select c2 from l", &mut vm), Ok(EvalResult::Lazy(_))));
    }

    #[test]
    fn collect_materialises_a_lazy_binding() {
        let mut vm = make_vm();
        run_vm("l: lazy select from t", &mut vm).unwrap();
        assert!(matches!(run_vm("m: collect l", &mut vm), Ok(EvalResult::Stored)));
        assert!(vm.tables.contains_key("m"));
        assert!(!vm.lazy_frames.contains_key("m"));
        let df = run(vm, "select c2 from m");
        assert_eq!(i64s(&df, "c2"), vec![10, 20, 30, 15]);
    }

    #[test]
    fn update_assigned_back_to_a_lazy_binding_stays_lazy_and_extends_the_plan() {
        let mut vm = make_vm();
        run_vm("l: lazy select from t", &mut vm).unwrap();
        // explicit re-assignment is the only way to extend a lazy plan
        assert!(matches!(run_vm("l: update c2: c2 * 2 from l", &mut vm), Ok(EvalResult::Stored)));
        assert!(vm.lazy_frames.contains_key("l"));
        assert!(!vm.tables.contains_key("l"));
        run_vm("tm: collect l", &mut vm).unwrap();
        let df = run(vm, "select c2 from tm");
        assert_eq!(i64s(&df, "c2"), vec![20, 40, 60, 30]);
    }

    #[test]
    fn collect_over_eager_table_is_noop_passthrough() {
        let df = run(make_vm(), "collect select c2 from t");
        assert_eq!(i64s(&df, "c2"), vec![10, 20, 30, 15]);
    }

    // --- error cases ---

    #[test]
    fn unknown_table_is_runtime_error() {
        let mut vm = make_vm();
        let prog = compile(&parse(tokenise("select c1 from nope").unwrap()).unwrap()).unwrap();
        assert!(matches!(vm.eval(prog), Err(QplError::Runtime(_))));
    }
}

