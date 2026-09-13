use polars::io::utils::sync_on_close::SyncOnCloseType;
use polars::prelude::*;
use polars_ops::prelude::RoundMode;
use crate::ast::{self, TableSource, Value};
use crate::enums::{PolarsFrameExpr, PolarsStackArg, WindowFn};
use crate::lexer::tokenise;
use crate::parser::parse;
use crate::compiler::compile;
use crate::errors::QplError;
use crate::opcodes::Instruction;
use crate::resolve;
use crate::temporal;
use crate::helpers::rename_columns_snake_case;
use std::collections::HashMap;

pub struct Vm {
    pub tables: HashMap<String, DataFrame>,
    pub lazy_frames: HashMap<String, LazyFrame>,
    pub globals: HashMap<String, ast::Value>,
    /// User functions bound by `name: {[..] ..}`. A binding kind alongside
    /// `tables` / `lazy_frames`, not a first-class value; applied in value
    /// context only (see [`crate::resolve`]).
    pub functions: HashMap<String, ast::Function>,
    /// The active user-function call stack. Empty at the top level. Only the
    /// *innermost* frame is ever searched (see [`Vm::lookup`]) — a call sees its
    /// own params/locals and the session globals, never an enclosing caller's
    /// frame, so this is lexical (not dynamic) scoping despite being a stack.
    pub scopes: Vec<Scope>,
    /// When set (via the `\1 <path>` command), every line printed through
    /// [`Vm::emit`] is also appended here — kdb-style stdout redirection.
    pub stdout_log: Option<std::fs::File>,
    /// Session-wide knobs set from `.qpl.cfg key=value ...`.
    pub config: VmConfig,
    /// Open `hopen` connections, keyed by the `Value::Handle` id returned to
    /// the caller. `ipc` feature only.
    #[cfg(feature = "ipc")]
    pub connections: HashMap<i64, crate::ipc::ClientConn>,
    /// Outstanding `async dispatch` replies, keyed by the `Value::Future` id;
    /// removed and resolved by `await`. `ipc` feature only.
    #[cfg(feature = "ipc")]
    pub pending: HashMap<i64, crate::ipc::ReplyRx>,
    /// Next id handed out by `hopen`/`async dispatch` — one counter shared by
    /// both, so a `Value::Handle` and a `Value::Future` are never confusable.
    /// `ipc` feature only.
    #[cfg(feature = "ipc")]
    pub next_handle: i64,
    /// Transient permission for the one dispatched request currently being
    /// evaluated, if any — `Some(Read)`/`Some(Write)` only for the duration
    /// of `Vm::with_request_permission`'s closure, `None` otherwise. Never
    /// set for local REPL/script input, regardless of whether a port is
    /// open, so a read handle can only ever restrict a *remote* caller.
    /// `ipc` feature only.
    #[cfg(feature = "ipc")]
    pub request_mode: Option<crate::ipc::HandleMode>,
}

/// One user-function call frame: params and any names the body binds, isolated
/// from every other frame. Mirrors the four top-level namespaces on [`Vm`] —
/// the session globals are, in effect, frame zero at the bottom of the search.
#[derive(Default)]
pub struct Scope {
    pub globals: HashMap<String, ast::Value>,
    pub tables: HashMap<String, DataFrame>,
    pub lazy_frames: HashMap<String, LazyFrame>,
    pub functions: HashMap<String, ast::Function>,
}

/// The kind of binding [`Vm::lookup`] found for a name.
pub(crate) enum Lookup<'a> {
    Global(&'a ast::Value),
    LazyFrame(&'a LazyFrame),
    Table(&'a DataFrame),
    Function(&'a ast::Function),
}

/// Hard cap on user-function call nesting (a clearer error than a stack
/// overflow); checked against `Vm::scopes.len()`. A function call recurses
/// through the native Rust call stack (`apply_function` -> `eval` ->
/// `eval_value` -> ...), so this is calibrated against the ~8 MiB default main
/// thread stack the REPL runs on — a much smaller stack (a worker thread, or a
/// future WASM build) could still overflow before reaching this many levels.
pub(crate) const MAX_CALL_DEPTH: usize = 128;

/// Interpreter configuration set at run time via `.qpl.cfg`. To add a knob:
/// give it a field + default here and a match arm in [`VmConfig::set`] — nothing
/// else in the pipeline needs to change.
#[derive(Debug, Clone)]
pub struct VmConfig {
    /// max columns physically printed when rendering a table (`maxcol`)
    pub maxcol: usize,
    /// max rows physically printed when rendering a table (`maxrow`)
    pub maxrow: usize,
    /// rounding mode used by the `round` column function (`round_type`)
    pub round_type: RoundMode,
}

impl Default for VmConfig {
    fn default() -> Self {
        // mirror Polars' own display defaults
        Self { maxcol: 8, maxrow: 10, round_type: RoundMode::HalfToEven }
    }
}

impl VmConfig {
    /// Apply one `key=value` assignment. Unknown keys / bad values are errors.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), QplError> {
        match key {
            "maxcol" => self.maxcol = parse_cfg_usize(key, value)?,
            "maxrow" => self.maxrow = parse_cfg_usize(key, value)?,
            "round_type" => self.round_type = parse_round_type(value)?,
            _ => return Err(QplError::Runtime(format!(
                "unknown config '{key}' (known: maxcol, maxrow, round_type)"
            ))),
        }
        // the row/col limits are read by Polars from the environment at render time
        match key {
            "maxcol" => unsafe { std::env::set_var("POLARS_FMT_MAX_COLS", self.maxcol.to_string()) },
            "maxrow" => unsafe { std::env::set_var("POLARS_FMT_MAX_ROWS", self.maxrow.to_string()) },
            _ => {}
        }
        Ok(())
    }

    /// One `key=value` line per knob — printed by a bare `.qpl.cfg`.
    pub fn describe(&self) -> String {
        format!(
            "maxcol={}\nmaxrow={}\nround_type={}",
            self.maxcol, self.maxrow, round_type_name(self.round_type),
        )
    }
}

fn parse_cfg_usize(key: &str, value: &str) -> Result<usize, QplError> {
    value.parse().map_err(|_| {
        QplError::Runtime(format!("config '{key}' expects a non-negative integer, got '{value}'"))
    })
}

fn parse_round_type(value: &str) -> Result<RoundMode, QplError> {
    match value.to_ascii_uppercase().as_str() {
        "HALF_UP" => Ok(RoundMode::HalfAwayFromZero),
        "HALF_TO_EVEN" => Ok(RoundMode::HalfToEven),
        _ => Err(QplError::Runtime(format!(
            "round_type must be HALF_UP or HALF_TO_EVEN, got '{value}'"
        ))),
    }
}

fn round_type_name(mode: RoundMode) -> &'static str {
    match mode {
        RoundMode::HalfAwayFromZero => "HALF_UP",
        _ => "HALF_TO_EVEN",
    }
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
            functions: HashMap::new(),
            scopes: Vec::new(),
            stdout_log: None,
            config: VmConfig::default(),
            #[cfg(feature = "ipc")]
            connections: HashMap::new(),
            #[cfg(feature = "ipc")]
            pending: HashMap::new(),
            #[cfg(feature = "ipc")]
            next_handle: 0,
            #[cfg(feature = "ipc")]
            request_mode: None,
        }
    }

    /// Run `f` with the transient per-request permission set to `mode` for
    /// its duration, then cleared — so it can never leak into subsequent
    /// local input. Used by the `\port` server loop, wrapped around the
    /// evaluation of exactly one dispatched command.
    #[cfg(feature = "ipc")]
    pub fn with_request_permission<T>(
        &mut self,
        mode: crate::ipc::HandleMode,
        f: impl FnOnce(&mut Vm) -> T,
    ) -> T {
        self.request_mode = Some(mode);
        let result = f(self);
        self.request_mode = None;
        result
    }

    /// Errors out `what` when the request currently being evaluated (if any)
    /// arrived over a read-only `hopen` connection. A no-op for local
    /// input — `request_mode` is only ever set for the duration of one
    /// dispatched command (see `with_request_permission`).
    #[cfg_attr(not(feature = "ipc"), allow(unused_variables))]
    fn check_write_allowed(&self, what: &str) -> Result<(), QplError> {
        #[cfg(feature = "ipc")]
        if self.request_mode == Some(crate::ipc::HandleMode::Read) {
            return Err(QplError::Runtime(format!(
                "{what} is not allowed over a read-only connection (open with `w!hopen` for a write handle)"
            )));
        }
        Ok(())
    }

    /// Push a fresh call frame (a user-function call). The caller is
    /// responsible for popping it (`pop_scope`) on every exit path.
    pub(crate) fn push_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    /// Pop the innermost call frame, discarding whatever it bound.
    pub(crate) fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Resolve `name`: the active call frame (if any) first, then the session
    /// globals. Deliberately **not** a walk of the whole `scopes` stack — a
    /// function call only ever sees its own frame and the globals, never an
    /// enclosing caller's frame, which is what makes this lexical scoping
    /// rather than "whatever the dynamic call chain happens to have bound".
    pub(crate) fn lookup(&self, name: &str) -> Option<Lookup<'_>> {
        if let Some(scope) = self.scopes.last() {
            if let Some(v) = scope.globals.get(name) {
                return Some(Lookup::Global(v));
            }
            if let Some(lf) = scope.lazy_frames.get(name) {
                return Some(Lookup::LazyFrame(lf));
            }
            if let Some(df) = scope.tables.get(name) {
                return Some(Lookup::Table(df));
            }
            if let Some(f) = scope.functions.get(name) {
                return Some(Lookup::Function(f));
            }
        }
        if let Some(v) = self.globals.get(name) {
            return Some(Lookup::Global(v));
        }
        if let Some(lf) = self.lazy_frames.get(name) {
            return Some(Lookup::LazyFrame(lf));
        }
        if let Some(df) = self.tables.get(name) {
            return Some(Lookup::Table(df));
        }
        if let Some(f) = self.functions.get(name) {
            return Some(Lookup::Function(f));
        }
        None
    }

    /// `lookup`, narrowed to the scalar-global kind.
    pub(crate) fn lookup_global(&self, name: &str) -> Option<&ast::Value> {
        match self.lookup(name) {
            Some(Lookup::Global(v)) => Some(v),
            _ => None,
        }
    }

    /// `lookup`, narrowed to the user-function kind.
    pub(crate) fn lookup_function(&self, name: &str) -> Option<&ast::Function> {
        match self.lookup(name) {
            Some(Lookup::Function(f)) => Some(f),
            _ => None,
        }
    }

    /// Bind a scalar to `name` in the active call frame, or the session
    /// globals when there is none.
    pub(crate) fn bind_global(&mut self, name: String, val: ast::Value) {
        match self.scopes.last_mut() {
            Some(scope) => {
                scope.globals.insert(name, val);
            }
            None => {
                self.globals.insert(name, val);
            }
        }
    }

    /// Bind an eager table to `name`, scope-aware like `bind_global`.
    pub(crate) fn bind_table(&mut self, name: String, df: DataFrame) {
        match self.scopes.last_mut() {
            Some(scope) => {
                scope.lazy_frames.remove(&name);
                scope.tables.insert(name, df);
            }
            None => {
                self.lazy_frames.remove(&name);
                self.tables.insert(name, df);
            }
        }
    }

    /// Bind a lazy plan to `name`, scope-aware like `bind_global`.
    pub(crate) fn bind_lazy(&mut self, name: String, lf: LazyFrame) {
        match self.scopes.last_mut() {
            Some(scope) => {
                scope.tables.remove(&name);
                scope.lazy_frames.insert(name, lf);
            }
            None => {
                self.tables.remove(&name);
                self.lazy_frames.insert(name, lf);
            }
        }
    }

    /// Bind a function to `name`, scope-aware like `bind_global` — a function
    /// defined inside a call is local to that call, same as any other name.
    pub(crate) fn bind_function(&mut self, name: String, f: ast::Function) {
        match self.scopes.last_mut() {
            Some(scope) => {
                scope.globals.remove(&name);
                scope.tables.remove(&name);
                scope.lazy_frames.remove(&name);
                scope.functions.insert(name, f);
            }
            None => {
                self.globals.remove(&name);
                self.tables.remove(&name);
                self.lazy_frames.remove(&name);
                self.functions.insert(name, f);
            }
        }
    }

    /// Point stdout logging at `path` (created / appended). Passing an empty
    /// path detaches any current log.
    pub fn set_stdout_log(&mut self, path: &str) -> Result<(), QplError> {
        self.check_write_allowed("\\1 (stdout log)")?;
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
            ast::Expr::ColRef(name) => self.lookup_global(name)
                .cloned()
                .ok_or_else(|| QplError::Runtime(format!("undefined variable '{name}'"))),
            ast::Expr::BinOp { left, op, right } => {
                let l = self.eval_scalar(left)?;
                let r = self.eval_scalar(right)?;
                if l.as_vec().is_some() || r.as_vec().is_some() {
                    vector_binop(l, r, op)
                } else {
                    scalar_binop(l, r, op)
                }
            }
            // `.qpl.d` / `.qpl.t` / `.qpl.p` / `.qpl.n` — nullary now-functions
            ast::Expr::Call { func, args } if func.starts_with(".qpl.") && args.is_empty() => {
                temporal::now_value(func)
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

    /// Build the Polars expr for a column-context cast (`` type$expr ``) applied
    /// to `expr`. `frame` (the active frame, when there is one) is consulted to
    /// tell a string source column apart from an already-typed one, since the
    /// target alone doesn't say what `expr` is.
    ///
    /// String → temporal casts route through Polars' string datetime/time
    /// parser: `expr.cast(Date/Datetime/Time)` on a String is deprecated (gone
    /// in Polars 2.0). `to_datetime` infers the format per value (ISO *and*
    /// kdb's dotted `2024.03.15`), so `` `date$ `` / `` `month$ `` parse there
    /// and then truncate to a real `Date`; `` `timestamp$ `` keeps the time
    /// part; `` `time$ `` uses the time parser. Every *other* source dtype — a
    /// column already `Date` / `Datetime` / `Time`, or a raw integer offset
    /// (kdb `` `date$8000 ``-style) — takes a plain `.cast()`.
    pub(crate) fn build_cast_expr(
        &self,
        target: &ast::CastTarget,
        expr: Expr,
        frame: Option<&LazyFrame>,
    ) -> Result<Expr, QplError> {
        let dtype = self.resolve_cast_target(target)?;
        let temporal_target =
            matches!(dtype, DataType::Date | DataType::Datetime(_, _) | DataType::Time);
        Ok(if temporal_target && expr_dtype_is_string(frame, &expr)? {
            let to_datetime = |e: Expr| {
                e.str().to_datetime(
                    None,
                    None,
                    StrptimeOptions::default(),
                    lit("raise"),
                )
            };
            match &dtype {
                DataType::Date => to_datetime(expr).dt().date(),
                DataType::Datetime(_, _) => to_datetime(expr),
                DataType::Time => expr.str().to_time(StrptimeOptions::default()),
                _ => unreachable!("temporal_target guards this to Date/Datetime/Time"),
            }
        } else {
            expr.cast(dtype)
        })
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
                let cats = match self.lookup_global(name) {
                    Some(v @ Value::SymVec(_)) => v.vec_strings().map_err(QplError::Runtime)?,
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

    /// Executes a compiled program and reduces the final stack to an
    /// [`EvalResult`]. The instruction loop itself lives in [`Vm::run_program`]
    /// so it can be reused by [`Vm::eval_frame`].
    pub fn eval(&mut self, program: Vec<Instruction>) -> Result<EvalResult, QplError> {
        let (mut stack, lazy_mode) = self.run_program(program)?;
        if let Some(last_item) = stack.pop() {
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

    /// Run a compiled table expression and hand back the still-lazy frame plus
    /// whether it should stay lazy. Used by `resolve::eval_value` to compose a
    /// column expression with reductions / slices without collecting early.
    pub(crate) fn eval_frame(&mut self, mut program: Vec<Instruction>) -> Result<(LazyFrame, bool), QplError> {
        program.push(Instruction::Result);
        let (mut stack, lazy_mode) = self.run_program(program)?;
        match stack.pop() {
            Some(StackObj::Frame(lf)) => Ok((lf, lazy_mode)),
            _ => Err(QplError::Runtime("expected a table expression".into())),
        }
    }

    /// The instruction loop. Builds a LazyFrame plan for every instruction and
    /// returns the final operand stack and the `lazy` flag.
    fn run_program(&mut self, program: Vec<Instruction>) -> Result<(Vec<StackObj>, bool), QplError> {
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
                            let lf = match self.lookup(&name) {
                                // reading from a lazy binding is contagious: the
                                // result stays lazy unless explicitly collected.
                                Some(Lookup::LazyFrame(lf)) => {
                                    lazy_mode = true;
                                    lf.clone()
                                }
                                Some(Lookup::Table(df)) => df.clone().lazy(),
                                _ => return Err(QplError::Runtime(format!("unknown table '{name}'"))),
                            };
                            frame = Some(if needs_i { lf.with_row_index("i", None) } else { lf });
                        }
                        TableSource::Load(path) => {
                            let lf = rename_columns_snake_case(load_file(&path)?)?;
                            frame = Some(if needs_i { lf.with_row_index("i", None) } else { lf });
                        }

                    }
                }
                Instruction::Sink => {
                    self.check_write_allowed("sink")?;
                    let path = pop1(&mut stack)?.unwrap_scalar()?;
                    let path_str = match path {
                        Value::Str(s) => s,
                        _ => return Err(QplError::Runtime(format!("expected a string path for sink, got {path:?}"))),
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
                    if let Some(val) = self.lookup_global(&name) {
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

                Instruction::Round { decimals } => {
                    let expr = pop1(&mut stack)?.unwrap_expr()?;
                    stack.push(StackObj::Expr(expr.round(decimals, self.config.round_type)));
                }

                Instruction::Window { func, partition, order, rolling } => {
                    if let Some((agg, window)) = rolling {
                        let column = pop1(&mut stack)?.unwrap_expr()?;
                        stack.push(StackObj::Expr(
                            build_rolling_window(&agg, window, column, &partition, &order)?));
                    } else {
                        let target = match func {
                            WindowFn::Over => Some(pop1(&mut stack)?.unwrap_expr()?),
                            _ => None,
                        };
                        stack.push(StackObj::Expr(build_window(func, target, &partition, &order)?));
                    }
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
                    let casted = self.build_cast_expr(&target, expr, frame.as_ref())?;
                    stack.push(StackObj::Expr(casted));
                }

                Instruction::Eval(expr) => {
                    match resolve::eval_value(self, &expr)? {
                        resolve::EvalValue::Scalar(val) => stack.push(StackObj::Scalar(val)),
                        resolve::EvalValue::Frame { lf, lazy } => {
                            if lazy {
                                lazy_mode = true;
                            }
                            stack.push(StackObj::Frame(lf));
                        }
                    }
                }

                Instruction::DefFunc { name, params, body } => {
                    self.bind_function(name, ast::Function { params, body });
                }

                Instruction::Result => {
                    let lf = require_frame(&mut frame)?;
                    stack.push(StackObj::Frame(lf));
                    frame = None; // clear frame so we don't accidentally use it after Result
                }

                Instruction::Assign(name) => {
                    self.check_write_allowed("assignment")?;
                    match pop1(&mut stack)? {
                        StackObj::Scalar(s) => {
                            self.bind_global(name, s);
                        }
                        StackObj::Frame(lf) => {
                            if lazy_mode {
                                // keep the plan lazy under this name
                                self.bind_lazy(name, lf);
                            } else {
                                let df = lf.collect().map_err(|e| QplError::Runtime(e.to_string()))?;
                                self.bind_table(name, df);
                            }
                        }
                        typ => return Err(QplError::Runtime(format!("Cannot assign '{}' to type {}: expected a table or scalar on stack", name, typ.type_name()))),
                    }
                }
            }
        }
        Ok((stack, lazy_mode))
    }
}

#[derive(Debug)]
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

/// Does `expr`, resolved against the active `frame`'s schema, have dtype
/// `String`? Used to decide whether a `` `date$ `` / `` `timestamp$ `` /
/// `` `time$ `` cast should parse a string or plain-`.cast()` an already
/// temporal (or raw integer offset) source. `expr` is a moved-through
/// `PushColRef`/`PushConst`/… expression, not necessarily a bare column, so
/// this resolves it the same way Polars would: select it (schema-only, no
/// data touched) off a clone of the current plan.
fn expr_dtype_is_string(frame: Option<&LazyFrame>, expr: &Expr) -> Result<bool, QplError> {
    let Some(lf) = frame else { return Ok(false) };
    let schema = lf
        .clone()
        .select([expr.clone().alias("__qpl_cast_probe")])
        .collect_schema()
        .map_err(|e| QplError::Runtime(e.to_string()))?;
    Ok(schema
        .iter_names_and_dtypes()
        .next()
        .is_some_and(|(_, dtype)| matches!(dtype, DataType::String)))
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

pub(crate) fn ast_val_to_expr(val: ast::Value) -> Result<Expr, QplError> {
    Ok(match val {
        ast::Value::Int(n)     => lit(n),
        ast::Value::Float(n)   => lit(n),
        ast::Value::Str(s)     => lit(s),
        ast::Value::Sym(s)     => lit(s),
        ast::Value::Bool(b)    => lit(b),
        ast::Value::IntVec(s)   => s.lit(),
        ast::Value::FloatVec(s) => s.lit(),
        ast::Value::BoolVec(s)  => s.lit(),
        ast::Value::SymVec(s) | ast::Value::StrVec(s) => s.lit(),
        // temporal scalars carry a kdb offset; re-base to the Polars 1970 epoch
        // and give the literal its Polars dtype so it composes with columns.
        ast::Value::Date(d) => {
            lit(d + temporal::DAYS_2000_TO_1970).cast(DataType::Date)
        }
        ast::Value::Month(mo) => {
            let days = temporal::days_from_civil(2000 + mo.div_euclid(12), (mo.rem_euclid(12) + 1) as u32, 1);
            lit(days).cast(DataType::Date)
        }
        ast::Value::Time(ns)    => lit(ns).cast(DataType::Time),
        ast::Value::Minute(m)   => lit(m as i64 * 60_000_000_000).cast(DataType::Time),
        ast::Value::Second(s)   => lit(s as i64 * 1_000_000_000).cast(DataType::Time),
        ast::Value::Timestamp(ns) => {
            lit(ns + temporal::NS_2000_TO_1970).cast(DataType::Datetime(TimeUnit::Nanoseconds, None))
        }
        ast::Value::Timespan(ns) => lit(ns).cast(DataType::Duration(TimeUnit::Nanoseconds)),
        v @ (ast::Value::Handle(_) | ast::Value::Future(_)) => {
            return Err(QplError::Runtime(format!("{v:?} cannot be used in a query expression")))
        }
        // typed temporal vectors: same offset-rebasing as their scalar
        // counterparts, applied elementwise via Series/Expr arithmetic.
        ast::Value::DateVec(s) => (s.lit() + lit(temporal::DAYS_2000_TO_1970)).cast(DataType::Date),
        ast::Value::MonthVec(s) => {
            let days: Vec<i32> = s
                .i32()?
                .into_no_null_iter()
                .map(|mo| temporal::days_from_civil(2000 + mo.div_euclid(12), (mo.rem_euclid(12) + 1) as u32, 1))
                .collect();
            Series::new("".into(), days).lit().cast(DataType::Date)
        }
        ast::Value::TimeVec(s) => s.lit().cast(DataType::Time),
        ast::Value::MinuteVec(s) => {
            let ns: Vec<i64> = s.i32()?.into_no_null_iter().map(|m| m as i64 * 60_000_000_000).collect();
            Series::new("".into(), ns).lit().cast(DataType::Time)
        }
        ast::Value::SecondVec(s) => {
            let ns: Vec<i64> = s.i32()?.into_no_null_iter().map(|sec| sec as i64 * 1_000_000_000).collect();
            Series::new("".into(), ns).lit().cast(DataType::Time)
        }
        ast::Value::TimestampVec(s) => {
            (s.lit() + lit(temporal::NS_2000_TO_1970)).cast(DataType::Datetime(TimeUnit::Nanoseconds, None))
        }
        ast::Value::TimespanVec(s) => s.lit().cast(DataType::Duration(TimeUnit::Nanoseconds)),
    })
}

const NS_PER_DAY: i64 = 86_400_000_000_000;
const NS_PER_MIN: i64 = 60_000_000_000;
const NS_PER_SEC: i64 = 1_000_000_000;
const NS_PER_MS:  i64 = 1_000_000;

/// A temporal scalar as `(kind class, nanoseconds)` for comparison. Classes:
/// 0 = absolute instant (`date` / `timestamp` interchange), 1 = time of day
/// (`time` / `minute` / `second`), 2 = duration (`timespan`), 3 = month.
/// Comparison only crosses variants within the same class.
fn temporal_ns(v: &ast::Value) -> Option<(u8, i64)> {
    use ast::Value::*;
    Some(match *v {
        Timestamp(ns) => (0, ns),
        Date(d)       => (0, d as i64 * NS_PER_DAY),
        Time(ns)      => (1, ns),
        Minute(m)     => (1, m as i64 * NS_PER_MIN),
        Second(s)     => (1, s as i64 * NS_PER_SEC),
        Timespan(ns)  => (2, ns),
        Month(m)      => (3, m as i64),
        _ => return None,
    })
}

/// Nanosecond magnitude of a "duration-like" temporal scalar (`time`, `minute`,
/// `second`, `timespan`) added to / taken from a timestamp, date or time.
fn as_ns_delta(v: &ast::Value) -> Option<i64> {
    use ast::Value::*;
    Some(match *v {
        Time(ns) | Timespan(ns) => ns,
        Minute(m)               => m as i64 * NS_PER_MIN,
        Second(s)               => s as i64 * NS_PER_SEC,
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
        Date(d)       => Date(i32c(d as i64 + n)?),
        Month(m)      => Month(i32c(m as i64 + n)?),
        Minute(x)     => Minute(i32c(x as i64 + n)?),
        Second(x)     => Second(i32c(x as i64 + n)?),
        Time(ns)      => Time(ns.checked_add(n.checked_mul(NS_PER_MS)?)?),
        Timestamp(ns) => Timestamp(ns.checked_add(n)?),
        Timespan(ns)  => Timespan(ns.checked_add(n)?),
        _ => return None,
    })
}

/// All binops where at least one side is a temporal scalar. `None` lets
/// `scalar_binop` fall through to the numeric path.
fn temporal_binop(l: &ast::Value, r: &ast::Value, op: &str) -> Option<Result<ast::Value, QplError>> {
    use ast::Value::*;
    let overflow = || QplError::Runtime("temporal arithmetic overflowed".into());

    // comparison — only within the same kind class
    if matches!(op, "=" | "<>" | "!=" | "<" | ">" | "<=" | ">=") {
        let ((lc, a), (rc, b)) = (temporal_ns(l)?, temporal_ns(r)?);
        if lc != rc {
            return Some(Err(QplError::Runtime(format!("cannot compare {l:?} and {r:?}"))));
        }
        return Some(Ok(Bool(match op {
            "="        => a == b,
            "<>" | "!="=> a != b,
            "<"        => a < b,
            ">"        => a > b,
            "<="       => a <= b,
            _          => a >= b,
        })));
    }

    if !matches!(op, "+" | "-" | "*") {
        return if temporal_ns(l).is_some() || temporal_ns(r).is_some() {
            Some(Err(QplError::Runtime(format!("cannot apply '{op}' to {l:?} and {r:?}"))))
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
                Time(a)      => a.checked_add(d).map(Time).ok_or_else(overflow),
                Timespan(a)  => a.checked_add(d).map(Timespan).ok_or_else(overflow),
                Date(a)      => (a as i64).checked_mul(NS_PER_DAY)
                                    .and_then(|x| x.checked_add(d))
                                    .map(Timestamp).ok_or_else(overflow),
                _ => return Some(Err(QplError::Runtime(
                    format!("cannot apply '{op}' to {l:?} and {r:?}")))),
            });
        }
        if op == "+" && as_ns_delta(l).is_some() && matches!(r, Timestamp(_) | Date(_)) {
            return temporal_binop(r, l, op); // commute
        }
    }

    let checked = |o: Option<ast::Value>| o.ok_or_else(overflow);
    Some(match (l, r, op) {
        (Date(a),      Date(b),      "-") => Ok(Int((*a - *b) as i64)),
        (Timestamp(a), Timestamp(b), "-") => checked(a.checked_sub(*b).map(Timespan)),
        (Month(a),     Month(b),     "-") => Ok(Int((*a - *b) as i64)),
        (Timespan(a),  Int(b),       "*") => checked(a.checked_mul(*b).map(Timespan)),
        (Int(a),       Timespan(b),  "*") => checked(b.checked_mul(*a).map(Timespan)),
        _ => Err(QplError::Runtime(format!("cannot apply '{op}' to {l:?} and {r:?}"))),
    })
}

fn scalar_binop(l: ast::Value, r: ast::Value, op: &str) -> Result<ast::Value, QplError> {
    use ast::Value::*;

    if (temporal_ns(&l).is_some() || temporal_ns(&r).is_some())
        && let Some(res) = temporal_binop(&l, &r, op)
    {
        return res;
    }

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
        (Str(a) | Sym(a), Str(b) | Sym(b), "like") => Bool(like_match(&a, &b)?),
        (l, r, op) => return Err(QplError::Runtime(
            format!("cannot apply '{op}' to {l:?} and {r:?}"))),
    })
}

/// `<vector> op <scalar>` / `<scalar> op <vector>` / `<vector> op <vector>` —
/// at least one operand is a vector `Value`. Reuses the same literal→`Expr`
/// bridge (`ast_val_to_expr`) and operator table (`apply_binop`) the table
/// pipeline uses for column expressions, so a vector composes with a scalar
/// exactly like a Polars column would (broadcasting a length-1 side, and
/// applying the same dtype/temporal promotion rules Polars applies to
/// columns) — this is the native vectorised path `scalar_binop` doesn't cover.
fn vector_binop(l: ast::Value, r: ast::Value, op: &str) -> Result<ast::Value, QplError> {
    let l_expr = ast_val_to_expr(l)?;
    let r_expr = ast_val_to_expr(r)?;
    let expr = apply_binop(l_expr, r_expr, op)?.alias("r");
    let df = df!("_" => [0i64])?.lazy().select([expr]).collect()?;
    resolve::column_to_value(df.column("r")?)
}

/// Scalar counterpart of `apply_binop`'s `"like"` arm: matches `text` against
/// a q-glob `pattern` directly, with no Polars column involved.
fn like_match(text: &str, pattern: &str) -> Result<bool, QplError> {
    let regex_src = like_pattern_to_regex(pattern);
    regex::Regex::new(&regex_src)
        .map(|re| re.is_match(text))
        .map_err(|e| QplError::Runtime(format!("invalid 'like' pattern '{pattern}': {e}")))
}

/// Casts a scalar [`Value`] to the family named by `dtype` — the same type
/// names [`polars_dtype`] accepts. qpl scalars carry a single integer and a
/// single float type, so every `iN`/`uN` name folds to `Int` and `f32`/`f64`
/// to `Float`; the width only matters once the value reaches a column.
fn scalar_cast(val: ast::Value, dtype: &str) -> Result<ast::Value, QplError> {
    use ast::Value::*;
    // shared string parse for `Str` / `Sym` sources; accepts an int- or
    // float-looking literal
    let as_int = |s: &str| {
        let s = s.trim();
        s.parse::<i64>().ok().or_else(|| s.parse::<f64>().ok().map(|f| f as i64))
    };
    let bad = |v: &ast::Value| QplError::Runtime(format!("cannot cast {v:?} to '{dtype}'"));
    // temporal targets (`` `date$x ``, `"p"$"…"`, …) have their own path
    if matches!(dtype, "date" | "month" | "time" | "minute" | "second" | "timestamp" | "timespan") {
        return scalar_temporal_cast(val, dtype);
    }
    Ok(match dtype {
        "i64" | "int" | "long" | "i32" | "i16" | "i8" | "u64" | "u32" | "u16" | "u8" => match val {
            Int(n)          => Int(n),
            Float(f)        => Int(f as i64),
            Bool(b)         => Int(b as i64),
            // a temporal scalar unwraps to its kdb integer offset
            Date(n) | Month(n) | Minute(n) | Second(n) => Int(n as i64),
            Time(n) | Timestamp(n) | Timespan(n)       => Int(n),
            Str(ref s) | Sym(ref s) => Int(as_int(s)
                .ok_or_else(|| QplError::Runtime(format!("cannot parse '{s}' as '{dtype}'")))?),
            ref v           => return Err(bad(v)),
        },
        "f64" | "float" | "f32" => match val {
            Int(n)          => Float(n as f64),
            Float(f)        => Float(f),
            Bool(b)         => Float(b as i64 as f64),
            Str(ref s) | Sym(ref s) => Float(s.trim().parse::<f64>()
                .map_err(|_| QplError::Runtime(format!("cannot parse '{s}' as '{dtype}'")))?),
            ref v           => return Err(bad(v)),
        },
        "bool" => match val {
            Int(n)          => Bool(n != 0),
            Float(f)        => Bool(f != 0.0),
            Bool(b)         => Bool(b),
            Str(ref s) | Sym(ref s) => match s.trim().to_ascii_lowercase().as_str() {
                "true"  | "1" => Bool(true),
                "false" | "0" => Bool(false),
                _ => return Err(QplError::Runtime(format!("cannot parse '{s}' as 'bool'"))),
            },
            ref v           => return Err(bad(v)),
        },
        "str" | "string" => match val {
            Int(n)          => Str(n.to_string()),
            Float(f)        => Str(f.to_string()),
            Bool(b)         => Str(b.to_string()),
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
fn scalar_temporal_cast(val: ast::Value, target: &str) -> Result<ast::Value, QplError> {
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
            Date(d)       => d,
            Timestamp(ns) => ns.div_euclid(NS_PER_DAY) as i32,
            Month(m)      => temporal::days_from_civil(
                                 2000 + m.div_euclid(12), (m.rem_euclid(12) + 1) as u32, 1)
                             - temporal::DAYS_2000_TO_1970,
            _ => return None,
        })
    };
    let bad = || QplError::Runtime(format!("cannot cast {val:?} to '{target}'"));

    Ok(match target {
        "date" => match val {
            Date(_)  => val,
            Int(n)   => Date(n as i32),
            ref v    => Date(to_days(v).ok_or_else(bad)?),
        },
        "month" => match val {
            Month(_) => val,
            Int(n)   => Month(n as i32),
            ref v => {
                let d = to_days(v).ok_or_else(bad)?;
                let (y, m, _) = temporal::civil_from_days(d + temporal::DAYS_2000_TO_1970);
                Month((y - 2000) * 12 + (m as i32 - 1))
            }
        },
        "timestamp" => match val {
            Timestamp(_) => val,
            Int(n)       => Timestamp(n),
            ref v        => Timestamp(to_days(v).ok_or_else(bad)? as i64 * NS_PER_DAY),
        },
        "time" => match val {
            Time(_)       => val,
            Int(n)        => Time(n),
            Timestamp(ns) => Time(ns.rem_euclid(NS_PER_DAY)),
            Minute(m)     => Time(m as i64 * 60_000_000_000),
            Second(s)     => Time(s as i64 * 1_000_000_000),
            _             => return Err(bad()),
        },
        "minute" => match val {
            Minute(_)     => val,
            Int(n)        => Minute(n as i32),
            Time(ns)      => Minute((ns / 60_000_000_000) as i32),
            Timestamp(ns) => Minute((ns.rem_euclid(NS_PER_DAY) / 60_000_000_000) as i32),
            Second(s)     => Minute(s / 60),
            _             => return Err(bad()),
        },
        "second" => match val {
            Second(_)     => val,
            Int(n)        => Second(n as i32),
            Time(ns)      => Second((ns / 1_000_000_000) as i32),
            Timestamp(ns) => Second((ns.rem_euclid(NS_PER_DAY) / 1_000_000_000) as i32),
            Minute(m)     => Second(m * 60),
            _             => return Err(bad()),
        },
        "timespan" => match val {
            Timespan(_) => val,
            Int(n)      => Timespan(n),
            Time(ns)    => Timespan(ns),
            _           => return Err(bad()),
        },
        _ => return Err(QplError::Runtime(format!("unknown cast type '{target}'"))),
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
        "long"           => DataType::Int64,
        // temporal targets in column context. `month` has no Polars dtype so it
        // maps to `Date`; `minute` / `second` map to `Time` (truncation to the
        // unit is a Phase-2 concern).
        "date"           => DataType::Date,
        "month"          => DataType::Date,
        "time" | "minute" | "second" => DataType::Time,
        "timestamp"      => DataType::Datetime(TimeUnit::Nanoseconds, None),
        "timespan"       => DataType::Duration(TimeUnit::Nanoseconds),
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

/// Apply `.over(partition)` to `e`, honouring an optional window `order`
/// sub-clause. With no `order` this is a plain partition broadcast; with one it
/// sorts each partition by the order keys first (a single direction is applied
/// to every key — mixed asc/desc is only supported by the ranking verbs). The
/// result is mapped back onto the original row positions.
fn apply_over(e: Expr, part: &[Expr], order: &[(String, bool)]) -> Result<Expr, QplError> {
    if order.is_empty() {
        return e.over(part).map_err(|err| QplError::Runtime(err.to_string()));
    }
    let order_by: Vec<Expr> = order.iter().map(|(c, _)| col(c.as_str())).collect();
    let sort = SortOptions::default().with_order_descending(order[0].1);
    e.over_with_options(Some(part.to_vec()), Some((order_by, sort)), WindowMapping::GroupsToRows)
        .map_err(|err| QplError::Runtime(err.to_string()))
}

/// Build a fixed-size rolling-window aggregate (`<agg> <col> <n>!rolling over
/// `key [order `k asc]`). Unlike the plain window path the aggregate is *not*
/// pre-applied: `col` is the raw column and `agg` names the rolling reduction.
fn build_rolling_window(
    agg: &str,
    window: usize,
    column: Expr,
    partition: &[String],
    order: &[(String, bool)],
) -> Result<Expr, QplError> {
    let opts = RollingOptionsFixedWindow {
        window_size: window,
        min_periods: window,
        weights: None,
        center: false,
        fn_params: None,
    };
    let rolled = match agg {
        "sum"            => column.rolling_sum(opts),
        "avg" | "mean"   => column.rolling_mean(opts),
        "min"            => column.rolling_min(opts),
        "max"            => column.rolling_max(opts),
        "std" | "dev"    => column.rolling_std(opts),
        "var"            => column.rolling_var(opts),
        "median" | "med" => column.rolling_median(opts),
        other => return Err(QplError::Runtime(format!(
            "`rolling` supports sum/avg/min/max/std/var/median, not '{other}'"))),
    };
    let part: Vec<Expr> = partition.iter().map(|c| col(c.as_str())).collect();
    apply_over(rolled, &part, order)
}

/// Build a window expression. `WindowFn::Over` broadcasts `target` (an aggregate
/// or column expression) across each partition.
///
/// The ranking verbs need a single per-partition ordering key. For one `order`
/// column that key is the column itself. For several — with independent asc/desc
/// directions — each column is replaced by its dense per-partition rank (which
/// preserves order and value-equality) and the ranks are packed positionally
/// into one number, so ascending order matches the requested lexicographic
/// order and equal keys stay equal. That composite is then ranked with the
/// method for the verb (`Ordinal` = `rn`, `Min` = `rank`, `Dense` = `drank`).
fn build_window(
    func: WindowFn,
    target: Option<Expr>,
    partition: &[String],
    order: &[(String, bool)],
) -> Result<Expr, QplError> {
    let part: Vec<Expr> = partition.iter().map(|c| col(c.as_str())).collect();
    let over = |e: Expr, keys: &[Expr]| {
        e.over(keys).map_err(|err| QplError::Runtime(err.to_string()))
    };

    if let WindowFn::Over = func {
        return apply_over(target.expect("Over target"), &part, order);
    }

    // (rank_key, descending) — the single key the final rank is computed over.
    let (rank_key, descending) = if let [(name, desc)] = order {
        (col(name.as_str()), *desc)
    } else {
        // pack per-column dense ranks: composite = ((r1)*B2 + r2)*B3 + r3 ...
        // where Bi = (max r_i in partition) + 1 keeps digits from colliding.
        let mut composite: Option<Expr> = None;
        for (name, desc) in order {
            let ri = over(
                col(name.as_str())
                    .rank(RankOptions { method: RankMethod::Dense, descending: *desc }, None)
                    .cast(DataType::Int64),
                &part,
            )?;
            composite = Some(match composite {
                None => ri,
                Some(acc) => {
                    let base = over(ri.clone().max(), &part)? + lit(1i64);
                    acc * base + ri
                }
            });
        }
        (composite.expect("non-empty order"), false)
    };

    let method = match func {
        WindowFn::RowNumber => RankMethod::Ordinal,
        WindowFn::Rank => RankMethod::Min,
        WindowFn::DenseRank => RankMethod::Dense,
        WindowFn::Over => unreachable!(),
    };
    let ranked = over(rank_key.rank(RankOptions { method, descending }, None), &part)?;
    Ok(ranked.cast(DataType::Int64))
}

fn apply_binop(left: Expr, right: Expr, op: &str) -> Result<Expr, QplError> {
    if op == "like" {
        let pattern = match &right {
            Expr::Literal(lv) => lv.extract_str(),
            _ => None,
        }.ok_or_else(|| QplError::Runtime("'like's pattern must be a string literal".into()))?;
        let regex = like_pattern_to_regex(pattern);
        return Ok(left.str().contains(lit(regex), true));
    }
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
            '*' => { out.push_str(".*"); i += 1; }
            '?' => { out.push('.'); i += 1; }
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

// fn ap

pub(crate) fn apply_call(func: &str, mut args: Vec<Expr>) -> Result<Expr, QplError> {
    if args.is_empty() {
        return Err(QplError::Runtime(format!("'{func}' called with no args")));
    }
    // dyadic verbs (`<param> verb <col>`): the parser hands us `[value, param]`,
    // mirroring `round`. They never reach here with any other arity.
    if args.len() == 2 {
        let param = args.pop().unwrap();
        let value = args.pop().unwrap();
        return apply_dyadic(func, value, param);
    }
    let arg = args.remove(0);
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
        "mode" | "modal"        => arg.mode(false).sort(SortOptions::default()).first(),
        "skew"                  => arg.skew(false),
        "kurt" | "kurtosis"     => arg.kurtosis(true, false),
        "any"                   => arg.any(true),
        "all"                   => arg.all(true),
        "prod" | "product"      => arg.product(),
        "argmin"                => arg.arg_min(),
        "argmax"                => arg.arg_max(),
        "nnull" | "null_count"  => arg.null_count(),
        "cumsum"                => arg.cum_sum(false),
        "cummax"                => arg.cum_max(false),
        "cummin"                => arg.cum_min(false),
        "cumprod"               => arg.cum_prod(false),
        "cumcount"              => arg.cum_count(false),
        "ffill"                 => arg.fill_null_with_strategy(FillNullStrategy::Forward(None)),
        "bfill"                 => arg.fill_null_with_strategy(FillNullStrategy::Backward(None)),
        "abs"                   => arg.abs(),
        "neg"                   => -arg,
        "not"                   => arg.not(),
        "distinct" | "n_unique" => arg.n_unique(),
        _ => return Err(QplError::Runtime(format!("unknown function '{func}'"))),
    })
}

/// Dyadic column verbs, parsed q-style as `<param> verb <col>` (like `round`).
pub(crate) fn apply_dyadic(func: &str, value: Expr, param: Expr) -> Result<Expr, QplError> {
    Ok(match func {
        "quantile" | "pctl" => value.quantile(param, QuantileMethod::Linear),
        "shift" | "lag"     => value.shift(param),
        "lead"              => value.shift(-param),
        "diff"              => value.diff(param, polars::series::ops::NullBehavior::Ignore),
        "pctchange"         => value.pct_change(param),
        _ => return Err(QplError::Runtime(format!("unknown dyadic verb '{func}'"))),
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

    fn opt_i64s(df: &DataFrame, name: &str) -> Vec<Option<i64>> {
        df.column(name).unwrap().i64().unwrap().iter().collect()
    }

    fn opt_f64s(df: &DataFrame, name: &str) -> Vec<Option<f64>> {
        df.column(name).unwrap().f64().unwrap().iter().collect()
    }

    fn bools(df: &DataFrame, name: &str) -> Vec<bool> {
        df.column(name).unwrap().bool().unwrap().iter().flatten().collect()
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

    fn like(text: &str, pattern: &str) -> ast::Expr {
        ast::Expr::BinOp {
            left: Box::new(ast::Expr::Lit(ast::Value::Str(text.into()))),
            op: "like".into(),
            right: Box::new(ast::Expr::Lit(ast::Value::Str(pattern.into()))),
        }
    }

    #[test]
    fn like_matches_per_q_glob_semantics() {
        let vm = make_vm();
        let cases: &[(&str, &str, bool)] = &[
            ("quick",   "qu?ck",       true),   // ? = any single char
            ("quickly", "quick*",      true),   // * = any sequence, incl. empty
            ("quick",   "quick*",      true),
            ("brown",   "br[ao]wn",    true),   // char class
            ("brown",   "br[eiu]wn",   false),
            ("br0wn",   "br[0-3]wn",   true),   // range
            ("br9wn",   "br[0-3]wn",   false),
            ("brown",   "[^cf]rown",   true),   // negated class
            ("crown",   "[^cf]rown",   false),
            ("brown",   "brown",       true),   // no pattern chars = exact match
            ("brownx",  "brown",       false),
            ("BROWN",   "brown",       false),  // case-sensitive
            ("br*wn",   "br[*]wn",     true),   // escaping via a single-char class
            ("br?wn",   "br[?]wn",     true),
            ("br]wn",   "[bf]r[]]wn",  true),
            ("a[c",     "a[[]c",       true),
        ];
        for (text, pattern, expect) in cases {
            let got = vm.eval_scalar(&like(text, pattern)).expect("eval");
            assert_eq!(got, ast::Value::Bool(*expect), "{text:?} like {pattern:?}");
        }
    }

    #[test]
    fn like_treats_symbol_and_string_uniformly() {
        let vm = make_vm();
        let expr = ast::Expr::BinOp {
            left: Box::new(ast::Expr::Sym("quick".into())),
            op: "like".into(),
            right: Box::new(ast::Expr::Lit(ast::Value::Str("qu?ck".into()))),
        };
        assert_eq!(vm.eval_scalar(&expr).expect("eval"), ast::Value::Bool(true));
    }

    #[test]
    fn sink_and_load_round_trip_with_a_string_path() {
        let path = std::env::temp_dir().join("qpl_vm_test_sink_round_trip.csv");
        let path_str = path.to_str().unwrap();

        run_instructions(make_vm(), &format!("t sink \"{path_str}\""));

        let df = run(Vm::new(), &format!("load \"{path_str}\""));
        assert_eq!(df.height(), 4);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn sink_rejects_a_symbol_path_at_runtime() {
        let mut vm = make_vm();
        let tokens = tokenise("t sink `out.parquet").expect("lex");
        let stmt = parse(tokens).expect("parse");
        let prog = compile(&stmt).expect("compile");
        match vm.eval(prog) {
            Err(QplError::Runtime(_)) => {}
            Err(e) => panic!("expected a runtime error, got {e:?}"),
            Ok(_) => panic!("expected sink to reject a symbol path, but it succeeded"),
        }
    }

    // --- phase 2: per-connection read/write permission (`ipc` feature) ---

    #[cfg(feature = "ipc")]
    mod request_permission {
        use super::*;
        use crate::ipc::HandleMode;

        fn assert_read_only_rejects(src: &str) {
            let mut vm = make_vm();
            let err = vm
                .with_request_permission(HandleMode::Read, |vm| run_vm(src, vm))
                .expect_err(&format!("expected '{src}' to be rejected over a read handle"));
            assert!(matches!(err, QplError::Runtime(_)));
            // the transient flag must not leak into the next call
            assert_eq!(vm.request_mode, None);
        }

        fn assert_write_allows(src: &str) {
            let mut vm = make_vm();
            vm.with_request_permission(HandleMode::Write, |vm| run_vm(src, vm))
                .unwrap_or_else(|e| panic!("expected '{src}' to succeed over a write handle: {e}"));
            assert_eq!(vm.request_mode, None);
        }

        #[test]
        fn read_handle_rejects_assignment() {
            assert_read_only_rejects("x: 1");
        }

        #[test]
        fn read_handle_rejects_table_assignment() {
            assert_read_only_rejects("u: select from t");
        }

        #[test]
        fn read_handle_rejects_sink() {
            assert_read_only_rejects(r#"t sink "qpl_vm_test_read_only_sink.parquet""#);
            assert!(!std::path::Path::new("qpl_vm_test_read_only_sink.parquet").exists());
        }

        #[test]
        fn write_handle_allows_assignment() {
            assert_write_allows("x: 1");
        }

        #[test]
        fn write_handle_allows_sink() {
            let path = "qpl_vm_test_write_handle_sink.parquet";
            assert_write_allows(&format!(r#"t sink "{path}""#));
            assert!(std::path::Path::new(path).exists());
            std::fs::remove_file(path).ok();
        }

        #[test]
        fn read_handle_does_not_reject_a_plain_select() {
            // only assignment / sink / stdout-log are gated — an ordinary query
            // (no write) must still work over a read handle.
            let mut vm = make_vm();
            vm.with_request_permission(HandleMode::Read, |vm| run_vm("select from t", vm))
                .expect("a read-only select must succeed");
        }

        #[test]
        fn local_input_is_never_restricted_regardless_of_request_mode() {
            // `request_mode` only exists for the duration of a dispatched
            // request; local calls (nothing wraps them in
            // `with_request_permission`) always see `None` and are unaffected,
            // even on a server that also happens to have read handles connected.
            let mut vm = make_vm();
            assert_eq!(vm.request_mode, None);
            run_vm("x: 1", &mut vm).expect("local assignment always allowed");
        }
    }

    #[test]
    fn angle_bracket_pairs_do_not_sink() {
        let mut vm = make_vm();
        let tokens = tokenise("t >> \"qpl_vm_test_should_not_be_created.parquet\"").expect("lex");
        let stmt = parse(tokens).expect("parse");
        let prog = compile(&stmt).expect("compile");
        assert!(vm.eval(prog).is_err());
        assert!(!std::path::Path::new("qpl_vm_test_should_not_be_created.parquet").exists());
    }

    #[test]
    fn scalar_cast_covers_every_family() {
        use ast::Value::*;
        // float -> int truncates; the width name is accepted but folds to i64
        assert_eq!(scalar_cast(Float(45.3), "int").unwrap(), Int(45));
        assert_eq!(scalar_cast(Float(45.9), "u32").unwrap(), Int(45));
        assert_eq!(scalar_cast(Int(300), "i8").unwrap(), Int(300));
        // int/bool -> float
        assert_eq!(scalar_cast(Int(45), "f64").unwrap(), Float(45.0));
        assert_eq!(scalar_cast(Bool(true), "f32").unwrap(), Float(1.0));
        // -> bool
        assert_eq!(scalar_cast(Int(0), "bool").unwrap(), Bool(false));
        assert_eq!(scalar_cast(Float(3.0), "bool").unwrap(), Bool(true));
        assert_eq!(scalar_cast(Str("true".into()), "bool").unwrap(), Bool(true));
        // string parses into a number
        assert_eq!(scalar_cast(Str("45".into()), "int").unwrap(), Int(45));
        assert_eq!(scalar_cast(Str("3.9".into()), "int").unwrap(), Int(3));
        assert_eq!(scalar_cast(Str(" 3.5 ".into()), "f64").unwrap(), Float(3.5));
        // -> string
        assert_eq!(scalar_cast(Int(45), "str").unwrap(), Str("45".into()));
        assert_eq!(scalar_cast(Bool(true), "string").unwrap(), Str("true".into()));
    }

    #[test]
    fn scalar_cast_rejects_junk() {
        use ast::Value::*;
        assert!(scalar_cast(Str("nope".into()), "bool").is_err());
        assert!(scalar_cast(Str("abc".into()), "int").is_err());
        assert!(scalar_cast(Int(1), "widget").is_err());
    }

    #[test]
    fn eval_scalar_cast_end_to_end() {
        // the `l: int$45.3` case from the docs: a cast folds during scalar eval
        let vm = make_vm();
        let expr = ast::Expr::Cast {
            target: ast::CastTarget::Prim("int".into()),
            expr: Box::new(ast::Expr::Lit(ast::Value::Float(45.3))),
        };
        assert_eq!(vm.eval_scalar(&expr).unwrap(), ast::Value::Int(45));
    }

    #[test]
    fn scalar_cast_of_a_negative_literal() {
        // `l: int$-45.3` — lexes, parses (negative literal) and folds to -45
        let mut vm = make_vm();
        let prog = compile(&parse(tokenise("l: int$-45.3").unwrap()).unwrap()).unwrap();
        vm.eval(prog).unwrap();
        assert_eq!(vm.globals.get("l"), Some(&ast::Value::Int(-45)));
    }

    // --- temporal scalars ---

    fn scalar_of(src: &str) -> ast::Value {
        let mut vm = make_vm();
        let prog = compile(&parse(tokenise(src).unwrap()).unwrap()).unwrap();
        vm.eval(prog).unwrap();
        vm.globals.get("l").cloned().expect("l bound")
    }

    /// Compile + run `src`, expecting a lex/parse/compile/runtime error; returns its message.
    fn run_err(src: &str) -> String {
        let mut vm = make_vm();
        let res = tokenise(src)
            .and_then(parse)
            .and_then(|s| compile(&s))
            .and_then(|p| vm.eval(p));
        match res {
            Ok(_) => panic!("expected an error from {src:?}"),
            Err(e) => format!("{e:?}"),
        }
    }

    #[test]
    fn temporal_scalar_arithmetic() {
        use ast::Value::*;
        assert_eq!(scalar_of("l: 2024.03.15 + 10"), Date(8850));
        assert_eq!(scalar_of("l: 2024.03.20 - 2024.03.15"), Int(5));
        // timestamp + a time-of-day offset
        assert_eq!(
            scalar_of("l: 2000.01.01D00:00:00.0 + 00:30:00.0"),
            Timestamp(1_800_000_000_000),
        );
        // timestamp - timespan
        assert_eq!(
            scalar_of("l: 2000.01.01D01:00:00.0 - 0D01:00:00.0"),
            Timestamp(0),
        );
        // date + a bare timespan promotes to timestamp
        assert_eq!(scalar_of("l: 2000.01.02 + 0D00:00:00.000000001"), Timestamp(NS_PER_DAY + 1));
    }

    #[test]
    fn temporal_scalar_comparison() {
        use ast::Value::*;
        assert_eq!(scalar_of("l: 2024.03.15 < 2024.03.16"), Bool(true));
        assert_eq!(scalar_of("l: 2024.03.15 = 2024.03.15"), Bool(true));
        assert_eq!(scalar_of("l: 09:30 > 09:00"), Bool(true));
        // cross-variant, same kind class (date <-> timestamp, minute <-> second)
        assert_eq!(scalar_of("l: 2024.03.15 = 2024.03.15D00:00:00.0"), Bool(true));
        assert_eq!(scalar_of("l: 09:30 < 09:31:00"), Bool(true));
    }

    #[test]
    fn temporal_scalar_plus_integer_uses_the_operand_unit() {
        use ast::Value::*;
        assert_eq!(scalar_of("l: 09:30 + 5"), Minute(575));               // minutes
        assert_eq!(scalar_of("l: 12:00:00 + 5"), Second(43205));          // seconds
        assert_eq!(scalar_of("l: 12:30:00.000 + 5"), Time(45_000_005_000_000)); // ms
        assert_eq!(scalar_of("l: 2024.03m + 1"), Month(291));             // months
        assert_eq!(
            scalar_of("l: 2000.01.01D00:00:00.0 + 1"),
            Timestamp(1),                                                 // ns
        );
    }

    #[test]
    fn temporal_negative_literal_and_overflow() {
        use ast::Value::*;
        assert_eq!(scalar_of("l: -0D01:00:00.000000000"), Timespan(-3_600_000_000_000));
        // i32 offset overflow is an error, not a silent wrap
        assert!(run_err("l: 2024.03.15 + 3000000000").contains("overflow"));
        // i64 overflow on a scaled timespan is an error, not a debug panic
        assert!(run_err("l: 0D01:00:00.000000000 * 1000000000").contains("overflow"));
        // comparing across kind classes is rejected
        assert!(run_err("l: 0D01:00:00.0 = 2024.03.15").contains("cannot compare"));
    }

    #[test]
    fn temporal_casts() {
        use ast::Value::*;
        // timestamp -> date
        assert_eq!(scalar_of("l: `date$2024.03.15D12:30:00.0"), Date(8840));
        // date -> month
        assert_eq!(scalar_of("l: `month$2024.03.15"), Month(290));
        // date -> timestamp (midnight)
        assert_eq!(scalar_of("l: `timestamp$2000.01.02"), Timestamp(NS_PER_DAY));
        // temporal -> underlying kdb integer
        assert_eq!(scalar_of("l: `int$2024.03.15"), Int(8840));
        assert_eq!(scalar_of("l: `long$2000.01.01D00:00:00.000000001"), Int(1));
        // string parse via a kdb type code
        assert_eq!(
            scalar_of(r#"l: "p"$"2000.01.01D00:00:00.000000000""#),
            Timestamp(0),
        );
        assert_eq!(scalar_of(r#"l: "d"$"2024.03.15""#), Date(8840));
    }

    #[test]
    fn qpl_now_functions_evaluate_in_scalar_context() {
        assert!(matches!(scalar_of("l: .qpl.d"), ast::Value::Date(_)));
        assert!(matches!(scalar_of("l: .qpl.p"), ast::Value::Timestamp(_)));
    }

    #[test]
    fn temporal_literal_projects_as_a_typed_column() {
        let df = run(make_vm(), "select d: 2024.03.15, ts: 2024.03.15D09:30:00.0 from t");
        assert_eq!(df.column("d").unwrap().dtype(), &DataType::Date);
        assert!(matches!(
            df.column("ts").unwrap().dtype(),
            DataType::Datetime(TimeUnit::Nanoseconds, None)
        ));
    }

    #[test]
    fn string_temporal_casts_use_the_dedicated_parsers() {
        // String → temporal casts route through Polars' string parsers, not a
        // deprecated `expr.cast(<temporal>)`. `` `date$ `` / `` `month$ `` yield
        // a real `Date`; `` `timestamp$ `` a `Datetime` that keeps the time
        // part; `` `time$ `` a `Time`. `to_datetime`'s inference handles both
        // ISO and kdb's dotted `2024.03.15`, so nothing comes back null.
        let mut vm = make_vm();
        let src = df![
            "ds"  => ["2024.03.15", "2024-06-01", "2024-01-02"],
            "ts"  => ["2024-03-15T09:30:00", "2024-06-01T16:00:00", "2024-01-02T00:00:01"],
            "tm"  => ["09:30:00", "16:00:00", "00:00:01"],
        ]
        .unwrap();
        vm.tables.insert("d".into(), src);
        let df = run(
            vm,
            "select a: `date$ds, b: `timestamp$ts, c: `time$tm, e: `month$ds from d",
        );

        assert_eq!(df.column("a").unwrap().dtype(), &DataType::Date);
        assert_eq!(df.column("e").unwrap().dtype(), &DataType::Date);
        assert!(matches!(
            df.column("b").unwrap().dtype(),
            DataType::Datetime(_, _)
        ));
        assert_eq!(df.column("c").unwrap().dtype(), &DataType::Time);

        for name in ["a", "b", "c", "e"] {
            assert_eq!(
                df.column(name).unwrap().null_count(),
                0,
                "column {name} has nulls — parse failed"
            );
        }
    }

    #[test]
    fn string_date_cast_rejects_an_unparseable_value() {
        // strict by default: a value the inferred format cannot read aborts the
        // query rather than silently nulling.
        let mut vm = make_vm();
        let src = df!["ds" => ["2024-03-15", "not a date", "2024-01-02"]].unwrap();
        vm.tables.insert("d".into(), src);
        let prog = compile(&parse(tokenise("select a: `date$ds from d").unwrap()).unwrap()).unwrap();
        match vm.eval(prog) {
            Err(e) => assert!(e.to_string().contains("not a date"), "unexpected error: {e}"),
            Ok(_) => panic!("expected a parse failure on an unreadable date string"),
        }
    }

    #[test]
    fn temporal_casts_on_an_already_temporal_column_use_a_plain_cast() {
        // a column that's already `Date`/`Datetime`/`Time` (not `String`) must
        // NOT go through the string parser — it should plain-`.cast()`, same as
        // any other non-string source.
        let mut vm = make_vm();
        let src = df!["ts" => ["2024-03-15T09:30:00", "2024-06-01T16:00:00"]]
            .unwrap()
            .lazy()
            .select([col("ts").str().to_datetime(
                None,
                None,
                StrptimeOptions::default(),
                lit("raise"),
            )])
            .collect()
            .unwrap();
        vm.tables.insert("d".into(), src);
        let df = run(vm, "select a: `date$ts, b: `time$ts from d");
        assert_eq!(df.column("a").unwrap().dtype(), &DataType::Date);
        assert_eq!(df.column("b").unwrap().dtype(), &DataType::Time);
        for name in ["a", "b"] {
            assert_eq!(df.column(name).unwrap().null_count(), 0, "column {name} has nulls");
        }
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
        vm.globals.insert("e".into(), ast::sym_vec(vec!["a".into(), "b".into(), "c".into()]));
        let df = run(vm, "select lvl: e::`$c1 from t");
        assert!(df.column("lvl").unwrap().dtype().is_enum());
    }

    #[test]
    fn enum_cast_maps_unknown_labels_to_null() {
        let mut vm = make_vm();
        vm.globals.insert("e".into(), ast::sym_vec(vec!["a".into(), "b".into()]));
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
            Some(&ast::sym_vec(vec!["low".into(), "mid".into(), "high".into()])),
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
    fn limit_keyword_and_hash_on_tables() {
        // `2 limit …` and `n#` on a whole table stay table results
        for source in ["2 limit select c1 from t", "2#t", "2#select c1, c2 from t"] {
            let df = run(make_vm(), source);
            assert_eq!(df.height(), 2, "{source}");
            assert_eq!(strs(&df, "c1"), vec!["a", "b"]);
        }
    }

    #[test]
    fn hash_take_on_a_column_expression_is_a_list() {
        match run_instructions(make_vm(), "2#select c1 from t") {
            EvalResult::Scalar(v @ ast::Value::StrVec(_)) => assert_eq!(v.vec_strings().unwrap(), vec!["a", "b"]),
            EvalResult::Scalar(other) => panic!("expected a str list, got scalar {other:?}"),
            _ => panic!("expected a scalar str list"),
        }
    }

    #[test]
    fn vector_plus_scalar_is_elementwise() {
        let mut vm = make_vm();
        run_vm("l: 12 34", &mut vm).unwrap();
        assert_eq!(scalar_v(&mut vm, "l + 2"), ast::int_vec(vec![14, 36]));
        assert_eq!(scalar_v(&mut vm, "2 + l"), ast::int_vec(vec![14, 36]));
    }

    #[test]
    fn vector_plus_vector_is_elementwise() {
        let mut vm = make_vm();
        run_vm("l: 12 34", &mut vm).unwrap();
        assert_eq!(scalar_v(&mut vm, "l - (1 2)"), ast::int_vec(vec![11, 32]));
    }

    #[test]
    fn vector_comparison_yields_a_bool_vector() {
        let mut vm = make_vm();
        run_vm("l: 12 34", &mut vm).unwrap();
        assert_eq!(scalar_v(&mut vm, "l > 20"), ast::bool_vec(vec![false, true]));
    }

    #[test]
    fn mismatched_vector_lengths_are_a_runtime_error() {
        let mut vm = make_vm();
        run_vm("l: 1 2 3", &mut vm).unwrap();
        assert!(run_vm("l + (1 2)", &mut vm).is_err());
    }

    #[test]
    fn til_monadic_and_dyadic() {
        let mut vm = make_vm();
        assert_eq!(scalar_v(&mut vm, "til 5"), ast::int_vec(vec![0, 1, 2, 3, 4]));
        assert_eq!(scalar_v(&mut vm, "10 til 15"), ast::int_vec(vec![10, 11, 12, 13, 14]));
        assert_eq!(scalar_v(&mut vm, "til 0"), ast::int_vec(vec![]));
        assert_eq!(scalar_v(&mut vm, "5 til 5"), ast::int_vec(vec![]));
    }

    #[test]
    fn til_upper_below_lower_is_a_runtime_error() {
        assert!(run_vm("5 til 2", &mut make_vm()).is_err());
    }

    #[test]
    fn zip_builds_a_table_from_a_dict_of_named_lists() {
        let mut vm = make_vm();
        run_vm("a: til 5", &mut vm).unwrap();
        run_vm("b: 2 * til 5", &mut vm).unwrap();
        match run_vm("zip `cola`colb!a b", &mut vm).expect("run") {
            EvalResult::Table(df) => {
                let names: Vec<&str> = df.get_column_names().iter().map(|n| n.as_str()).collect();
                assert_eq!(names, vec!["cola", "colb"]);
                assert_eq!(
                    df.column("cola").unwrap().i64().unwrap().into_no_null_iter().collect::<Vec<_>>(),
                    vec![0, 1, 2, 3, 4],
                );
                assert_eq!(
                    df.column("colb").unwrap().i64().unwrap().into_no_null_iter().collect::<Vec<_>>(),
                    vec![0, 2, 4, 6, 8],
                );
            }
            _ => panic!("expected a table"),
        }
    }

    #[test]
    fn zip_with_a_single_key_dict() {
        let mut vm = make_vm();
        run_vm("a: til 3", &mut vm).unwrap();
        match run_vm("zip `only!a", &mut vm).expect("run") {
            EvalResult::Table(df) => {
                let names: Vec<&str> = df.get_column_names().iter().map(|n| n.as_str()).collect();
                assert_eq!(names, vec!["only"]);
            }
            _ => panic!("expected a table"),
        }
    }

    #[test]
    fn zip_rejects_mismatched_column_lengths() {
        let mut vm = make_vm();
        run_vm("a: til 5", &mut vm).unwrap();
        run_vm("c: til 3", &mut vm).unwrap();
        assert!(run_vm("zip `cola`colb!a c", &mut vm).is_err());
    }

    fn scalar_v(vm: &mut Vm, src: &str) -> ast::Value {
        match run_vm(src, vm).expect("run") {
            EvalResult::Scalar(v) => v,
            _ => panic!("expected a scalar result"),
        }
    }

    #[test]
    fn every_vec_kind_round_trips_through_scalarise_and_take() {
        use ast::VecKind::*;
        let cases: Vec<(ast::VecKind, ast::Value)> = vec![
            (Int, ast::int_vec(vec![1, 2, 3])),
            (Float, ast::float_vec(vec![1.0, 2.0, 3.0])),
            (Bool, ast::bool_vec(vec![true, false, true])),
            (Sym, ast::sym_vec(vec!["a".into(), "b".into(), "c".into()])),
            (Str, ast::str_vec(vec!["a".into(), "b".into(), "c".into()])),
            (Date, ast::date_vec(vec![0, 1, 2])),
            (Month, ast::month_vec(vec![0, 1, 2])),
            (Time, ast::time_vec(vec![0, 1, 2])),
            (Minute, ast::minute_vec(vec![0, 1, 2])),
            (Second, ast::second_vec(vec![0, 1, 2])),
            (Timestamp, ast::timestamp_vec(vec![0, 1, 2])),
            (Timespan, ast::timespan_vec(vec![0, 1, 2])),
        ];
        for (kind, v) in cases {
            let (got_kind, s) = v.as_vec().expect("is a vector");
            assert_eq!(got_kind, kind);
            assert_eq!(s.len(), 3);
        }
    }

    #[test]
    fn drop_single_symbol_keyword_and_shorthand() {
        for source in ["`c2 drop select from t", "`c2 _ t"] {
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
        assert!(matches!(run_vm("t2: `c1`c2!01b t", &mut vm), Ok(EvalResult::Stored)));
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
    fn where_like_exact_match() {
        let df = run(make_vm(), r#"select c2 from t where c1 like "a""#);
        assert_eq!(i64s(&df, "c2"), vec![10, 30]);
    }

    #[test]
    fn where_like_char_class() {
        let df = sorted(run(make_vm(), r#"select c2 from t where c1 like "[ab]""#), "c2");
        assert_eq!(i64s(&df, "c2"), vec![10, 20, 30]);
    }

    #[test]
    fn where_like_negated_char_class() {
        let df = run(make_vm(), r#"select c2 from t where c1 like "[^ab]""#);
        assert_eq!(i64s(&df, "c2"), vec![15]);
    }

    #[test]
    fn where_like_wildcard_on_symbol() {
        // symbols and strings are matched uniformly
        let df = run(make_vm(), "select c2 from t where c1 like `a");
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
    fn agg_mode() {
        let df = run(make_vm(), "select n: mode c1 from t");
        assert_eq!(strs(&df, "n"), vec!["a"]);
    }

    #[test]
    fn agg_modal_alias() {
        // ties resolve to the smallest value (mode list is sorted)
        let df = run(make_vm(), "select n: modal c2 from t");
        assert_eq!(i64s(&df, "n"), vec![10]);
    }

    #[test]
    fn agg_any_all_prod_argmax() {
        let df = run(make_vm(),
            "select a: any c2 > 25, b: all c2 > 5, p: prod c3 from t");
        assert_eq!(bools(&df, "a"), vec![true]);
        assert_eq!(bools(&df, "b"), vec![true]);
        assert_eq!(f64s(&df, "p"), vec![24.0]); // 1*2*3*4
        let m = run(make_vm(), "select m: argmax c2 from t"); // 30 is at row 2
        assert_eq!(m.column("m").unwrap().u32().unwrap().get(0), Some(2));
    }

    #[test]
    fn dyadic_quantile() {
        // c2 = [10, 20, 30, 15]; linear p50 over the whole column = 17.5
        let df = run(make_vm(), "select q: 0.5 quantile c2 from t");
        assert_eq!(f64s(&df, "q"), vec![17.5]);
    }

    #[test]
    fn dyadic_shift_and_diff() {
        let df = run(make_vm(), "select s: 1 shift c2, d: 1 diff c2 from t");
        assert_eq!(opt_i64s(&df, "s"), vec![None, Some(10), Some(20), Some(30)]);
        assert_eq!(opt_i64s(&df, "d"), vec![None, Some(10), Some(10), Some(-15)]);
    }

    #[test]
    fn cumsum_over_partition_in_order() {
        // c1 partitions: a{c2:10,30}, b{20}, c{15}; cumsum in ascending c2 order,
        // mapped back to the original row positions [a10, b20, a30, c15].
        let df = run(make_vm(), "select r: cumsum c2 over `c1 order `c2 asc from t");
        assert_eq!(i64s(&df, "r"), vec![10, 20, 40, 15]);
    }

    #[test]
    fn rolling_window_sum_over_partition() {
        // partition a has c3 {1.0, 3.0} in order -> [null, 4.0]; singletons -> null
        let df = run(make_vm(), "select r: sum c3 over `c1 order `c3 asc rolling 2 from t");
        assert_eq!(opt_f64s(&df, "r"), vec![None, None, Some(4.0), None]);
    }

    #[test]
    fn rolling_rejects_unsupported_aggregate() {
        let mut vm = make_vm();
        let src = "select r: first c3 over `c1 order `c3 asc rolling 2 from t";
        let tokens = tokenise(src).unwrap();
        let stmt = parse(tokens).unwrap();
        let prog = compile(&stmt).unwrap();
        assert!(vm.eval(prog).is_err());
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
    fn by_key_reprojected_by_name_does_not_duplicate_the_column() {
        // regression: `group_by(keys).agg(proj)` already carries the key
        // column through, so also projecting it by name used to hand Polars
        // two columns called "c1" and panic.
        let df = sorted(run(make_vm(), "select c1, c2, r: 1 diff c2 by c1 from t"), "c1");
        // one row per group; "c1" appears exactly once in the schema (not
        // duplicated by both the group key and the projection)
        assert_eq!(strs(&df, "c1"), vec!["a", "b", "c"]);
        assert_eq!(df.get_column_names().iter().filter(|n| n.as_str() == "c1").count(), 1);
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

    #[test]
    fn from_accepts_a_nested_select() {
        let df = sorted(run(make_vm(), "select from select c2 from t"), "c2");
        assert_eq!(df.get_column_names(), vec!["c2"]);
        assert_eq!(i64s(&df, "c2"), vec![10, 15, 20, 30]);
    }

    #[test]
    fn where_applies_to_the_outer_select_around_a_nested_from() {
        let df = sorted(run(make_vm(), "select c2 from select c1, c2 from t where c2>10"), "c2");
        assert_eq!(i64s(&df, "c2"), vec![15, 20, 30]);
    }

    #[test]
    fn cols_accepts_a_nested_select() {
        let df = run(make_vm(), "cols select c1, c2 from t where c2>0");
        assert_eq!(strs(&df, "column"), vec!["c1", "c2"]);
    }

    // --- join ---

    fn make_join_vm() -> Vm {
        let mut vm = Vm::new();
        vm.tables.insert("trades".into(), df![
            "sym"   => ["a", "a", "b"],
            "price" => [10i64, 20, 30],
        ].unwrap());
        vm.tables.insert("quotes".into(), df![
            "sym" => ["a", "b", "c"],
            "bid"  => [1i64, 2, 3],
        ].unwrap());
        vm
    }

    #[test]
    fn join_right_side_is_a_bare_name_without_parens() {
        let df = sorted(run(make_join_vm(), "select price, bid from trades `sym lj quotes `sym"), "price");
        assert_eq!(i64s(&df, "price"), vec![10, 20, 30]);
        assert_eq!(opt_i64s(&df, "bid"), vec![Some(1), Some(1), Some(2)]);
    }

    #[test]
    fn join_right_side_rejects_a_table_expr_without_parens() {
        let tokens = tokenise("select price, bid from trades `sym lj distinct quotes `sym").expect("lex");
        assert!(parse(tokens).is_err());
    }

    #[test]
    fn join_right_side_accepts_a_parenthesised_table_expr() {
        let df = sorted(
            run(make_join_vm(), "select price, bid from trades `sym lj (select sym, bid from quotes where bid > 1) `sym"),
            "price",
        );
        assert_eq!(i64s(&df, "price"), vec![10, 20, 30]);
        // the quote for sym "a" (bid=1) is filtered out of the join's right side
        assert_eq!(opt_i64s(&df, "bid"), vec![None, None, Some(2)]);
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

    // --- round column function + config ---

    fn round_vm() -> Vm {
        // c3 = [0.5, 1.5, 2.5, 0.125]
        let df = df![
            "c3" => [0.5f64, 1.5, 2.5, 0.125],
        ].unwrap();
        let mut vm = Vm::new();
        vm.tables.insert("t".into(), df);
        vm
    }

    #[test]
    fn round_defaults_to_half_to_even() {
        let df = run(round_vm(), "select r: 0 round c3 from t");
        assert_eq!(f64s(&df, "r"), vec![0.0, 2.0, 2.0, 0.0]);
    }

    #[test]
    fn round_type_half_up_rounds_half_away_from_zero() {
        let mut vm = round_vm();
        vm.config.set("round_type", "HALF_UP").unwrap();
        let df = run(vm, "select r: 0 round c3 from t");
        assert_eq!(f64s(&df, "r"), vec![1.0, 2.0, 3.0, 0.0]);
    }

    #[test]
    fn round_honours_precision() {
        let df = run(round_vm(), "select r: 2 round c3 from t");
        assert_eq!(f64s(&df, "r"), vec![0.5, 1.5, 2.5, 0.12]);
    }

    #[test]
    fn config_set_rejects_unknown_key_and_bad_value() {
        let mut cfg = VmConfig::default();
        assert!(cfg.set("nope", "1").is_err());
        assert!(cfg.set("maxrow", "abc").is_err());
        assert!(cfg.set("round_type", "sideways").is_err());
        cfg.set("maxrow", "42").unwrap();
        cfg.set("maxcol", "7").unwrap();
        cfg.set("round_type", "half_up").unwrap(); // case-insensitive
        assert_eq!(cfg.maxrow, 42);
        assert_eq!(cfg.maxcol, 7);
        assert_eq!(cfg.round_type, RoundMode::HalfAwayFromZero);
    }

    // --- window functions ---

    fn win_vm() -> Vm {
        // grp:  x x x | y y
        // v:    10 10 20 | 5 7      (a tie at v=10 within x)
        // k:    p q p | p q        (for the mixed-direction multi-key test)
        let df = df![
            "grp" => ["x", "x", "x", "y", "y"],
            "v"   => [10i64, 10, 20, 5, 7],
            "k"   => ["p", "q", "p", "p", "q"],
        ].unwrap();
        let mut vm = Vm::new();
        vm.tables.insert("t".into(), df);
        vm
    }

    #[test]
    fn window_over_broadcasts_a_partition_aggregate() {
        let df = run(win_vm(), "select m: max v over `grp from t");
        assert_eq!(i64s(&df, "m"), vec![20, 20, 20, 7, 7]);
    }

    #[test]
    fn window_rn_is_a_strict_ordinal_per_partition() {
        let df = run(win_vm(), "select r: rn over `grp order `v asc from t");
        // x: v=[10,10,20] -> 1,2,3 (ties keep row order); y: [5,7] -> 1,2
        assert_eq!(i64s(&df, "r"), vec![1, 2, 3, 1, 2]);
    }

    #[test]
    fn window_rank_and_drank_share_a_rank_on_ties() {
        let df = run(win_vm(),
            "select rk: rank over `grp order `v asc, dr: drank over `grp order `v asc from t");
        // x: v=[10,10,20] -> rank 1,1,3 / dense 1,1,2 ; y: [5,7] -> 1,2 / 1,2
        assert_eq!(i64s(&df, "rk"), vec![1, 1, 3, 1, 2]);
        assert_eq!(i64s(&df, "dr"), vec![1, 1, 2, 1, 2]);
    }

    #[test]
    fn window_rn_with_mixed_direction_multi_key_order() {
        let df = run(win_vm(), "select r: rn over `grp order `k asc `v desc from t");
        // x rows (k,v): (p,10)@0 (q,10)@1 (p,20)@2 -> order p:20,p:10,q:10 = [2,0,1]
        //   => row0=2, row1=3, row2=1 ; y: (p,5)@3 (q,7)@4 -> row3=1, row4=2
        assert_eq!(i64s(&df, "r"), vec![2, 3, 1, 1, 2]);
    }

    #[test]
    fn window_over_composes_inside_arithmetic() {
        let df = run(win_vm(), "select g: (max v over `grp) - v from t");
        assert_eq!(i64s(&df, "g"), vec![10, 10, 0, 2, 0]);
    }

    // --- error cases ---

    #[test]
    fn unknown_table_is_runtime_error() {
        let mut vm = make_vm();
        let prog = compile(&parse(tokenise("select c1 from nope").unwrap()).unwrap()).unwrap();
        assert!(matches!(vm.eval(prog), Err(QplError::Runtime(_))));
    }
}

