use crate::ast::{self, Value};
use crate::compiler::compile;
use crate::errors::QplError;
use crate::helpers::rename_columns_snake_case;
use crate::lexer::tokenise;
use crate::native::{Builtin, NativeCall};
use crate::ops;
use crate::parser::parse;
use crate::permission::Effect;
use crate::program::{Closure, Op, Operand, Program, WindowFn};
use crate::temporal;
use crate::vm_config::VmConfig;
#[cfg(not(target_family = "wasm"))]
use polars::io::utils::sync_on_close::SyncOnCloseType;
use polars::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

pub struct Vm {
    /// Every session binding: scalars, vectors, closures, `Table` (eager) and
    /// `Lazy` (plan) values.
    pub globals: HashMap<String, ast::Value>,
    /// Reserved natives (`.qpl.dt`, ...), built in [`Vm::new`]. Looked up like
    /// any name but can't be rebound. See [`crate::native`].
    pub(crate) builtins: HashMap<String, Builtin>,
    /// The value stack. Holds computation values and [`Slot::Call`] frames,
    /// interleaved.
    pub(crate) stack: Vec<Slot>,
    /// The executing `Program`. Switched by `CALL`/`RET`, saved and restored
    /// around a nested [`Vm::run_compiled`].
    pub(crate) prog: Arc<Program>,
    /// Instruction pointer into `prog.code`.
    pub(crate) ip: usize,
    /// Operand-stream cursor into `prog.operands`, consumed only by `PUSH`.
    pub(crate) cp: usize,
    /// Index of the innermost [`Slot::Call`] in `stack`, or `None` at top level.
    /// Only this frame's locals are searched (lexical scoping).
    pub(crate) fp: Option<usize>,
    /// Number of [`Slot::Call`] frames on `stack`, checked against
    /// [`MAX_CALL_DEPTH`].
    pub(crate) call_depth: usize,
    /// Where the first error escaping a top-level [`Vm::run_compiled`] happened:
    /// the executing `Program` (main script or an embedded `\l`/`\i` target)
    /// and the failing `ip`. Taken by [`Vm::take_error_site`] for the
    /// `path:line:` prefix.
    pub(crate) error_site: Option<(Arc<Program>, usize)>,
    /// Refuses every [`Effect::Write`] action unless built with
    /// [`Vm::new_writable`]. Fixed when the `Vm` is built and never changed.
    read_only: bool,
    /// Set by `\1 <path>`: [`Vm::emit`] also appends here.
    pub stdout_log: Option<std::fs::File>,
    /// When set, [`Vm::emit`] appends here instead of printing (the wasm REPL).
    pub capture: Option<String>,
    /// When set, a table result goes to [`Vm::last_table`] instead of being
    /// printed, so the wasm front-end gets data rather than text.
    #[cfg(feature = "wasm")]
    pub capture_table: bool,
    /// The table from the last statement run under [`Vm::capture_table`].
    #[cfg(feature = "wasm")]
    pub last_table: Option<DataFrame>,
    /// Session-wide knobs set from `.qpl.cfg key=value ...`.
    pub config: VmConfig,
    /// Ctrl-C flag (see [`crate::interrupt`]).
    pub interrupt: crate::interrupt::Interrupt,
    /// Open `hopen` connections, keyed by `Value::Handle` id.
    #[cfg(feature = "ipc")]
    pub connections: HashMap<i64, crate::ipc::ClientConn>,
    /// Pending `async dispatch` replies, keyed by `Value::Future` id.
    #[cfg(feature = "ipc")]
    pub pending: HashMap<i64, crate::ipc::ReplyRx>,
    /// Next handle/future id (one counter, so the two never collide).
    #[cfg(feature = "ipc")]
    pub next_handle: i64,
    /// The permission of the dispatched request being evaluated, set only
    /// inside `with_request_permission`. Never set for local input.
    #[cfg(feature = "ipc")]
    pub request_mode: Option<crate::ipc::HandleMode>,
    /// The open `\port` listener, set by `native_port`. The run loop checks it
    /// to decide whether to serve; dropping it stops the listener thread.
    #[cfg(feature = "ipc")]
    pub port: Option<crate::ipc::PortState>,
}

/// A user-function activation, pushed as [`Slot::Call`]. `locals` holds its
/// params and assignments; `ret_*` are the caller's registers for `Op::Ret`.
pub(crate) struct CallFrame {
    pub ret_prog: Arc<Program>,
    pub ret_ip: usize,
    pub ret_cp: usize,
    pub ret_fp: Option<usize>,
    pub locals: HashMap<String, ast::Value>,
}

/// The kind of binding [`Vm::lookup`] found for a name.
pub(crate) enum Lookup<'a> {
    Value(&'a ast::Value),
    Builtin(&'a Builtin),
}

/// Max user-function call nesting. Calls don't recurse in Rust (they push a
/// `Slot::Call` and jump), so this only guards against runaway recursion.
pub(crate) const MAX_CALL_DEPTH: usize = 128;

/// A stack slot. All of a statement's working state lives in these: a table
/// being built is `Frame`, a key/projection list is `List`, and `Noop` is an
/// explicit "nothing" result. `LazyFrame` is boxed to keep slots small.
pub(crate) enum Slot {
    Expr(Expr),
    /// A table being built. `lazy` is set by reading a `Value::Lazy` or
    /// `Op::Lazy`, and cleared by `Op::Collect` (and `cols`).
    Frame {
        lf: Box<LazyFrame>,
        lazy: bool,
    },
    Scalar(ast::Value),
    /// A projection/key/predicate list under construction (`Op::List`).
    List(Vec<Expr>),
    /// A value `Op::Push` placed for the next opcode (any `Operand` except
    /// `Value`, which becomes `Scalar`).
    Operand(Operand),
    /// A typed Polars column from `Op::CastList`, kept as-is so a narrow dtype
    /// survives into `Op::Zip`, its only consumer.
    Column(polars::prelude::Column),
    /// The result of `noop`, `while`, or anything else that yields nothing.
    /// Can't be stored; at top level it evaluates to `EvalResult::Stored`.
    Noop,
    /// A call activation ([`CallFrame`]). Addressed by index (`Vm::fp`), never
    /// an opcode input.
    Call(CallFrame),
}

impl Slot {
    pub(crate) fn unwrap_frame(self) -> Result<(LazyFrame, bool), QplError> {
        match self {
            Slot::Frame { lf, lazy } => Ok((*lf, lazy)),
            other => Err(QplError::Runtime(format!(
                "Expected Frame on stack, got {}",
                other.type_name()
            ))),
        }
    }
    pub(crate) fn unwrap_scalar(self) -> Result<ast::Value, QplError> {
        match self {
            Slot::Scalar(s) => Ok(s),
            other => Err(QplError::Runtime(format!(
                "Expected Scalar on stack, got {}",
                other.type_name()
            ))),
        }
    }
    pub(crate) fn unwrap_list(self) -> Result<Vec<Expr>, QplError> {
        match self {
            Slot::List(l) => Ok(l),
            other => Err(QplError::Runtime(format!(
                "Expected List on stack, got {}",
                other.type_name()
            ))),
        }
    }
    pub(crate) fn unwrap_operand(self) -> Result<Operand, QplError> {
        match self {
            Slot::Operand(o) => Ok(o),
            other => Err(QplError::Runtime(format!(
                "Expected an operand on stack, got {}",
                other.type_name()
            ))),
        }
    }

    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            Slot::Expr(_) => "Expr",
            Slot::Frame { .. } => "Frame",
            Slot::Scalar(_) => "Scalar",
            Slot::List(_) => "List",
            Slot::Operand(_) => "Operand",
            Slot::Column(_) => "Column",
            Slot::Noop => "Noop",
            Slot::Call(_) => "Call",
        }
    }
}

/// Pop a column expression: an `Expr` as-is, or a `Scalar` lifted with
/// [`ast_val_to_expr`] (`PUSH` doesn't know the context, so the consumer lifts).
fn pop_expr(stack: &mut Vec<Slot>) -> Result<Expr, QplError> {
    slot_to_expr(pop1(stack)?)
}

/// [`pop_expr`] for an already-popped slot.
fn slot_to_expr(slot: Slot) -> Result<Expr, QplError> {
    match slot {
        Slot::Expr(e) => Ok(e),
        Slot::Scalar(v) => ast_val_to_expr(v),
        other => Err(QplError::Runtime(format!(
            "Expected Expr on stack, got {}",
            other.type_name()
        ))),
    }
}

/// Like [`slot_to_expr`], with specific errors for a table or `Noop` operand.
fn expr_for_binop(slot: Slot) -> Result<Expr, QplError> {
    match slot {
        Slot::Expr(e) => Ok(e),
        Slot::Scalar(v) => ast_val_to_expr(v),
        Slot::Frame { .. } => Err(QplError::Runtime(
            "expected a scalar here, got a table".into(),
        )),
        Slot::Noop => Err(QplError::Runtime(
            "cannot use a no-op expression as a value".into(),
        )),
        other => Err(QplError::Runtime(format!(
            "Expected Expr on stack, got {}",
            other.type_name()
        ))),
    }
}

/// Pop the top of the stack, expecting an `Operand::Name`.
fn pop_name(stack: &mut Vec<Slot>) -> Result<std::sync::Arc<str>, QplError> {
    match pop1(stack)?.unwrap_operand()? {
        Operand::Name(n) => Ok(n),
        other => Err(QplError::Runtime(format!(
            "Expected a Name operand, got {other:?}"
        ))),
    }
}

/// Pop the top of the stack, expecting an `Operand::Count`.
fn pop_count(stack: &mut Vec<Slot>) -> Result<usize, QplError> {
    match pop1(stack)?.unwrap_operand()? {
        Operand::Count(n) => Ok(n as usize),
        other => Err(QplError::Runtime(format!(
            "Expected a Count operand, got {other:?}"
        ))),
    }
}

/// Pop the top of the stack, expecting an `Operand::Target`.
fn pop_target(stack: &mut Vec<Slot>) -> Result<(u32, u32), QplError> {
    match pop1(stack)?.unwrap_operand()? {
        Operand::Target { ip, cp } => Ok((ip, cp)),
        other => Err(QplError::Runtime(format!(
            "Expected a Target operand, got {other:?}"
        ))),
    }
}

/// The single `Str` argument of a statement native (`.qpl.cfg`, `\1`, `\d`).
/// A mismatch means corrupt bytecode, not a user error.
fn expect_one_str(args: &mut Vec<Slot>, what: &str) -> Result<String, QplError> {
    if args.len() != 1 {
        return Err(QplError::Runtime(format!(
            "corrupt bytecode: '{what}' expects exactly 1 argument, got {}",
            args.len()
        )));
    }
    match args.remove(0).unwrap_scalar()? {
        Value::Str(s) => Ok(s),
        other => Err(QplError::Runtime(format!(
            "corrupt bytecode: '{what}' expected a string argument, got {other:?}"
        ))),
    }
}

/// The single embedded-program argument of `\l`/`\i` (corrupt bytecode if not).
fn expect_one_program(args: &mut Vec<Slot>, what: &str) -> Result<Arc<Program>, QplError> {
    if args.is_empty() {
        return Err(QplError::Runtime(format!(
            "corrupt bytecode: '{what}' expects an embedded program argument"
        )));
    }
    match args.remove(0) {
        Slot::Operand(Operand::Program(p)) => Ok(p),
        other => Err(QplError::Runtime(format!(
            "corrupt bytecode: '{what}' expected an embedded program, got {}",
            other.type_name()
        ))),
    }
}

/// A call argument as a value: a lazy frame stays `Lazy`, any other frame is
/// collected to a `Table`.
fn slot_into_value(slot: Slot) -> Result<Value, QplError> {
    match slot {
        Slot::Scalar(s) => Ok(s),
        Slot::Frame { lf, lazy: true } => Ok(Value::Lazy(lf)),
        Slot::Frame { lf, lazy: false } => Ok(Value::Table(
            (*lf)
                .collect()
                .map_err(|e| QplError::Runtime(e.to_string()))?,
        )),
        Slot::Noop => Err(QplError::Runtime(
            "cannot use a no-op expression as a value".into(),
        )),
        other => Err(QplError::Runtime(format!(
            "Expected Expr on stack, got {}",
            other.type_name()
        ))),
    }
}

/// A returned value as a slot: tables become frames, keeping laziness.
fn value_into_slot(v: Value) -> Slot {
    match v {
        Value::Table(df) => Slot::Frame {
            lf: Box::new(df.lazy()),
            lazy: false,
        },
        Value::Lazy(lf) => Slot::Frame { lf, lazy: true },
        other => Slot::Scalar(other),
    }
}

/// The nearest `Frame` on the stack, without popping: the dtype probe for a
/// column-context cast (other lists may sit above it mid-select).
fn nearest_frame(stack: &[Slot]) -> Option<&LazyFrame> {
    stack.iter().rev().find_map(|s| match s {
        Slot::Frame { lf, .. } => Some(lf.as_ref()),
        _ => None,
    })
}

impl Default for Vm {
    fn default() -> Self {
        Self::new()
    }
}

impl Vm {
    /// A read-only session: it refuses every write action for its whole life.
    pub fn new() -> Self {
        Self {
            globals: HashMap::new(),
            builtins: crate::native::builtins(),
            stack: Vec::new(),
            prog: Arc::new(Program::default()),
            ip: 0,
            cp: 0,
            fp: None,
            call_depth: 0,
            error_site: None,
            read_only: true,
            stdout_log: None,
            capture: None,
            #[cfg(feature = "wasm")]
            capture_table: false,
            #[cfg(feature = "wasm")]
            last_table: None,
            config: VmConfig::default(),
            interrupt: crate::interrupt::Interrupt::default(),
            #[cfg(feature = "ipc")]
            connections: HashMap::new(),
            #[cfg(feature = "ipc")]
            pending: HashMap::new(),
            #[cfg(feature = "ipc")]
            next_handle: 0,
            #[cfg(feature = "ipc")]
            request_mode: None,
            #[cfg(feature = "ipc")]
            port: None,
        }
    }

    /// Run `f` with the request permission set to `mode`, then clear it. Wraps
    /// each dispatched command.
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

    /// A session that may also perform write actions (`qpl -w`).
    pub fn new_writable() -> Self {
        Self {
            read_only: false,
            ..Self::new()
        }
    }

    /// Whether this session refuses write actions.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Error if the session may not perform an action with `effect`: a write
    /// in a read-only session, or any change over a read-only IPC handle.
    pub(crate) fn authorize(&self, effect: Effect, what: &str) -> Result<(), QplError> {
        if effect == Effect::Write && self.read_only {
            return Err(QplError::Runtime(format!(
                "Cannot perform write action in read-only session: {what} (start qpl with -w to allow writes)"
            )));
        }
        #[cfg(feature = "ipc")]
        if effect != Effect::Read && self.request_mode == Some(crate::ipc::HandleMode::Read) {
            return Err(QplError::Runtime(format!(
                "{what} is not allowed over a read-only connection (open with `w!hopen` for a write handle)"
            )));
        }
        Ok(())
    }

    /// The innermost active call frame, if any.
    pub(crate) fn current_frame(&self) -> Option<&CallFrame> {
        self.fp.map(|i| match &self.stack[i] {
            Slot::Call(f) => f,
            other => unreachable!(
                "Vm::fp must always index a Slot::Call, got {}",
                other.type_name()
            ),
        })
    }

    /// Mutable counterpart of [`Vm::current_frame`].
    fn current_frame_mut(&mut self) -> Option<&mut CallFrame> {
        match self.fp {
            Some(i) => match &mut self.stack[i] {
                Slot::Call(f) => Some(f),
                _ => unreachable!("Vm::fp must always index a Slot::Call"),
            },
            None => None,
        }
    }

    /// Resolve `name`: reserved builtins, then the innermost call frame's
    /// locals, then globals. Never an enclosing caller's frame. Namespaces are
    /// resolved at compile time, so names are looked up exactly as written.
    pub(crate) fn lookup(&self, name: &str) -> Option<Lookup<'_>> {
        // builtins can't be rebound, so checking them first is unambiguous
        if let Some(b) = self.builtins.get(name) {
            return Some(Lookup::Builtin(b));
        }
        if let Some(v) = self.current_frame().and_then(|f| f.locals.get(name)) {
            return Some(Lookup::Value(v));
        }
        self.globals.get(name).map(Lookup::Value)
    }

    /// `lookup` excluding table values, which must never become literals in a
    /// column expression.
    pub(crate) fn lookup_global(&self, name: &str) -> Option<&ast::Value> {
        match self.lookup(name) {
            Some(Lookup::Value(v)) if !matches!(v, ast::Value::Table(_) | ast::Value::Lazy(_)) => {
                Some(v)
            }
            _ => None,
        }
    }

    /// Whether `name` is a function value or builtin.
    pub(crate) fn is_callable(&self, name: &str) -> bool {
        matches!(
            self.lookup(name),
            Some(Lookup::Builtin(_)) | Some(Lookup::Value(ast::Value::Closure(_)))
        )
    }

    /// Reject binding a builtin's name.
    fn check_not_builtin(&self, name: &str) -> Result<(), QplError> {
        if self.builtins.contains_key(name) {
            return Err(QplError::Runtime(format!(
                "'{name}' is a built-in and cannot be reassigned"
            )));
        }
        Ok(())
    }

    /// Bind `val` to `name` in the active call frame, or globals at top level,
    /// replacing whatever was there.
    pub(crate) fn bind(&mut self, name: String, val: ast::Value) -> Result<(), QplError> {
        self.check_not_builtin(&name)?;
        match self.current_frame_mut() {
            Some(frame) => {
                frame.locals.insert(name, val);
            }
            None => {
                self.globals.insert(name, val);
            }
        }
        Ok(())
    }

    /// Point the stdout log at `path` (appending), or detach it with `""`.
    pub fn set_stdout_log(&mut self, path: &str) -> Result<(), QplError> {
        if path.is_empty() {
            self.authorize(Effect::Session, "\\1 (stdout log)")?;
            self.stdout_log = None;
            return Ok(());
        }
        self.authorize(Effect::Write, "\\1 (stdout log)")?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| QplError::Runtime(format!("cannot open log file '{path}': {e}")))?;
        self.stdout_log = Some(file);
        Ok(())
    }

    /// Print `text` (or capture it), mirroring it to the stdout log if set.
    pub fn emit(&mut self, text: &str) {
        match self.capture.as_mut() {
            Some(buf) => {
                buf.push_str(text);
                buf.push('\n');
            }
            None => println!("{text}"),
        }
        if let Some(file) = self.stdout_log.as_mut() {
            use std::io::Write;
            let _ = writeln!(file, "{text}");
            let _ = file.flush();
        }
    }

    /// A `column`/`dtype` table describing `lf`'s schema.
    pub fn schema(&self, mut lf: LazyFrame) -> Result<DataFrame, QplError> {
        let schema = lf
            .collect_schema()
            .map_err(|e| QplError::Runtime(e.to_string()))?;
        let names = schema
            .iter_names_and_dtypes()
            .map(|(name, dtype)| (name.to_string(), dtype.to_string()))
            .collect::<Vec<(String, String)>>();
        let (names, types): (Vec<String>, Vec<String>) = names.into_iter().unzip();
        df!["column" => names, "dtype" => types].map_err(|e| QplError::Runtime(e.to_string()))
    }

    /// The Polars expr for a column-context cast. `frame` tells a string source
    /// from a typed one. Strings go through Polars' string parsers, which infer
    /// the format per value (ISO or kdb dotted): `` `date$ ``/`` `month$ `` yield
    /// a `Date`, `` `timestamp$ `` keeps the time, `` `time$ `` parses a time.
    /// Other sources (temporal columns, raw integer offsets) use `.cast()`.
    pub(crate) fn build_cast_expr(
        &self,
        target: &ast::CastTarget,
        expr: Expr,
        frame: Option<&LazyFrame>,
    ) -> Result<Expr, QplError> {
        let dtype = self.resolve_cast_target(target)?;
        let temporal_target = matches!(
            dtype,
            DataType::Date | DataType::Datetime(_, _) | DataType::Time
        );
        Ok(if temporal_target && expr_dtype_is_string(frame, &expr)? {
            let to_datetime = |e: Expr| {
                e.str()
                    .to_datetime(None, None, StrptimeOptions::default(), lit("raise"))
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

    /// A column-context cast target as a Polars `DataType`.
    fn resolve_cast_target(&self, target: &ast::CastTarget) -> Result<DataType, QplError> {
        match target {
            ast::CastTarget::Prim(name) => polars_dtype(name),
            // `` `$col ``: Categorical, u32 codes
            ast::CastTarget::Sym => Ok(DataType::from_categories(Categories::global())),
            // `` u8!`$col ``: Categorical with an explicit width (one global
            // pool per width)
            ast::CastTarget::SymPhysical(width) => {
                let phys = match width.as_str() {
                    "u8" => CategoricalPhysical::U8,
                    "u16" => CategoricalPhysical::U16,
                    "u32" => CategoricalPhysical::U32,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "categorical physical width must be u8/u16/u32, got '{other}'"
                        )));
                    }
                };
                Ok(DataType::from_categories(Categories::new(
                    "qpl".into(),
                    "".into(),
                    phys,
                )))
            }
            // `` name::`$col ``: an Enum with categories from symbol vector `name`
            ast::CastTarget::Enum(name) => {
                let cats = match self.lookup_global(name) {
                    Some(v @ Value::SymVec(_)) => v.vec_strings().map_err(QplError::Runtime)?,
                    Some(other) => {
                        return Err(QplError::Runtime(format!(
                            "'{name}' is not an enum (expected a symbol vector, got {other:?})"
                        )));
                    }
                    None => return Err(QplError::Runtime(format!("undefined enum '{name}'"))),
                };
                let fcats = FrozenCategories::new(cats.iter().map(String::as_str))
                    .map_err(|e| QplError::Runtime(format!("invalid enum '{name}': {e}")))?;
                Ok(DataType::from_frozen_categories(fcats))
            }
        }
    }

    /// Run a program and reduce its result to an [`EvalResult`].
    pub fn eval(&mut self, program: Program) -> Result<EvalResult, QplError> {
        self.interrupt.check()?;
        let result = self.run_compiled(Arc::new(program))?;
        self.interrupt.check()?;
        match result {
            Slot::Frame { lf, lazy } => {
                if lazy {
                    return Ok(EvalResult::Lazy(explain_plan(&lf)));
                }
                let df = lf.collect().map_err(|e| QplError::Runtime(e.to_string()))?;
                self.interrupt.check()?;
                Ok(EvalResult::Table(df))
            }
            Slot::Scalar(s) => Ok(EvalResult::Scalar(s)),
            Slot::Noop => Ok(EvalResult::Stored),
            other => Err(QplError::Runtime(format!(
                "Unexpected type on stack: {}",
                other.type_name()
            ))),
        }
    }

    /// Run `program` on the shared stack above whatever is there, saving and
    /// restoring `prog`/`ip`/`cp` so it nests inside an active call. `fp` is
    /// kept on success (so the sub-program sees the caller's locals); on error
    /// the stack and `fp` roll back so no half-finished frame is left.
    pub(crate) fn run_compiled(&mut self, program: Arc<Program>) -> Result<Slot, QplError> {
        let saved_prog = self.prog.clone();
        let saved_ip = self.ip;
        let saved_cp = self.cp;
        let saved_fp = self.fp;
        let base = self.stack.len();
        self.prog = program;
        self.ip = 0;
        self.cp = 0;
        let outcome = self.run_loop();
        self.prog = saved_prog;
        self.ip = saved_ip;
        self.cp = saved_cp;
        match outcome {
            Ok(()) => {
                let result = match self.stack.len().checked_sub(base) {
                    Some(0) => Slot::Noop,
                    Some(1) => self.stack.pop().expect("checked len above"),
                    _ => {
                        return Err(QplError::Runtime(format!(
                            "corrupt bytecode: program left {} value(s) on the stack, expected 0 or 1",
                            self.stack.len() - base
                        )));
                    }
                };
                Ok(result)
            }
            Err(e) => {
                self.stack.truncate(base);
                self.fp = saved_fp;
                Err(e)
            }
        }
    }

    /// Runs `self.prog` from `self.ip` until it's exhausted. `CALL`/`RET`
    /// switch `self.prog`/`self.ip`/`self.cp` mid-loop (into a closure's own
    /// program and back) — the loop only ever cares about the *current*
    /// `self.prog`'s length, so it transparently keeps going once control
    /// returns to whichever program called it.
    ///
    /// On the *first* opcode to fail, records `(self.prog, that opcode's own
    /// `ip`)` into [`Vm::error_site`] — before
    /// `run_compiled` restores any register — so the top-level driver
    /// (`repl::run_script`) can map it back to a `path:line:` prefix via
    /// whichever `Program::lines` was actually executing: the main script's
    /// own table for an ordinary failure, or an embedded `\l`/`\i` target's
    /// own table if the failure happened while that sub-program (or a
    /// namespaced import's function, which carries its *own* `Arc<Program>`)
    /// was running. A `run_loop` nested inside this one (e.g. the native
    /// behind `\l`/`\i`) always reaches its own failure first, so the
    /// `is_none()` guard here keeps that innermost, most specific site.
    fn run_loop(&mut self) -> Result<(), QplError> {
        while self.ip < self.prog.code.len() {
            let op_ip = self.ip;
            if let Err(e) = self.step() {
                if self.error_site.is_none() {
                    self.error_site = Some((self.prog.clone(), op_ip));
                }
                return Err(e);
            }
        }
        Ok(())
    }

    /// Take and clear the recorded error site. Called once per top-level run so
    /// a stale site never reaches a later error.
    pub(crate) fn take_error_site(&mut self) -> Option<(Arc<Program>, usize)> {
        self.error_site.take()
    }

    /// Execute the opcode at `ip`. Only `Push` reads the operand stream. An
    /// opcode handler can call this in a small loop to wait out a nested `CALL`
    /// without Rust recursion (see `Op::LoadCol`).
    fn step(&mut self) -> Result<(), QplError> {
        let byte = self.prog.code[self.ip];
        self.ip += 1;
        let op = Op::try_from(byte)?;
        match op {
            Op::Push => {
                let operand = self.prog.operands.get(self.cp).cloned().ok_or_else(|| {
                    QplError::Runtime("corrupt bytecode: operand stream underflow".into())
                })?;
                self.cp += 1;
                match operand {
                    Operand::Value(v) => self.stack.push(Slot::Scalar(v)),
                    // attach the running program so the closure outlives it
                    Operand::Func(proto) => {
                        let closure = Closure {
                            params: proto.params.clone(),
                            entry: proto.entry,
                            program: self.prog.clone(),
                            display: proto.display.clone(),
                        };
                        self.stack
                            .push(Slot::Scalar(Value::Closure(Arc::new(closure))));
                    }
                    other => self.stack.push(Slot::Operand(other)),
                }
            }

            Op::Source => {
                let name = pop_name(&mut self.stack)?;
                let (lf, lazy) = match self.lookup(&name) {
                    // reading a lazy binding keeps the result lazy
                    Some(Lookup::Value(Value::Lazy(lf))) => ((**lf).clone(), true),
                    Some(Lookup::Value(Value::Table(df))) => (df.clone().lazy(), false),
                    _ => {
                        return Err(QplError::Runtime(format!("unknown table '{name}'")));
                    }
                };
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf),
                    lazy,
                });
            }

            Op::LoadFile => {
                let path_str = match pop1(&mut self.stack)? {
                    Slot::Scalar(ast::Value::Str(s) | ast::Value::Sym(s)) => s,
                    Slot::Scalar(other) => {
                        return Err(QplError::Runtime(format!(
                            "expected a string path for load, got {other:?}"
                        )));
                    }
                    other => {
                        return Err(QplError::Runtime(format!(
                            "expected a string path for load, got {}",
                            other.type_name()
                        )));
                    }
                };
                let lf = rename_columns_snake_case(load_file(&path_str)?)?;
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf),
                    lazy: false,
                });
            }

            Op::RowIndex => {
                let (lf, lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf.with_row_index("i", None)),
                    lazy,
                });
            }

            // A niladic function (e.g. `.qpl.dt`) is called and its result
            // used as a literal; checked before globals because a user
            // function is itself a global. Unlike `Load`/`Call`, the call runs
            // to completion here (a nested `step()` loop, not Rust recursion),
            // since `LOAD_COL` needs the result's type for its error text.
            Op::LoadCol => {
                let name = pop_name(&mut self.stack)?;
                match self.niladic_closure_or_builtin(&name) {
                    Niladic::Builtin(b) => {
                        let v = match self.call_builtin(&name, b, vec![])? {
                            Slot::Scalar(v) => v,
                            other => {
                                return Err(QplError::Runtime(format!(
                                    "Expected Expr on stack, got {}",
                                    other.type_name()
                                )));
                            }
                        };
                        self.stack.push(Slot::Expr(ast_val_to_expr(v)?));
                    }
                    Niladic::Closure(c) => {
                        let target_fp = self.stack.len();
                        self.begin_closure_call(c, vec![], Some(name.to_string()))?;
                        while self.fp.is_some_and(|f| f >= target_fp) {
                            self.step()?;
                        }
                        match pop1(&mut self.stack)? {
                            Slot::Scalar(v) => self.stack.push(Slot::Expr(ast_val_to_expr(v)?)),
                            Slot::Frame { .. } => {
                                return Err(QplError::Runtime(format!(
                                    "'{name}' returns a table — it can't be used inside a column expression"
                                )));
                            }
                            Slot::Noop => {
                                return Err(QplError::Runtime(format!(
                                    "'{name}' returns nothing — it can't be used inside a column expression"
                                )));
                            }
                            other => {
                                return Err(QplError::Runtime(format!(
                                    "Expected Expr on stack, got {}",
                                    other.type_name()
                                )));
                            }
                        }
                    }
                    // a global shadows a column name, substituting a literal
                    Niladic::None => {
                        if let Some(val) = self.lookup_global(&name) {
                            self.stack.push(Slot::Expr(ast_val_to_expr(val.clone())?));
                        } else {
                            self.stack.push(Slot::Expr(col(name.as_ref())));
                        }
                    }
                }
            }

            Op::LoadRowIdx => {
                self.stack.push(Slot::Expr(col("i")));
            }

            // Value context: resolve a bare name. A niladic function is called
            // (via `CALL`/`RET`), a table global becomes a `Frame`, anything
            // else a `Scalar`.
            Op::Load => {
                let name = pop_name(&mut self.stack)?;
                match self.niladic_closure_or_builtin(&name) {
                    Niladic::Builtin(b) => {
                        let slot = self.call_builtin(&name, b, vec![])?;
                        self.stack.push(slot);
                    }
                    Niladic::Closure(c) => {
                        self.begin_closure_call(c, vec![], Some(name.to_string()))?;
                    }
                    Niladic::None => match self.resolve_plain(&name) {
                        Resolved::Lazy(lf) => self.stack.push(Slot::Frame { lf, lazy: true }),
                        Resolved::Table(df) => self.stack.push(Slot::Frame {
                            lf: Box::new(df.lazy()),
                            lazy: false,
                        }),
                        Resolved::Scalar(v) => self.stack.push(Slot::Scalar(v)),
                        Resolved::BuiltinNonNiladic => {
                            return Err(QplError::Runtime(format!(
                                "'{name}' is a built-in function — call it with '{name}[..]'"
                            )));
                        }
                        Resolved::Undefined => {
                            return Err(QplError::Runtime(format!(
                                "undefined name '{name}' (not a variable, table or lazy frame)"
                            )));
                        }
                    },
                }
            }

            // Value context: like `Load`, but a callable name isn't called; it's
            // pushed back as a name for `Index`/`Call` to resolve.
            Op::LoadFn => {
                let name = pop_name(&mut self.stack)?;
                if self.is_callable(&name) {
                    self.stack.push(Slot::Operand(Operand::Name(name)));
                } else {
                    match self.resolve_plain(&name) {
                        Resolved::Lazy(lf) => self.stack.push(Slot::Frame { lf, lazy: true }),
                        Resolved::Table(df) => self.stack.push(Slot::Frame {
                            lf: Box::new(df.lazy()),
                            lazy: false,
                        }),
                        Resolved::Scalar(v) => self.stack.push(Slot::Scalar(v)),
                        Resolved::BuiltinNonNiladic => {
                            return Err(QplError::Runtime(format!(
                                "'{name}' is a built-in function — call it with '{name}[..]'"
                            )));
                        }
                        Resolved::Undefined => {
                            return Err(QplError::Runtime(format!(
                                "undefined name '{name}' (not a variable, table or lazy frame)"
                            )));
                        }
                    }
                }
            }

            // Value context: a native by id, a name (see `ops::call_by_name`), or
            // a closure on the stack. A closure call jumps into the callee;
            // `Op::Ret` pushes the result.
            Op::Call => {
                let callee = pop1(&mut self.stack)?;
                let n = pop_count(&mut self.stack)?;
                let args = popn(&mut self.stack, n)?;
                self.call_value(callee, args)?;
            }

            // Value context: `n#expr`
            Op::Take => {
                let target = pop1(&mut self.stack)?;
                let n = match pop1(&mut self.stack)?.unwrap_scalar()? {
                    Value::Int(n) => n,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "take count must be an int, got {other:?}"
                        )));
                    }
                };
                let result = match target {
                    Slot::Frame { lf, lazy } => {
                        let lf = if n >= 0 {
                            lf.limit(n as IdxSize)
                        } else {
                            let k = -n;
                            lf.slice(-k, k as IdxSize)
                        };
                        Slot::Frame {
                            lf: Box::new(lf),
                            lazy,
                        }
                    }
                    other => Slot::Scalar(ops::take_list(ops::slot_to_scalar_value(other)?, n)?),
                };
                self.stack.push(result);
            }

            // Value context: call a callable target, otherwise index a list
            // (an atom picks an element, an int vector a sub-list).
            Op::Index => {
                let n = pop_count(&mut self.stack)?;
                let idx_args = popn(&mut self.stack, n)?;
                let target = pop1(&mut self.stack)?;
                match target {
                    Slot::Operand(Operand::Name(_)) | Slot::Scalar(Value::Closure(_)) => {
                        self.call_value(target, idx_args)?;
                    }
                    other => {
                        let list = ops::slot_to_list_value(other)?;
                        let idx = idx_args
                            .into_iter()
                            .next()
                            .ok_or_else(|| QplError::Runtime("index needs an argument".into()))?;
                        let (idxs, atom) = match idx.unwrap_scalar()? {
                            Value::Int(n) => (vec![n], true),
                            v @ Value::IntVec(_) => (
                                v.as_vec()
                                    .unwrap()
                                    .1
                                    .i64()
                                    .map_err(|e| QplError::Runtime(e.to_string()))?
                                    .into_no_null_iter()
                                    .collect(),
                                false,
                            ),
                            other => {
                                return Err(QplError::Runtime(format!(
                                    "index must be an int or int vector, got {other:?}"
                                )));
                            }
                        };
                        let picked = ops::index_list(list, &idxs)?;
                        self.stack.push(Slot::Scalar(if atom {
                            ops::scalarise(picked)?
                        } else {
                            picked
                        }));
                    }
                }
            }

            // Value context: a one-column select as a list
            Op::Column => {
                let (lf, _lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                let df = lf.collect().map_err(|e| QplError::Runtime(e.to_string()))?;
                self.interrupt.check()?;
                let col = df
                    .select_at_idx(0)
                    .ok_or_else(|| QplError::Runtime("empty column expression".into()))?;
                self.stack.push(Slot::Scalar(ops::column_to_value(col)?));
            }

            // `` zip `k1`k2!v1 v2 ``: each column is a `Column` (from `CastList`)
            // or a list value. The compiler always lowers a dict argument this
            // way, so a user function named `zip` is caught here at run time
            // and reported as an error.
            Op::Zip if self.is_callable("zip") => {
                return Err(QplError::Runtime(
                        "'zip' is shadowed by a user-defined function here — it cannot be called with a dict literal (a dict has no plain-value form to pass); call it as 'zip[<value>]' instead".into(),
                    ));
            }
            Op::Zip => {
                let n = pop_count(&mut self.stack)?;
                let names = match pop1(&mut self.stack)?.unwrap_operand()? {
                    Operand::Names(ns) => ns,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected a Names operand, got {other:?}"
                        )));
                    }
                };
                let cols = popn(&mut self.stack, n)?;
                let mut len: Option<usize> = None;
                let mut series_cols = Vec::with_capacity(n);
                for (name, slot) in names.iter().zip(cols) {
                    let series = match slot {
                        Slot::Column(c) => c.as_materialized_series().clone(),
                        Slot::Scalar(v) => ops::zip_value_to_series(v, name)?,
                        Slot::Frame { .. } => {
                            return Err(QplError::Runtime(
                                "expected a scalar here, got a table".into(),
                            ));
                        }
                        Slot::Noop => {
                            return Err(QplError::Runtime(
                                "cannot use a no-op expression as a value".into(),
                            ));
                        }
                        other => {
                            return Err(QplError::Runtime(format!(
                                "Expected Expr on stack, got {}",
                                other.type_name()
                            )));
                        }
                    };
                    match len {
                        None => len = Some(series.len()),
                        Some(l) if l != series.len() => {
                            return Err(QplError::Runtime(format!(
                                "'zip' columns have mismatched lengths: '{name}' has {}, expected {l}",
                                series.len()
                            )));
                        }
                        _ => {}
                    }
                    let mut series = series;
                    series.rename(name.as_str().into());
                    series_cols.push(Column::from(series));
                }
                let df =
                    DataFrame::new(len.expect("checked non-empty at compile time"), series_cols)
                        .map_err(|e| QplError::Runtime(e.to_string()))?;
                self.stack.push(Slot::Frame {
                    lf: Box::new(df.lazy()),
                    lazy: false,
                });
            }

            // cast a `zip` column's list, keeping the exact Series (dtype and
            // width) rather than going through `ast::Value`
            Op::CastList => {
                let name = pop_name(&mut self.stack)?;
                let target = match pop1(&mut self.stack)?.unwrap_operand()? {
                    Operand::Cast(c) => c,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected a Cast operand, got {other:?}"
                        )));
                    }
                };
                let list_val = ops::slot_to_list_value(pop1(&mut self.stack)?)?;
                if !ops::is_list_value(&list_val) {
                    return Err(QplError::Runtime(format!(
                        "'zip' column '{name}' is not a list: {list_val:?}"
                    )));
                }
                let lf = ops::list_to_lazy(list_val)?;
                let casted = self.build_cast_expr(&target, col("x"), Some(&lf))?;
                let df = lf
                    .select([casted.alias("x")])
                    .collect()
                    .map_err(|e| QplError::Runtime(e.to_string()))?;
                let column = df
                    .column("x")
                    .map_err(|e| QplError::Runtime(e.to_string()))?
                    .clone();
                self.stack.push(Slot::Column(column));
            }

            // the list half of `<list> where <pred>`: check it's a list and
            // expose it as a one-column `x` frame for the predicates that follow
            Op::ListWhereFrame => {
                let list_val = ops::slot_to_scalar_value(pop1(&mut self.stack)?)?;
                if !ops::is_list_value(&list_val) {
                    return Err(QplError::Runtime(format!(
                        "'where' needs a list on the left, got {list_val:?}"
                    )));
                }
                let lf = ops::list_to_lazy(list_val)?;
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf),
                    lazy: false,
                });
            }

            // `<conn> [async] dispatch <cmd>`
            Op::Dispatch => {
                let is_async = match pop1(&mut self.stack)?.unwrap_scalar()? {
                    Value::Bool(b) => b,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "expected a boolean here, got {other:?}"
                        )));
                    }
                };
                let command = match pop1(&mut self.stack)?.unwrap_operand()? {
                    Operand::Text(t) => t,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected a Text operand, got {other:?}"
                        )));
                    }
                };
                let conn = pop1(&mut self.stack)?.unwrap_scalar()?;
                #[cfg(feature = "ipc")]
                {
                    let result = ops::native_dispatch(self, conn, &command, is_async)?;
                    self.stack.push(result);
                }
                #[cfg(not(feature = "ipc"))]
                {
                    let _ = (conn, command, is_async);
                    return Err(QplError::Runtime(
                            "dispatch requires the `ipc` feature (on by default; this build used `--no-default-features`)".into(),
                        ));
                }
            }

            // two scalars evaluate eagerly (`ops::value_binop`); otherwise build
            // a Polars expression. A table or `Noop` operand is an error.
            Op::BinOp => {
                let kind = match pop1(&mut self.stack)?.unwrap_operand()? {
                    Operand::BinOp(k) => k,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected a BinOp operand, got {other:?}"
                        )));
                    }
                };
                let right = pop1(&mut self.stack)?;
                let left = pop1(&mut self.stack)?;
                let result = match (left, right) {
                    (Slot::Scalar(l), Slot::Scalar(r)) => {
                        Slot::Scalar(ops::value_binop(l, r, &kind)?)
                    }
                    (left, right) => {
                        let left = expr_for_binop(left)?;
                        let right = expr_for_binop(right)?;
                        Slot::Expr(ops::apply_binop(left, right, &kind)?)
                    }
                };
                self.stack.push(result);
            }

            Op::Verb => {
                // TODO support calls on lazyframes
                let verb = match pop1(&mut self.stack)?.unwrap_operand()? {
                    Operand::Verb(v) => v,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected a Verb operand, got {other:?}"
                        )));
                    }
                };
                let n = pop_count(&mut self.stack)?;
                let args = popn(&mut self.stack, n)?
                    .into_iter()
                    .map(slot_to_expr)
                    .collect::<Result<Vec<_>, _>>()?;
                self.stack.push(Slot::Expr(apply_call(&verb, args)?));
            }

            Op::Round => {
                let decimals = pop_count(&mut self.stack)? as u32;
                let expr = pop_expr(&mut self.stack)?;
                self.stack
                    .push(Slot::Expr(expr.round(decimals, self.config.round_type)));
            }

            Op::Window => {
                let spec = match pop1(&mut self.stack)?.unwrap_operand()? {
                    Operand::Window(w) => w,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected a Window operand, got {other:?}"
                        )));
                    }
                };
                if let Some((agg, window)) = &spec.rolling {
                    let column = pop_expr(&mut self.stack)?;
                    self.stack.push(Slot::Expr(build_rolling_window(
                        agg,
                        *window,
                        column,
                        &spec.partition,
                        &spec.order,
                    )?));
                } else {
                    let target = match spec.func {
                        WindowFn::Over => Some(pop_expr(&mut self.stack)?),
                        _ => None,
                    };
                    self.stack.push(Slot::Expr(build_window(
                        spec.func,
                        target,
                        &spec.partition,
                        &spec.order,
                    )?));
                }
            }

            Op::Case => {
                let branches = pop_count(&mut self.stack)?;
                let n = branches * 2 + 1;
                let mut values = Vec::with_capacity(n);
                for slot in popn(&mut self.stack, n)? {
                    values.push(match slot {
                        Slot::Expr(e) => e,
                        Slot::Scalar(v) => ast_val_to_expr(v)?,
                        other => {
                            return Err(QplError::Runtime(format!(
                                "Expected Expr on stack, got {}",
                                other.type_name()
                            )));
                        }
                    });
                }
                let default = values
                    .last()
                    .cloned()
                    .ok_or_else(|| QplError::Runtime("case expression has no default".into()))?;
                let mut case_expr = default;
                for pair in values[..values.len() - 1].as_chunks::<2>().0.iter().rev() {
                    case_expr = when(pair[0].clone())
                        .then(pair[1].clone())
                        .otherwise(case_expr);
                }
                self.stack.push(Slot::Expr(case_expr));
            }

            Op::Alias => {
                let name = pop_name(&mut self.stack)?;
                let expr = pop_expr(&mut self.stack)?;
                self.stack.push(Slot::Expr(expr.alias(name.as_ref())));
            }

            Op::Filter => {
                let n = pop_count(&mut self.stack)?;
                let preds = popn(&mut self.stack, n)?
                    .into_iter()
                    .map(slot_to_expr)
                    .collect::<Result<Vec<_>, _>>()?;
                let (mut lf, lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                for pred in preds {
                    lf = lf.filter(pred);
                }
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf),
                    lazy,
                });
            }

            Op::Sort => {
                let sort_map = match pop1(&mut self.stack)?.unwrap_operand()? {
                    Operand::Sort(s) => s,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected a Sort operand, got {other:?}"
                        )));
                    }
                };
                let cols_v = sort_map
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>();
                let ascs = sort_map
                    .iter()
                    .map(|(_, descending)| *descending)
                    .collect::<Vec<_>>();
                let (lf, lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                let sorted_lf = lf.sort_by_exprs(
                    cols_v
                        .iter()
                        .map(|c| col(c.as_str()))
                        .collect::<Vec<_>>()
                        .as_slice(),
                    SortMultipleOptions::new().with_order_descending_multi(ascs),
                );
                self.stack.push(Slot::Frame {
                    lf: Box::new(sorted_lf),
                    lazy,
                });
            }

            Op::Distinct => {
                let (lf, lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf.unique_stable(None, UniqueKeepStrategy::First)),
                    lazy,
                });
            }

            Op::DropNull => {
                let columns = match pop1(&mut self.stack)?.unwrap_operand()? {
                    Operand::Names(n) => n,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected a Names operand, got {other:?}"
                        )));
                    }
                };
                let (lf, lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf.drop_nulls(Some(cols(columns.to_vec())))),
                    lazy,
                });
            }

            Op::Limit => {
                let limit = match pop1(&mut self.stack)?.unwrap_scalar()? {
                    Value::Int(n) => n,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "limit/take count must be an int, got {other:?}"
                        )));
                    }
                };
                let (lf, lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                let lf = if limit < 0 {
                    lf.tail(limit.unsigned_abs() as IdxSize)
                } else {
                    lf.limit(limit as IdxSize)
                };
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf),
                    lazy,
                });
            }

            Op::Drop => {
                let columns = match pop1(&mut self.stack)?.unwrap_operand()? {
                    Operand::Names(n) => n,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected a Names operand, got {other:?}"
                        )));
                    }
                };
                let (lf, lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf.drop(cols(columns.to_vec()))),
                    lazy,
                });
            }

            Op::Cols => {
                let (lf, _lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                let df = self.schema(lf)?;
                // `cols` resolves the schema, so show it as a table
                self.stack.push(Slot::Frame {
                    lf: Box::new(df.lazy()),
                    lazy: false,
                });
            }

            Op::Sink => {
                self.authorize(Effect::Write, "sink")?;
                let path = pop1(&mut self.stack)?.unwrap_scalar()?;
                let path_str = match path {
                    Value::Str(s) => s,
                    _ => {
                        return Err(QplError::Runtime(format!(
                            "expected a string path for sink, got {path:?}"
                        )));
                    }
                };
                let (lf, _lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                sink_file(lf, &path_str)?
            }

            Op::Lazy => {
                let (lf, _) = pop1(&mut self.stack)?.unwrap_frame()?;
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf),
                    lazy: true,
                });
            }

            Op::Collect => {
                let (lf, _) = pop1(&mut self.stack)?.unwrap_frame()?;
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf),
                    lazy: false,
                });
            }

            Op::Join => {
                let join_type = match pop1(&mut self.stack)?.unwrap_operand()? {
                    Operand::Join(k) => k,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected a Join operand, got {other:?}"
                        )));
                    }
                };
                let (right, right_lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                let right_on = pop1(&mut self.stack)?.unwrap_list()?;
                let left_on = pop1(&mut self.stack)?.unwrap_list()?;
                let (left, left_lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                let joined = left.join(right, left_on, right_on, JoinArgs::new(join_type));
                self.stack.push(Slot::Frame {
                    lf: Box::new(joined),
                    lazy: left_lazy || right_lazy,
                });
            }

            Op::List => {
                let n = pop_count(&mut self.stack)?;
                let exprs = popn(&mut self.stack, n)?
                    .into_iter()
                    .map(slot_to_expr)
                    .collect::<Result<Vec<_>, _>>()?;
                self.stack.push(Slot::List(exprs));
            }

            Op::Select => {
                let proj = pop1(&mut self.stack)?.unwrap_list()?;
                let (lf, lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                let lf = if proj.is_empty() {
                    lf // empty projection = all columns
                } else {
                    lf.select(proj)
                };
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf),
                    lazy,
                });
            }

            // group_by().agg(): keys become keyed columns, aggs are the projection
            Op::SelectBy => {
                let aggs = pop1(&mut self.stack)?.unwrap_list()?;
                let keys = pop1(&mut self.stack)?.unwrap_list()?;
                let (lf, lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf.group_by(keys).agg(aggs)),
                    lazy,
                });
            }

            Op::Update => {
                let count = pop_count(&mut self.stack)?;
                let predicates = pop_count(&mut self.stack)?;
                let names = match pop1(&mut self.stack)?.unwrap_operand()? {
                    Operand::Names(n) => n,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected a Names operand, got {other:?}"
                        )));
                    }
                };
                let expressions = popn(&mut self.stack, count)?
                    .into_iter()
                    .map(slot_to_expr)
                    .collect::<Result<Vec<_>, _>>()?;
                let keys = pop1(&mut self.stack)?.unwrap_list()?;
                let preds = popn(&mut self.stack, predicates)?
                    .into_iter()
                    .map(slot_to_expr)
                    .collect::<Result<Vec<_>, _>>()?;
                let (mut lf, lazy) = pop1(&mut self.stack)?.unwrap_frame()?;
                let predicate = preds.into_iter().reduce(|left, right| left.and(right));
                // `update col: … where p` keeps the old value where `p` is false;
                // a new column has no old value, so it gets null
                let schema = match &predicate {
                    Some(_) => Some(
                        lf.collect_schema()
                            .map_err(|e| QplError::Runtime(e.to_string()))?,
                    ),
                    None => None,
                };
                let mut proj = expressions
                    .into_iter()
                    .zip(names.iter())
                    .map(|(expr, name)| {
                        let expr = if keys.is_empty() {
                            expr
                        } else {
                            expr.over(keys.clone())
                                .map_err(|e| QplError::Runtime(e.to_string()))?
                        };
                        Ok(match &predicate {
                            Some(predicate) => {
                                let old =
                                    if schema.as_ref().is_none_or(|s| s.contains(name.as_str())) {
                                        col(name.as_str())
                                    } else {
                                        lit(NULL)
                                    };
                                when(predicate.clone())
                                    .then(expr)
                                    .otherwise(old)
                                    .alias(name.as_str())
                            }
                            None => expr,
                        })
                    })
                    .collect::<Result<Vec<_>, QplError>>()?;
                let all_except = (all() - by_name(names.iter().cloned(), false, false)).as_expr();
                proj.insert(0, all_except);
                self.stack.push(Slot::Frame {
                    lf: Box::new(lf.select(proj)),
                    lazy,
                });
            }

            // Type-dispatched: an `Expr`
            // operand builds a Polars cast expression (query context);
            // a `Scalar` atom folds eagerly in Rust
            // (`ops::scalar_cast_target`); a `Scalar` list or a `Frame` operand
            // materialises through the same one-column-select path.
            Op::Cast => {
                let target = match pop1(&mut self.stack)?.unwrap_operand()? {
                    Operand::Cast(c) => c,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected a Cast operand, got {other:?}"
                        )));
                    }
                };
                let operand = pop1(&mut self.stack)?;
                let result = match operand {
                    Slot::Expr(expr) => {
                        let probe = nearest_frame(&self.stack);
                        Slot::Expr(self.build_cast_expr(&target, expr, probe)?)
                    }
                    Slot::Scalar(v) if ops::is_list_value(&v) => {
                        let lf = ops::list_to_lazy(v)?;
                        let casted = self.build_cast_expr(&target, col("x"), Some(&lf))?;
                        let df = lf
                            .select([casted.alias("r")])
                            .collect()
                            .map_err(|e| QplError::Runtime(e.to_string()))?;
                        let result_col = df
                            .column("r")
                            .map_err(|e| QplError::Runtime(e.to_string()))?;
                        Slot::Scalar(ops::column_to_value(result_col)?)
                    }
                    Slot::Scalar(v) => {
                        Slot::Scalar(ops::scalar_cast_target(v, &target, self.config.useqepoch)?)
                    }
                    Slot::Frame { lf, .. } => {
                        let lf = *lf;
                        let name = ops::first_col_name(&lf)?;
                        let casted =
                            self.build_cast_expr(&target, col(name.as_str()), Some(&lf))?;
                        let df = lf
                            .select([casted.alias("r")])
                            .collect()
                            .map_err(|e| QplError::Runtime(e.to_string()))?;
                        let result_col = df
                            .column("r")
                            .map_err(|e| QplError::Runtime(e.to_string()))?;
                        Slot::Scalar(ops::column_to_value(result_col)?)
                    }
                    Slot::Noop => {
                        return Err(QplError::Runtime(
                            "cannot use a no-op expression as a value".into(),
                        ));
                    }
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected Expr on stack, got {}",
                            other.type_name()
                        )));
                    }
                };
                self.stack.push(result);
            }

            Op::Store => {
                self.authorize(Effect::Session, "assignment")?;
                let name = pop_name(&mut self.stack)?;
                let value = self
                    .stack
                    .pop()
                    .ok_or_else(|| QplError::Runtime("cannot assign a no-op expression.".into()))?;
                match value {
                    Slot::Scalar(s) => {
                        self.bind(name.to_string(), s)?;
                    }
                    Slot::Frame { lf, lazy } => {
                        if lazy {
                            // keep the plan lazy under this name
                            self.bind(name.to_string(), Value::Lazy(lf))?;
                        } else {
                            let df = lf.collect().map_err(|e| QplError::Runtime(e.to_string()))?;
                            self.bind(name.to_string(), Value::Table(df))?;
                        }
                    }
                    Slot::Noop => {
                        return Err(QplError::Runtime(
                            "cannot assign a no-op expression.".into(),
                        ));
                    }
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Cannot assign '{}' to type {}: expected a table or scalar on stack",
                            name,
                            other.type_name()
                        )));
                    }
                }
            }

            // `(x → )`
            Op::Pop => {
                pop1(&mut self.stack)?;
            }

            // `(x → )`: print a top-level result. A lazy frame prints its plan,
            // an eager one its table (or goes to `last_table` under
            // `capture_table`), a scalar via `fmt_val`, `Noop` nothing.
            Op::Emit => match pop1(&mut self.stack)? {
                Slot::Frame { lf, lazy } => {
                    if lazy {
                        let text = explain_plan(&lf);
                        self.emit(&text);
                    } else {
                        let df = lf.collect().map_err(|e| QplError::Runtime(e.to_string()))?;
                        self.interrupt.check()?;
                        #[cfg(feature = "wasm")]
                        if self.capture_table {
                            self.last_table = Some(df);
                            return Ok(());
                        }
                        let text = df.to_string();
                        self.emit(&text);
                    }
                }
                Slot::Scalar(v) => {
                    let text = crate::repl::fmt_val(&v);
                    self.emit(&text);
                }
                Slot::Noop => {}
                other => {
                    return Err(QplError::Runtime(format!(
                        "Unexpected type on stack: {}",
                        other.type_name()
                    )));
                }
            },

            // `(result → result)`: unwind to `fp`, restore the caller's
            // registers, leave `result` on top
            Op::Ret => {
                let result = pop1(&mut self.stack)?;
                let fp = self.fp.expect("RET executed outside an active call frame");
                let mut popped = self.stack.split_off(fp);
                debug_assert_eq!(
                    popped.len(),
                    1,
                    "a function body must leave only its own Slot::Call frame above fp"
                );
                let frame = match popped.pop() {
                    Some(Slot::Call(f)) => f,
                    _ => unreachable!("Vm::fp must always index a Slot::Call"),
                };
                self.prog = frame.ret_prog;
                self.ip = frame.ret_ip;
                self.cp = frame.ret_cp;
                self.fp = frame.ret_fp;
                self.call_depth -= 1;
                self.stack.push(result);
            }

            Op::Halt => {
                self.ip = self.prog.code.len();
            }

            // `(target → )`. `ip` is already past this byte, so comparing it
            // with the target tells a backward jump (a loop) from a forward one.
            Op::Jump => {
                let (ip, cp) = pop_target(&mut self.stack)?;
                if (ip as usize) < self.ip {
                    self.interrupt.check()?;
                }
                self.ip = ip as usize;
                self.cp = cp as usize;
            }

            // `(cond target msg → )`: a non-`Bool` cond errors with `msg`
            Op::JumpIfFalse => {
                let msg = match pop1(&mut self.stack)?.unwrap_operand()? {
                    Operand::Text(t) => t,
                    other => {
                        return Err(QplError::Runtime(format!(
                            "Expected a Text operand, got {other:?}"
                        )));
                    }
                };
                let (ip, cp) = pop_target(&mut self.stack)?;
                match pop1(&mut self.stack)? {
                    Slot::Scalar(Value::Bool(true)) => {}
                    Slot::Scalar(Value::Bool(false)) => {
                        self.ip = ip as usize;
                        self.cp = cp as usize;
                    }
                    _ => return Err(QplError::Runtime(msg.to_string())),
                }
            }

            // `(cond target → cond)`: jump if `cond` is a `BoolVec` (peeked)
            Op::JumpIfVec => {
                let (ip, cp) = pop_target(&mut self.stack)?;
                if matches!(self.stack.last(), Some(Slot::Scalar(Value::BoolVec(_)))) {
                    self.ip = ip as usize;
                    self.cp = cp as usize;
                }
            }

            // `(mask v1 c2 v2 … d n → Val)`: see `ops::case_vec`
            Op::CaseVec => {
                let n = pop_count(&mut self.stack)?;
                let rest = popn(&mut self.stack, n)?;
                let mask = pop1(&mut self.stack)?.unwrap_scalar()?;
                self.stack.push(Slot::Scalar(ops::case_vec(mask, rest)?));
            }

            // `( → Noop)`
            Op::Noop => {
                self.stack.push(Slot::Noop);
            }
        }
        Ok(())
    }

    /// Whether `name` is a niladic closure or builtin, which `LOAD`/`LOAD_COL`
    /// call when named bare. Returns owned data so the caller can mutate `self`.
    fn niladic_closure_or_builtin(&self, name: &str) -> Niladic {
        match self.lookup(name) {
            Some(Lookup::Builtin(b)) if b.arity == (0..=0) => Niladic::Builtin(b.clone()),
            Some(Lookup::Value(Value::Closure(c))) if c.params.is_empty() => {
                Niladic::Closure(c.clone())
            }
            _ => Niladic::None,
        }
    }

    /// A bare name as an ordinary value, once a niladic call is ruled out.
    fn resolve_plain(&self, name: &str) -> Resolved {
        match self.lookup(name) {
            Some(Lookup::Value(Value::Lazy(lf))) => Resolved::Lazy(lf.clone()),
            Some(Lookup::Value(Value::Table(df))) => Resolved::Table(df.clone()),
            Some(Lookup::Value(v)) => Resolved::Scalar(v.clone()),
            Some(Lookup::Builtin(_)) => Resolved::BuiltinNonNiladic,
            None => Resolved::Undefined,
        }
    }

    /// `Op::Call`'s dispatch (also used by `Index` and niladic loads): a native
    /// by id runs directly, a closure begins a `CALL`, and any other name goes
    /// to [`ops::call_by_name`].
    fn call_value(&mut self, callee: Slot, args: Vec<Slot>) -> Result<(), QplError> {
        match callee {
            Slot::Operand(Operand::Native(id)) => {
                let result = match id {
                    crate::native::NativeId::Enlist => ops::native_enlist(args)?,
                    crate::native::NativeId::Roll => ops::native_roll(args)?,
                    crate::native::NativeId::Cfg => self.native_cfg(args)?,
                    crate::native::NativeId::StdoutLog => self.native_stdout_log(args)?,
                    crate::native::NativeId::PrintText => self.native_print_text(args)?,
                    crate::native::NativeId::LoadScript => self.native_load_script(args)?,
                    crate::native::NativeId::ImportScript => self.native_import_script(args)?,
                    crate::native::NativeId::Port => self.native_port(args)?,
                };
                self.stack.push(result);
            }
            Slot::Operand(Operand::Name(name)) => {
                enum Target {
                    Builtin(Builtin),
                    Closure(Arc<Closure>),
                    Generic,
                }
                let target = match self.lookup(&name) {
                    Some(Lookup::Builtin(b)) => Target::Builtin(b.clone()),
                    Some(Lookup::Value(Value::Closure(c))) => Target::Closure(c.clone()),
                    _ => Target::Generic,
                };
                match target {
                    Target::Builtin(b) => {
                        let result = self.call_builtin(&name, b, args)?;
                        self.stack.push(result);
                    }
                    Target::Closure(c) => {
                        self.begin_closure_call(c, args, Some(name.to_string()))?;
                    }
                    Target::Generic => {
                        let result = ops::call_by_name(self, &name, args)?;
                        self.stack.push(result);
                    }
                }
            }
            Slot::Scalar(Value::Closure(c)) => {
                self.begin_closure_call(c, args, None)?;
            }
            other => {
                return Err(QplError::Runtime(format!(
                    "cannot call {} — not a function",
                    other.type_name()
                )));
            }
        }
        Ok(())
    }

    /// Authorizes, arity-checks and runs a builtin (`.qpl.dt`, a Rust
    /// extension, ...).
    fn call_builtin(&mut self, name: &str, b: Builtin, args: Vec<Slot>) -> Result<Slot, QplError> {
        self.authorize(b.effect, name)?;
        if !b.arity.contains(&args.len()) {
            return Err(QplError::Runtime(format!(
                "'{name}' takes {} argument(s), got {}",
                crate::native::arity_desc(&b.arity),
                args.len()
            )));
        }
        match b.call {
            NativeCall::Internal(call) => call(self, args),
            NativeCall::Extension(call) => {
                let args = args
                    .into_iter()
                    .map(slot_into_value)
                    .collect::<Result<Vec<_>, _>>()?;
                match call(args).map_err(|e| QplError::Runtime(format!("{name}: {e}")))? {
                    Some(v) => Ok(value_into_slot(v)),
                    None => Ok(Slot::Noop),
                }
            }
        }
    }

    /// `.qpl.cfg key=value ...`; bare, print the settings.
    fn native_cfg(&mut self, mut args: Vec<Slot>) -> Result<Slot, QplError> {
        let args = expect_one_str(&mut args, ".qpl.cfg")?;
        if args.is_empty() {
            let current = self.config.describe();
            self.emit(&current);
            return Ok(Slot::Noop);
        }
        self.authorize(Effect::Session, ".qpl.cfg")?;
        for pair in args.split_whitespace() {
            let (key, value) = pair.split_once('=').ok_or_else(|| {
                QplError::Runtime(format!("expected key=value in `.qpl.cfg`, got '{pair}'"))
            })?;
            self.config.set(key.trim(), value.trim())?;
        }
        Ok(Slot::Noop)
    }

    /// `\1 <path>`: set (or with `""`, detach) the stdout log.
    fn native_stdout_log(&mut self, mut args: Vec<Slot>) -> Result<Slot, QplError> {
        let path = expect_one_str(&mut args, "\\1")?;
        self.set_stdout_log(&path)?;
        Ok(Slot::Noop)
    }

    /// `\d <stmt>`: print the disassembly rendered at compile time.
    fn native_print_text(&mut self, mut args: Vec<Slot>) -> Result<Slot, QplError> {
        let text = expect_one_str(&mut args, "\\d")?;
        self.emit(&text);
        Ok(Slot::Noop)
    }

    /// `\l <path>`: run the embedded program in the current scope.
    fn native_load_script(&mut self, mut args: Vec<Slot>) -> Result<Slot, QplError> {
        let program = expect_one_program(&mut args, "\\l")?;
        self.run_compiled(program)?;
        Ok(Slot::Noop)
    }

    /// `\i "<path>"`: run the embedded program as an import. Names were
    /// already qualified at compile time; this snapshots `globals`, clears the
    /// namespace, and restores the snapshot on failure.
    fn native_import_script(&mut self, mut args: Vec<Slot>) -> Result<Slot, QplError> {
        if args.len() != 2 {
            return Err(QplError::Runtime(format!(
                "\\i takes 2 argument(s), got {}",
                args.len()
            )));
        }
        let ns = match args.remove(1).unwrap_scalar()? {
            Value::Str(s) => s,
            other => {
                return Err(QplError::Runtime(format!(
                    "\\i expected a namespace string, got {other:?}"
                )));
            }
        };
        let program = expect_one_program(&mut args, "\\i")?;
        let prefix = format!("{ns}.");
        let snapshot = self.globals.clone();
        self.globals.retain(|k, _| !k.starts_with(&prefix));
        let result = self.run_compiled(program);
        if result.is_err() {
            self.globals = snapshot;
        }
        result?;
        Ok(Slot::Noop)
    }

    /// `\port [<expr>]`: close any open listener, then bind one on the port
    /// given as an `Int` or digit `Str` (an empty string, from bare `\port`,
    /// just closes). Never blocks; the run loop does the serving.
    #[cfg(feature = "ipc")]
    fn native_port(&mut self, mut args: Vec<Slot>) -> Result<Slot, QplError> {
        if args.len() != 1 {
            return Err(QplError::Runtime(format!(
                "corrupt bytecode: '\\port' expects exactly 1 argument, got {}",
                args.len()
            )));
        }
        let val = args.remove(0).unwrap_scalar()?;
        if let Some(state) = self.port.take() {
            state.handle.close();
        }
        let port: u16 = match val {
            Value::Str(s) if s.trim().is_empty() => return Ok(Slot::Noop),
            Value::Str(s) => s.trim().parse().map_err(|_| {
                QplError::Runtime(format!("\\port: expected a port number, got '{s}'"))
            })?,
            Value::Int(n) => u16::try_from(n)
                .map_err(|_| QplError::Runtime(format!("\\port: port number out of range: {n}")))?,
            other => {
                return Err(QplError::Runtime(format!(
                    "\\port: expected a port number, got {other:?}"
                )));
            }
        };
        self.port = Some(crate::ipc::PortState::open(port)?);
        Ok(Slot::Noop)
    }

    #[cfg(not(feature = "ipc"))]
    fn native_port(&mut self, _args: Vec<Slot>) -> Result<Slot, QplError> {
        Err(QplError::Runtime(
            "\\port requires the `ipc` feature (on by default; this build used `--no-default-features`)"
                .into(),
        ))
    }

    /// Begin a user-function call: check arity, depth and interrupt, push a
    /// [`Slot::Call`], and jump to the closure's entry point. The `step()` loop
    /// carries on in the callee until `Op::Ret`; no Rust frame is added.
    /// `args` were evaluated in the caller's frame. `name` is `Some` for a
    /// bare-name call (for the arity error).
    pub(crate) fn begin_closure_call(
        &mut self,
        closure: Arc<Closure>,
        args: Vec<Slot>,
        name: Option<String>,
    ) -> Result<(), QplError> {
        let label = name.clone().unwrap_or_else(|| closure.display.clone());
        if args.len() != closure.params.len() {
            return Err(QplError::Runtime(format!(
                "function '{label}' takes {} argument(s), got {}",
                closure.params.len(),
                args.len()
            )));
        }
        self.interrupt.check()?;
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(QplError::Runtime(format!(
                "function recursion too deep (limit {MAX_CALL_DEPTH})"
            )));
        }
        let mut locals = HashMap::with_capacity(closure.params.len());
        for (p, slot) in closure.params.iter().zip(args) {
            locals.insert(p.clone(), slot_into_value(slot)?);
        }
        let frame = CallFrame {
            ret_prog: self.prog.clone(),
            ret_ip: self.ip,
            ret_cp: self.cp,
            ret_fp: self.fp,
            locals,
        };
        self.fp = Some(self.stack.len());
        self.stack.push(Slot::Call(frame));
        self.call_depth += 1;
        self.prog = closure.program.clone();
        self.ip = closure.entry.0 as usize;
        self.cp = closure.entry.1 as usize;
        Ok(())
    }
}

/// The result of [`Vm::niladic_closure_or_builtin`].
enum Niladic {
    Builtin(Builtin),
    Closure(Arc<Closure>),
    None,
}

/// The result of [`Vm::resolve_plain`].
enum Resolved {
    Lazy(Box<LazyFrame>),
    Table(DataFrame),
    Scalar(ast::Value),
    BuiltinNonNiladic,
    Undefined,
}

#[derive(Debug)]
pub enum EvalResult {
    Table(DataFrame),
    Stored,
    Scalar(ast::Value),
    /// a lazy plan's (optimised) text
    Lazy(String),
}

fn explain_plan(lf: &LazyFrame) -> String {
    lf.clone()
        .explain(true)
        .unwrap_or_else(|e| format!("<could not explain plan: {e}>"))
}

pub fn run_vm(source: &str, vm: &mut Vm) -> Result<EvalResult, QplError> {
    let tokens = tokenise(source)?;
    let stmt = parse(tokens)?;
    let program = compile(&stmt)?;
    vm.eval(program)
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Whether `expr` has dtype `String` against `frame`'s schema, to choose
/// between parsing and `.cast()` for a temporal cast. Resolved schema-only.
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

fn pop1(stack: &mut Vec<Slot>) -> Result<Slot, QplError> {
    stack
        .pop()
        .ok_or_else(|| QplError::Runtime("stack underflow".into()))
}

fn popn(stack: &mut Vec<Slot>, n: usize) -> Result<Vec<Slot>, QplError> {
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
        ast::Value::Int(n) => lit(n),
        ast::Value::Float(n) => lit(n),
        ast::Value::Str(s) => lit(s),
        ast::Value::Sym(s) => lit(s),
        ast::Value::Bool(b) => lit(b),
        ast::Value::IntVec(s) => s.lit(),
        ast::Value::FloatVec(s) => s.lit(),
        ast::Value::BoolVec(s) => s.lit(),
        ast::Value::SymVec(s) | ast::Value::StrVec(s) => s.lit(),
        // rebase kdb offsets to Polars' 1970 epoch, with the Polars dtype
        ast::Value::Date(d) => lit(d + temporal::DAYS_2000_TO_1970).cast(DataType::Date),
        ast::Value::Month(mo) => {
            let days = temporal::days_from_civil(
                2000 + mo.div_euclid(12),
                (mo.rem_euclid(12) + 1) as u32,
                1,
            );
            lit(days).cast(DataType::Date)
        }
        ast::Value::Time(ns) => lit(ns).cast(DataType::Time),
        ast::Value::Minute(m) => lit(m as i64 * 60_000_000_000).cast(DataType::Time),
        ast::Value::Second(s) => lit(s as i64 * 1_000_000_000).cast(DataType::Time),
        ast::Value::Timestamp(ns) => lit(ns + temporal::NS_2000_TO_1970)
            .cast(DataType::Datetime(TimeUnit::Nanoseconds, None)),
        ast::Value::Timespan(ns) => lit(ns).cast(DataType::Duration(TimeUnit::Nanoseconds)),
        v @ (ast::Value::Handle(_) | ast::Value::Future(_)) => {
            return Err(QplError::Runtime(format!(
                "{v:?} cannot be used in a query expression"
            )));
        }
        // a function is a value, but not a *column* value
        ast::Value::Closure(_) => {
            return Err(QplError::Runtime(
                "a function cannot be used in a query expression".into(),
            ));
        }
        // unreachable in practice: `lookup_global` never substitutes a table
        v @ (ast::Value::Table(_) | ast::Value::Lazy(_)) => {
            return Err(QplError::Runtime(format!(
                "{v:?} cannot be used in a query expression"
            )));
        }
        // temporal vectors rebase like their scalars, elementwise
        ast::Value::DateVec(s) => (s.lit() + lit(temporal::DAYS_2000_TO_1970)).cast(DataType::Date),
        ast::Value::MonthVec(s) => {
            let days: Vec<i32> = s
                .i32()?
                .into_no_null_iter()
                .map(|mo| {
                    temporal::days_from_civil(
                        2000 + mo.div_euclid(12),
                        (mo.rem_euclid(12) + 1) as u32,
                        1,
                    )
                })
                .collect();
            Series::new("".into(), days).lit().cast(DataType::Date)
        }
        ast::Value::TimeVec(s) => s.lit().cast(DataType::Time),
        ast::Value::MinuteVec(s) => {
            let ns: Vec<i64> = s
                .i32()?
                .into_no_null_iter()
                .map(|m| m as i64 * 60_000_000_000)
                .collect();
            Series::new("".into(), ns).lit().cast(DataType::Time)
        }
        ast::Value::SecondVec(s) => {
            let ns: Vec<i64> = s
                .i32()?
                .into_no_null_iter()
                .map(|sec| sec as i64 * 1_000_000_000)
                .collect();
            Series::new("".into(), ns).lit().cast(DataType::Time)
        }
        ast::Value::TimestampVec(s) => (s.lit() + lit(temporal::NS_2000_TO_1970))
            .cast(DataType::Datetime(TimeUnit::Nanoseconds, None)),
        ast::Value::TimespanVec(s) => s.lit().cast(DataType::Duration(TimeUnit::Nanoseconds)),
    })
}

fn polars_dtype(name: &str) -> Result<DataType, QplError> {
    Ok(match name {
        "f64" | "float" => DataType::Float64,
        "f32" => DataType::Float32,
        "i64" | "int" => DataType::Int64,
        "i32" => DataType::Int32,
        "i16" => DataType::Int16,
        "i8" => DataType::Int8,
        "u64" => DataType::UInt64,
        "u32" => DataType::UInt32,
        "u16" => DataType::UInt16,
        "u8" => DataType::UInt8,
        "bool" => DataType::Boolean,
        "str" | "string" => DataType::String,
        "long" => DataType::Int64,
        // column-context temporal targets: `month` maps to `Date`, and
        // `minute`/`second` to `Time` (not truncated to the unit)
        "date" => DataType::Date,
        "month" => DataType::Date,
        "time" | "minute" | "second" => DataType::Time,
        "timestamp" => DataType::Datetime(TimeUnit::Nanoseconds, None),
        "timespan" => DataType::Duration(TimeUnit::Nanoseconds),
        _ => return Err(QplError::Runtime(format!("unknown cast type '{name}'"))),
    })
}

/// `load <path>`. Unavailable on wasm (no filesystem, and the readers don't
/// build there); a runtime error so grammar and messages stay identical.
#[cfg(target_family = "wasm")]
fn load_file(path: &str) -> Result<LazyFrame, QplError> {
    Err(QplError::Runtime(format!(
        "cannot load '{path}': file I/O is not available in this build"
    )))
}

#[cfg(not(target_family = "wasm"))]
fn load_file(path: &str) -> Result<LazyFrame, QplError> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "parquet" | "pq" | "parq" => {
            LazyFrame::scan_parquet(path.into(), ScanArgsParquet::default())
                .map_err(|e| QplError::Runtime(e.to_string()))
        }
        "csv" => LazyCsvReader::new(path.into())
            .finish()
            .map_err(|e| QplError::Runtime(e.to_string())),
        other => Err(QplError::Runtime(format!(
            "unsupported file format '.{other}' (supported: parquet, csv)"
        ))),
    }
}

/// `<table> sink <path>`. Unavailable on wasm — see [`load_file`].
#[cfg(target_family = "wasm")]
fn sink_file(_lf: LazyFrame, path: &str) -> Result<(), QplError> {
    Err(QplError::Runtime(format!(
        "cannot sink to '{path}': file I/O is not available in this build"
    )))
}

#[cfg(not(target_family = "wasm"))]
fn sink_file(lf: LazyFrame, path: &str) -> Result<(), QplError> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let file_write_format = match ext.as_str() {
        "parquet" | "pq" | "parq" => {
            FileWriteFormat::Parquet(Arc::new(ParquetWriteOptions::default()))
        }
        "csv" => FileWriteFormat::Csv(Default::default()),
        other => {
            return Err(QplError::Runtime(format!(
                "unsupported file format '.{other}' (supported: parquet, csv)"
            )));
        }
    };
    lf.sink(
        SinkDestination::File {
            target: SinkTarget::Path(path.into()),
        },
        file_write_format,
        UnifiedSinkArgs {
            mkdir: true,
            maintain_order: true,
            sync_on_close: SyncOnCloseType::None,
            cloud_options: None,
            sinked_paths_callback: None,
        },
    )
    .map_err(|e| QplError::Runtime(e.to_string()))?
    .collect_with_engine(Engine::Streaming)
    .map_err(|e| QplError::Runtime(e.to_string()))?;
    Ok(())
}

/// `.over(partition)`, sorting each partition by the `order` keys first if
/// given (one direction for all keys; mixed directions are ranking-only).
/// Results map back to the original rows.
fn apply_over(e: Expr, part: &[Expr], order: &[(String, bool)]) -> Result<Expr, QplError> {
    if order.is_empty() {
        return e
            .over(part)
            .map_err(|err| QplError::Runtime(err.to_string()));
    }
    let order_by: Vec<Expr> = order.iter().map(|(c, _)| col(c.as_str())).collect();
    let sort = SortOptions::default().with_order_descending(order[0].1);
    e.over_with_options(
        Some(part.to_vec()),
        Some((order_by, sort)),
        WindowMapping::GroupsToRows,
    )
    .map_err(|err| QplError::Runtime(err.to_string()))
}

/// A rolling-window aggregate (`... over `k order `t asc rolling n`). `col` is
/// the raw column; `agg` names the rolling reduction.
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
        "sum" => column.rolling_sum(opts),
        "avg" | "mean" => column.rolling_mean(opts),
        "min" => column.rolling_min(opts),
        "max" => column.rolling_max(opts),
        "std" | "dev" => column.rolling_std(opts),
        "var" => column.rolling_var(opts),
        "median" | "med" => column.rolling_median(opts),
        other => {
            return Err(QplError::Runtime(format!(
                "`rolling` supports sum/avg/min/max/std/var/median, not '{other}'"
            )));
        }
    };
    let part: Vec<Expr> = partition.iter().map(|c| col(c.as_str())).collect();
    apply_over(rolled, &part, order)
}

/// A window expression. `WindowFn::Over` broadcasts `target` over each
/// partition. Ranking verbs need one ordering key: a single `order` column is
/// used directly; several (each with its own direction) are replaced by dense
/// per-partition ranks packed into one number that sorts lexicographically.
/// That key is ranked with `Ordinal` (`rn`), `Min` (`rank`) or `Dense`
/// (`drank`).
fn build_window(
    func: WindowFn,
    target: Option<Expr>,
    partition: &[String],
    order: &[(String, bool)],
) -> Result<Expr, QplError> {
    let part: Vec<Expr> = partition.iter().map(|c| col(c.as_str())).collect();
    let over = |e: Expr, keys: &[Expr]| {
        e.over(keys)
            .map_err(|err| QplError::Runtime(err.to_string()))
    };

    if let WindowFn::Over = func {
        return apply_over(target.expect("Over target"), &part, order);
    }

    let (rank_key, descending) = if let [(name, desc)] = order {
        (col(name.as_str()), *desc)
    } else {
        // composite = ((r1)*B2 + r2)*B3 + r3 ..., where Bi = max(r_i) + 1
        let mut composite: Option<Expr> = None;
        for (name, desc) in order {
            let ri = over(
                col(name.as_str())
                    .rank(
                        RankOptions {
                            method: RankMethod::Dense,
                            descending: *desc,
                        },
                        None,
                    )
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
    let ranked = over(
        rank_key.rank(RankOptions { method, descending }, None),
        &part,
    )?;
    Ok(ranked.cast(DataType::Int64))
}

pub(crate) fn apply_call(func: &str, mut args: Vec<Expr>) -> Result<Expr, QplError> {
    if args.is_empty() {
        return Err(QplError::Runtime(format!("'{func}' called with no args")));
    }
    // dyadic verbs (`<param> verb <col>`): args are `[value, param]`
    if args.len() == 2 {
        let param = args.pop().unwrap();
        let value = args.pop().unwrap();
        return apply_dyadic(func, value, param);
    }
    let arg = args.remove(0);
    Ok(match func {
        "sum" => arg.sum(),
        "avg" | "mean" => arg.mean(),
        "min" => arg.min(),
        "max" => arg.max(),
        "count" => arg.count(),
        "first" => arg.first(),
        "last" => arg.last(),
        "std" | "dev" => arg.std(1),
        "var" => arg.var(1),
        "median" | "med" => arg.median(),
        "mode" | "modal" => arg.mode(false).sort(SortOptions::default()).first(),
        "skew" => arg.skew(false),
        "kurt" | "kurtosis" => arg.kurtosis(true, false),
        "any" => arg.any(true),
        "all" => arg.all(true),
        "prod" | "product" => arg.product(),
        "argmin" => arg.arg_min(),
        "argmax" => arg.arg_max(),
        "nnull" | "null_count" => arg.null_count(),
        "isnull" => arg.is_null(),
        "notnull" => arg.is_not_null(),
        "cumsum" => arg.cum_sum(false),
        "cummax" => arg.cum_max(false),
        "cummin" => arg.cum_min(false),
        "cumprod" => arg.cum_prod(false),
        "cumcount" => arg.cum_count(false),
        "ffill" => arg.fill_null_with_strategy(FillNullStrategy::Forward(None)),
        "bfill" => arg.fill_null_with_strategy(FillNullStrategy::Backward(None)),
        "abs" => arg.abs(),
        "neg" => -arg,
        "not" => arg.not(),
        "distinct" | "n_unique" => arg.n_unique(),
        _ => return Err(QplError::Runtime(format!("unknown function '{func}'"))),
    })
}

/// Dyadic column verbs, `<param> verb <col>` (like `round`).
pub(crate) fn apply_dyadic(func: &str, value: Expr, param: Expr) -> Result<Expr, QplError> {
    Ok(match func {
        "quantile" | "pctl" => value.quantile(param, QuantileMethod::Linear),
        "shift" | "lag" => value.shift(param),
        "lead" => value.shift(-param),
        "fill" => value.fill_null(param),
        "diff" => value.diff(param, polars::series::ops::NullBehavior::Ignore),
        "pctchange" => value.pct_change(param),
        _ => return Err(QplError::Runtime(format!("unknown dyadic verb '{func}'"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::compile;
    use crate::lexer::tokenise;
    use crate::ops::{NS_PER_DAY, scalar_cast};
    use crate::parser::parse;

    fn make_vm() -> Vm {
        let df = df![
            "c1" => ["a", "b", "a", "c"],
            "c2" => [10i64, 20, 30, 15],
            "c3" => [1.0f64, 2.0, 3.0, 4.0],
        ]
        .unwrap();
        let mut vm = Vm::new_writable();
        vm.globals.insert("t".into(), Value::Table(df));
        vm
    }

    fn run_instructions(mut vm: Vm, src: &str) -> EvalResult {
        let tokens = tokenise(src).expect("lex");
        let stmt = parse(tokens).expect("parse");
        let prog = compile(&stmt).expect("compile");
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
        df.column(name)
            .unwrap()
            .i64()
            .unwrap()
            .into_no_null_iter()
            .collect()
    }

    fn f64s(df: &DataFrame, name: &str) -> Vec<f64> {
        df.column(name)
            .unwrap()
            .f64()
            .unwrap()
            .into_no_null_iter()
            .collect()
    }

    fn opt_i64s(df: &DataFrame, name: &str) -> Vec<Option<i64>> {
        df.column(name).unwrap().i64().unwrap().iter().collect()
    }

    fn opt_f64s(df: &DataFrame, name: &str) -> Vec<Option<f64>> {
        df.column(name).unwrap().f64().unwrap().iter().collect()
    }

    fn bools(df: &DataFrame, name: &str) -> Vec<bool> {
        df.column(name)
            .unwrap()
            .bool()
            .unwrap()
            .iter()
            .flatten()
            .collect()
    }

    fn strs(df: &DataFrame, name: &str) -> Vec<String> {
        df.column(name)
            .unwrap()
            .str()
            .unwrap()
            .iter()
            .flatten()
            .map(str::to_owned)
            .collect()
    }

    fn sorted(df: DataFrame, by: &str) -> DataFrame {
        df.sort([by], SortMultipleOptions::default()).unwrap()
    }

    // scalars

    #[test]
    fn eval_int() {
        let mut vm = make_vm();
        match run_vm("42", &mut vm).expect("eval") {
            EvalResult::Scalar(v) => assert_eq!(v, ast::Value::Int(42)),
            other => panic!("expected a scalar, got {other:?}"),
        }
    }

    #[test]
    fn eval_bare_symbol_is_a_symbol_value() {
        let mut vm = make_vm();
        match run_vm("`trades", &mut vm).expect("eval") {
            EvalResult::Scalar(v) => assert_eq!(v, ast::Value::Sym("trades".into())),
            other => panic!("expected a scalar, got {other:?}"),
        }
    }

    #[test]
    fn eval_cast_string_var_to_symbol() {
        // `` `$o `` resolves the string global `o` and interns it into a symbol
        let mut vm = make_vm();
        run_vm("o: \"out.parquet\"", &mut vm).expect("bind o");
        match run_vm("`$o", &mut vm).expect("eval") {
            EvalResult::Scalar(v) => assert_eq!(v, ast::Value::Sym("out.parquet".into())),
            other => panic!("expected a scalar, got {other:?}"),
        }
    }

    #[test]
    fn like_matches_per_q_glob_semantics() {
        let mut vm = make_vm();
        let cases: &[(&str, &str, bool)] = &[
            ("quick", "qu?ck", true),    // ? = any single char
            ("quickly", "quick*", true), // * = any sequence, incl. empty
            ("quick", "quick*", true),
            ("brown", "br[ao]wn", true), // char class
            ("brown", "br[eiu]wn", false),
            ("br0wn", "br[0-3]wn", true), // range
            ("br9wn", "br[0-3]wn", false),
            ("brown", "[^cf]rown", true), // negated class
            ("crown", "[^cf]rown", false),
            ("brown", "brown", true), // no pattern chars = exact match
            ("brownx", "brown", false),
            ("BROWN", "brown", false),  // case-sensitive
            ("br*wn", "br[*]wn", true), // escaping via a single-char class
            ("br?wn", "br[?]wn", true),
            ("br]wn", "[bf]r[]]wn", true),
            ("a[c", "a[[]c", true),
        ];
        for (text, pattern, expect) in cases {
            let src = format!("\"{text}\" like \"{pattern}\"");
            match run_vm(&src, &mut vm).expect("eval") {
                EvalResult::Scalar(v) => {
                    assert_eq!(v, ast::Value::Bool(*expect), "{text:?} like {pattern:?}")
                }
                other => panic!("expected a scalar, got {other:?}"),
            }
        }
    }

    #[test]
    fn like_treats_symbol_and_string_uniformly() {
        let mut vm = make_vm();
        match run_vm("`quick like \"qu?ck\"", &mut vm).expect("eval") {
            EvalResult::Scalar(v) => assert_eq!(v, ast::Value::Bool(true)),
            other => panic!("expected a scalar, got {other:?}"),
        }
    }

    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn sink_and_load_round_trip_with_a_string_path() {
        let path = std::env::temp_dir().join("qpl_vm_test_sink_round_trip.csv");
        let path_str = path.to_str().unwrap();

        run_instructions(make_vm(), &format!("t sink \"{path_str}\""));

        let df = run(Vm::new(), &format!("load \"{path_str}\""));
        assert_eq!(df.height(), 4);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn load_accepts_a_bound_variable_path() {
        // `load` takes a variable path
        let path = std::env::temp_dir().join("qpl_vm_test_load_variable_path.csv");
        let path_str = path.to_str().unwrap();

        run_instructions(make_vm(), &format!("t sink \"{path_str}\""));

        let mut vm = Vm::new();
        run_vm(&format!("p: \"{path_str}\""), &mut vm).unwrap();
        let df = run(vm, "load p");
        assert_eq!(df.height(), 4);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn function_call_accepts_a_lazy_load_bracket_argument() {
        // a lazy load as a bracket-call argument
        let path = std::env::temp_dir().join("qpl_vm_test_lazy_load_bracket_arg.csv");
        let path_str = path.to_str().unwrap();
        run_instructions(make_vm(), &format!("t sink \"{path_str}\""));

        let mut vm = Vm::new();
        run_vm("f: {[t] cols t}", &mut vm).unwrap();
        run_vm(&format!("p: \"{path_str}\""), &mut vm).unwrap();
        let df = run(vm, "f[lazy load p]");
        assert_eq!(df.height(), 3); // c1, c2, c3

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

    // --- read-only sessions (the default; `qpl -w` for writes) ---

    mod read_only_session {
        use super::*;

        fn read_only_vm() -> Vm {
            let mut vm = Vm::new();
            vm.globals = make_vm().globals;
            vm
        }

        fn assert_write_refused(vm: &mut Vm, src: &str, what: &str) {
            match run_vm(src, vm) {
                Err(QplError::Runtime(msg)) => assert_eq!(
                    msg,
                    format!(
                        "Cannot perform write action in read-only session: {what} (start qpl with -w to allow writes)"
                    )
                ),
                other => panic!("expected '{src}' to be refused, got {other:?}"),
            }
        }

        #[test]
        fn a_session_is_read_only_unless_built_writable() {
            assert!(Vm::new().read_only());
            assert!(Vm::default().read_only());
            assert!(!Vm::new_writable().read_only());
        }

        #[test]
        fn sink_is_refused_and_writes_nothing() {
            let path = "qpl_vm_test_read_only_session_sink.parquet";
            let mut vm = read_only_vm();
            assert_write_refused(&mut vm, &format!(r#"t sink "{path}""#), "sink");
            assert!(!std::path::Path::new(path).exists());
        }

        #[test]
        fn sink_inside_a_function_is_refused_when_reached() {
            let path = "qpl_vm_test_read_only_session_fn_sink.parquet";
            let mut vm = read_only_vm();
            run_vm(&format!(r#"save: {{[x] x sink "{path}"}}"#), &mut vm)
                .expect("defining the function writes nothing");
            assert_write_refused(&mut vm, "save[t]", "sink");
            assert!(!std::path::Path::new(path).exists());
        }

        #[test]
        fn queries_bindings_cfg_and_log_are_allowed() {
            let mut vm = read_only_vm();
            run_vm("u: select from t where c2 > 10", &mut vm).expect("assignment");
            run_vm("x: 1 + 2", &mut vm).expect("scalar assignment");
            run_vm("select from u", &mut vm).expect("query");
            vm.capture = Some(String::new());
            run_vm(r#"log["x is "; x]"#, &mut vm).expect("log");
            assert_eq!(vm.capture.as_deref(), Some("x is 3\n"));
            vm.native_cfg(vec![Slot::Scalar(Value::Str("round_type=HALF_UP".into()))])
                .expect(".qpl.cfg");
        }

        #[test]
        fn stdout_log_to_a_file_is_refused_but_detaching_is_allowed() {
            let path = "qpl_vm_test_read_only_session_stdout.log";
            let mut vm = read_only_vm();
            match vm.set_stdout_log(path) {
                Err(QplError::Runtime(msg)) => assert_eq!(
                    msg,
                    "Cannot perform write action in read-only session: \\1 (stdout log) (start qpl with -w to allow writes)"
                ),
                other => panic!("expected \\1 to be refused, got {other:?}"),
            }
            assert!(!std::path::Path::new(path).exists());
            vm.set_stdout_log("").expect("detaching writes nothing");
        }

        #[test]
        fn a_write_builtin_is_refused_before_it_runs() {
            fn must_not_run(_: &mut Vm, _: Vec<Slot>) -> Result<Slot, QplError> {
                panic!("a write builtin ran in a read-only session")
            }
            let mut vm = read_only_vm();
            for (name, arity) in [(".test.write", 1..=1), (".test.write0", 0..=0)] {
                vm.builtins.insert(
                    name.into(),
                    Builtin {
                        arity,
                        effect: Effect::Write,
                        call: NativeCall::Internal(must_not_run),
                    },
                );
            }
            assert_write_refused(&mut vm, ".test.write[1]", ".test.write");
            // a niladic builtin is called by naming it bare
            assert_write_refused(&mut vm, "x: .test.write0", ".test.write0");
        }

        #[cfg(feature = "ipc")]
        #[test]
        fn a_write_handle_is_refused_before_connecting() {
            let mut vm = read_only_vm();
            assert_write_refused(&mut vm, "`w!hopen 1", "whopen");
            assert!(vm.connections.is_empty());
        }

        #[cfg(feature = "ipc")]
        #[test]
        fn a_write_handle_request_still_cannot_write() {
            use crate::ipc::HandleMode;
            let path = "qpl_vm_test_read_only_session_ipc_sink.parquet";
            let mut vm = read_only_vm();
            vm.with_request_permission(HandleMode::Write, |vm| {
                run_vm("x: 1", vm).expect("assignment over a write handle");
                assert_write_refused(vm, &format!(r#"t sink "{path}""#), "sink");
            });
            assert!(!std::path::Path::new(path).exists());
        }
    }

    // --- per-connection read/write permission (`ipc` feature) ---

    #[cfg(feature = "ipc")]
    mod request_permission {
        use super::*;
        use crate::ipc::HandleMode;

        fn assert_read_only_rejects(src: &str) {
            let mut vm = make_vm();
            let err = vm
                .with_request_permission(HandleMode::Read, |vm| run_vm(src, vm))
                .expect_err(&format!(
                    "expected '{src}' to be rejected over a read handle"
                ));
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
        fn read_handle_rejects_opening_a_write_handle() {
            assert_read_only_rejects("`w!hopen 1");
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
            // only writes are gated; a query works over a read handle
            let mut vm = make_vm();
            vm.with_request_permission(HandleMode::Read, |vm| run_vm("select from t", vm))
                .expect("a read-only select must succeed");
        }

        #[test]
        fn local_input_is_never_restricted_regardless_of_request_mode() {
            // local calls aren't wrapped in `with_request_permission`, so they
            // always see `None`
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
        assert_eq!(scalar_cast(Float(45.3), "int", false).unwrap(), Int(45));
        assert_eq!(scalar_cast(Float(45.9), "u32", false).unwrap(), Int(45));
        assert_eq!(scalar_cast(Int(300), "i8", false).unwrap(), Int(300));
        // int/bool -> float
        assert_eq!(scalar_cast(Int(45), "f64", false).unwrap(), Float(45.0));
        assert_eq!(scalar_cast(Bool(true), "f32", false).unwrap(), Float(1.0));
        // -> bool
        assert_eq!(scalar_cast(Int(0), "bool", false).unwrap(), Bool(false));
        assert_eq!(scalar_cast(Float(3.0), "bool", false).unwrap(), Bool(true));
        assert_eq!(
            scalar_cast(Str("true".into()), "bool", false).unwrap(),
            Bool(true)
        );
        // string parses into a number
        assert_eq!(
            scalar_cast(Str("45".into()), "int", false).unwrap(),
            Int(45)
        );
        assert_eq!(
            scalar_cast(Str("3.9".into()), "int", false).unwrap(),
            Int(3)
        );
        assert_eq!(
            scalar_cast(Str(" 3.5 ".into()), "f64", false).unwrap(),
            Float(3.5)
        );
        // -> string
        assert_eq!(
            scalar_cast(Int(45), "str", false).unwrap(),
            Str("45".into())
        );
        assert_eq!(
            scalar_cast(Bool(true), "string", false).unwrap(),
            Str("true".into())
        );
    }

    #[test]
    fn scalar_cast_rejects_junk() {
        use ast::Value::*;
        assert!(scalar_cast(Str("nope".into()), "bool", false).is_err());
        assert!(scalar_cast(Str("abc".into()), "int", false).is_err());
        assert!(scalar_cast(Int(1), "widget", false).is_err());
    }

    #[test]
    fn eval_scalar_cast_end_to_end() {
        // `l: int$45.3` folds during scalar eval
        let mut vm = make_vm();
        run_vm("l: int$45.3", &mut vm).unwrap();
        assert_eq!(vm.globals.get("l"), Some(&ast::Value::Int(45)));
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
        assert_eq!(
            scalar_of("l: 2000.01.02 + 0D00:00:00.000000001"),
            Timestamp(NS_PER_DAY + 1)
        );
    }

    #[test]
    fn temporal_scalar_comparison() {
        use ast::Value::*;
        assert_eq!(scalar_of("l: 2024.03.15 < 2024.03.16"), Bool(true));
        assert_eq!(scalar_of("l: 2024.03.15 = 2024.03.15"), Bool(true));
        assert_eq!(scalar_of("l: 09:30 > 09:00"), Bool(true));
        // cross-variant, same kind class (date <-> timestamp, minute <-> second)
        assert_eq!(
            scalar_of("l: 2024.03.15 = 2024.03.15D00:00:00.0"),
            Bool(true)
        );
        assert_eq!(scalar_of("l: 09:30 < 09:31:00"), Bool(true));
    }

    #[test]
    fn temporal_scalar_plus_integer_uses_the_operand_unit() {
        use ast::Value::*;
        assert_eq!(scalar_of("l: 09:30 + 5"), Minute(575)); // minutes
        assert_eq!(scalar_of("l: 12:00:00 + 5"), Second(43205)); // seconds
        assert_eq!(scalar_of("l: 12:30:00.000 + 5"), Time(45_000_005_000_000)); // ms
        assert_eq!(scalar_of("l: 2024.03m + 1"), Month(291)); // months
        assert_eq!(
            scalar_of("l: 2000.01.01D00:00:00.0 + 1"),
            Timestamp(1), // ns
        );
    }

    #[test]
    fn temporal_negative_literal_and_overflow() {
        use ast::Value::*;
        assert_eq!(
            scalar_of("l: -0D01:00:00.000000000"),
            Timespan(-3_600_000_000_000)
        );
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
        // temporal -> underlying kdb day/month offset (unaffected by `useqepoch`)
        assert_eq!(scalar_of("l: `int$2024.03.15"), Int(8840));
        // `long$`/`timestamp$` cross the Unix-epoch boundary by default
        assert_eq!(
            scalar_of("l: `long$2000.01.01D00:00:00.000000001"),
            Int(temporal::NS_2000_TO_1970 + 1),
        );
        // string parse via a kdb type code
        assert_eq!(
            scalar_of(r#"l: "p"$"2000.01.01D00:00:00.000000000""#),
            Timestamp(0),
        );
        assert_eq!(scalar_of(r#"l: "d"$"2024.03.15""#), Date(8840));
    }

    #[test]
    fn timestamp_int_boundary_defaults_to_unix_epoch() {
        use ast::Value::*;
        // a raw long is ns since 1970.01.01 by default, matching Polars' column
        // cast
        let mut vm = make_vm();
        run_vm("l: `timestamp$1000000000", &mut vm).unwrap();
        assert_eq!(
            vm.globals.get("l"),
            Some(&Timestamp(1_000_000_000 - temporal::NS_2000_TO_1970))
        );
        run_vm("l: `long$2000.01.01D00:00:00.0", &mut vm).unwrap();
        assert_eq!(vm.globals.get("l"), Some(&Int(temporal::NS_2000_TO_1970)));

        // `useqepoch=true` uses kdb's 2000.01.01 boundary
        vm.config.useqepoch = true;
        run_vm("l: `timestamp$1000000000", &mut vm).unwrap();
        assert_eq!(vm.globals.get("l"), Some(&Timestamp(1_000_000_000)));
        run_vm("l: `long$2000.01.01D00:00:00.0", &mut vm).unwrap();
        assert_eq!(vm.globals.get("l"), Some(&Int(0)));
    }

    #[test]
    fn qpl_now_functions_evaluate_in_scalar_context() {
        assert!(matches!(scalar_of("l: .qpl.dt"), ast::Value::Date(_)));
        assert!(matches!(scalar_of("l: .qpl.ts"), ast::Value::Timestamp(_)));
    }

    #[test]
    fn temporal_literal_projects_as_a_typed_column() {
        let df = run(
            make_vm(),
            "select d: 2024.03.15, ts: 2024.03.15D09:30:00.0 from t",
        );
        assert_eq!(df.column("d").unwrap().dtype(), &DataType::Date);
        assert!(matches!(
            df.column("ts").unwrap().dtype(),
            DataType::Datetime(TimeUnit::Nanoseconds, None)
        ));
    }

    #[test]
    fn string_temporal_casts_use_the_dedicated_parsers() {
        // string → temporal casts go through Polars' string parsers: `date$`/
        // `month$` give `Date`, `timestamp$` keeps the time, `time$` gives
        // `Time`. ISO and kdb dotted formats both parse.
        let mut vm = make_vm();
        let src = df![
            "ds"  => ["2024.03.15", "2024-06-01", "2024-01-02"],
            "ts"  => ["2024-03-15T09:30:00", "2024-06-01T16:00:00", "2024-01-02T00:00:01"],
            "tm"  => ["09:30:00", "16:00:00", "00:00:01"],
        ]
        .unwrap();
        vm.globals.insert("d".into(), Value::Table(src));
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
        // strict: an unparseable value aborts the query rather than nulling
        let mut vm = make_vm();
        let src = df!["ds" => ["2024-03-15", "not a date", "2024-01-02"]].unwrap();
        vm.globals.insert("d".into(), Value::Table(src));
        let prog =
            compile(&parse(tokenise("select a: `date$ds from d").unwrap()).unwrap()).unwrap();
        match vm.eval(prog) {
            Err(e) => assert!(
                e.to_string().contains("not a date"),
                "unexpected error: {e}"
            ),
            Ok(_) => panic!("expected a parse failure on an unreadable date string"),
        }
    }

    #[test]
    fn temporal_casts_on_an_already_temporal_column_use_a_plain_cast() {
        // an already-temporal column uses `.cast()`, not the string parser
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
        vm.globals.insert("d".into(), Value::Table(src));
        let df = run(vm, "select a: `date$ts, b: `time$ts from d");
        assert_eq!(df.column("a").unwrap().dtype(), &DataType::Date);
        assert_eq!(df.column("b").unwrap().dtype(), &DataType::Time);
        for name in ["a", "b"] {
            assert_eq!(
                df.column(name).unwrap().null_count(),
                0,
                "column {name} has nulls"
            );
        }
    }

    // scalar eval and assignment via instructions
    #[test]
    fn eval_assign_scalar() {
        let src = "x: 42";
        let tokens = tokenise(src).expect("lex");
        let stmt = parse(tokens).expect("parse");
        let prog = compile(&stmt).expect("compile");
        let mut vm = make_vm();
        vm.eval(prog).expect("eval");
        let val = vm.globals.get("x").expect("x exists");
        assert_eq!(val, &ast::Value::Int(42));
    }

    // --- LOAD ---

    #[test]
    fn load_of_a_bare_table_name_gives_a_frame() {
        // `u: t` resolves `t` through `LOAD` to a `Frame`
        let mut vm = make_vm();
        run_vm("u: t", &mut vm).expect("t resolves to a frame, not a scalar error");
        assert!(matches!(vm.globals.get("u"), Some(ast::Value::Table(_))));
    }

    #[test]
    fn load_of_a_bare_lazy_binding_stays_lazy() {
        let mut vm = make_vm();
        run_vm("l: lazy select from t", &mut vm).expect("bind lazy plan");
        match run_vm("l", &mut vm).expect("load the lazy binding") {
            EvalResult::Lazy(_) => {}
            other => panic!("expected a lazy plan, got {other:?}"),
        }
    }

    #[test]
    fn load_of_a_niladic_function_calls_it() {
        let mut vm = Vm::new();
        run_vm("f: {[] 42}", &mut vm).expect("define f");
        match run_vm("f", &mut vm).expect("load calls the niladic function") {
            EvalResult::Scalar(ast::Value::Int(42)) => {}
            other => panic!("expected Scalar(42), got {other:?}"),
        }
    }

    #[test]
    fn load_of_an_undefined_name_is_the_resolve_name_error_text() {
        let mut vm = Vm::new();
        let err = run_vm("nosuchname", &mut vm).expect_err("undefined name");
        assert!(
            err.to_string()
                .contains("undefined name 'nosuchname' (not a variable, table or lazy frame)"),
            "{err}"
        );
    }

    #[test]
    fn binop_mixed_fallback_still_computes_the_right_answer() {
        // the BINOP and the call operand compose to the right value
        let mut vm = Vm::new();
        run_vm("fac: {[n] ?[n<=1; 1; n * fac[n-1]]}", &mut vm).expect("define fac");
        match run_vm("fac[5]", &mut vm).expect("call fac") {
            EvalResult::Scalar(ast::Value::Int(120)) => {}
            other => panic!("expected Scalar(120), got {other:?}"),
        }
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
        let stmt = parse(tokens).expect("parse");
        let prog = compile(&stmt).expect("compile");
        assert!(make_vm().eval(prog).is_err());
    }

    #[test]
    fn enum_cast_builds_an_enum_column_from_a_global() {
        let mut vm = make_vm();
        vm.globals.insert(
            "e".into(),
            ast::sym_vec(vec!["a".into(), "b".into(), "c".into()]),
        );
        let df = run(vm, "select lvl: e::`$c1 from t");
        assert!(df.column("lvl").unwrap().dtype().is_enum());
    }

    #[test]
    fn enum_cast_maps_unknown_labels_to_null() {
        let mut vm = make_vm();
        vm.globals
            .insert("e".into(), ast::sym_vec(vec!["a".into(), "b".into()]));
        let df = run(vm, "select lvl: e::`$c1 from t");
        // c1 = [a, b, a, c] — the "c" row is not in the enum
        assert_eq!(df.column("lvl").unwrap().null_count(), 1);
    }

    #[test]
    fn enum_cast_with_undefined_global_is_an_error() {
        let tokens = tokenise("select lvl: nope::`$c1 from t").expect("lex");
        let stmt = parse(tokens).expect("parse");
        let prog = compile(&stmt).expect("compile");
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
            Some(&ast::sym_vec(vec![
                "low".into(),
                "mid".into(),
                "high".into()
            ])),
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
    fn cast_of_a_select_statement_end_to_end() {
        // a cast applies to a select's frame
        match run_instructions(make_vm(), "int$select c3 from t where c2 > 15") {
            EvalResult::Scalar(ast::Value::IntVec(s)) => {
                let got: Vec<i64> = s.i64().unwrap().into_no_null_iter().collect();
                assert_eq!(got, vec![2, 3]);
            }
            other => panic!("expected an int list, got {other:?}"),
        }
    }

    #[test]
    fn cast_of_a_collect_of_a_select_statement_end_to_end() {
        match run_instructions(make_vm(), "int$collect select c3 from t where c2 > 15") {
            EvalResult::Scalar(ast::Value::IntVec(s)) => {
                let got: Vec<i64> = s.i64().unwrap().into_no_null_iter().collect();
                assert_eq!(got, vec![2, 3]);
            }
            other => panic!("expected an int list, got {other:?}"),
        }
    }

    #[test]
    fn hash_take_on_a_column_expression_is_a_list() {
        match run_instructions(make_vm(), "2#select c1 from t") {
            EvalResult::Scalar(v @ ast::Value::StrVec(_)) => {
                assert_eq!(v.vec_strings().unwrap(), vec!["a", "b"])
            }
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
        assert_eq!(
            scalar_v(&mut vm, "l > 20"),
            ast::bool_vec(vec![false, true])
        );
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
        assert_eq!(
            scalar_v(&mut vm, "til 5"),
            ast::int_vec(vec![0, 1, 2, 3, 4])
        );
        assert_eq!(
            scalar_v(&mut vm, "10 til 15"),
            ast::int_vec(vec![10, 11, 12, 13, 14])
        );
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
                    df.column("cola")
                        .unwrap()
                        .i64()
                        .unwrap()
                        .into_no_null_iter()
                        .collect::<Vec<_>>(),
                    vec![0, 1, 2, 3, 4],
                );
                assert_eq!(
                    df.column("colb")
                        .unwrap()
                        .i64()
                        .unwrap()
                        .into_no_null_iter()
                        .collect::<Vec<_>>(),
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
            let names: Vec<&str> = df
                .get_column_names()
                .iter()
                .map(|name| name.as_str())
                .collect();
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
        let df = run(
            make_vm(),
            "update band: ?[c2>20;`high;c2>10;`mid;`low] from t",
        );
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
    fn isnull_and_notnull_filter_rows() {
        let mut vm = make_vm();
        vm.globals.insert(
            "n".into(),
            Value::Table(df!["x" => [Some(1i64), None, Some(3), None]].unwrap()),
        );
        let df = run(vm, "select from n where isnull x");
        assert_eq!(df.height(), 2);
        let mut vm = make_vm();
        vm.globals.insert(
            "n".into(),
            Value::Table(df!["x" => [Some(1i64), None, Some(3), None]].unwrap()),
        );
        let df = run(vm, "select from n where notnull x");
        assert_eq!(df.height(), 2);
    }

    #[test]
    fn fill_replaces_nulls_and_dropnull_drops_rows() {
        let mk = || {
            let mut vm = make_vm();
            vm.globals.insert(
                "n".into(),
                Value::Table(df!["x" => [Some(1i64), None, Some(3), None]].unwrap()),
            );
            vm
        };
        let df = run(mk(), "select 0 fill x from n");
        let x: Vec<Option<i64>> = df.column("x").unwrap().i64().unwrap().iter().collect();
        assert_eq!(x, vec![Some(1), Some(0), Some(3), Some(0)]);
        let df = run(mk(), "`x dropnull n");
        assert_eq!(df.height(), 2);
        let df = run(mk(), "select count i from (`x dropnull n)");
        assert_eq!(df.height(), 1);
    }

    #[test]
    fn distinct_on_a_column_is_n_unique() {
        let df = run(make_vm(), "select distinct c1 from t");
        assert_eq!(df.column("c1").unwrap().u32().unwrap().get(0), Some(3));
        let df = run(make_vm(), "select n: distinct c1 by c2 from t");
        assert_eq!(df.height(), 4);
        // table position is unchanged: deduplicates rows
        assert_eq!(run(make_vm(), "distinct select c1 from t").height(), 3);
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
        let names: Vec<&str> = df
            .get_column_names()
            .iter()
            .map(|name| name.as_str())
            .collect();
        assert_eq!(names, vec!["c1", "c3"]);
    }

    #[test]
    fn case_expression_returns_first_matching_value() {
        let df = run(
            make_vm(),
            "select bin: ?[c2>20;`high;c2>10;`mid;`low] from t",
        );
        assert_eq!(strs(&df, "bin"), vec!["low", "mid", "high", "mid"]);
    }

    #[test]
    fn dictionary_sort_order_survives_assignment() {
        let mut vm = make_vm();
        assert!(matches!(
            run_vm("t2: `c1`c2!01b t", &mut vm),
            Ok(EvalResult::Stored)
        ));
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
        // `%` is always float division
        let df = run(make_vm(), "select h: c2%2 from t");
        assert_eq!(f64s(&df, "h"), vec![5.0, 10.0, 15.0, 7.5]);
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
        let df = sorted(
            run(make_vm(), r#"select c2 from t where c1 like "[ab]""#),
            "c2",
        );
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
        let df = run(
            make_vm(),
            "select a: any c2 > 25, b: all c2 > 5, p: prod c3 from t",
        );
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
        assert_eq!(
            opt_i64s(&df, "d"),
            vec![None, Some(10), Some(10), Some(-15)]
        );
    }

    #[test]
    fn cumsum_over_partition_in_order() {
        // c1 partitions: a{c2:10,30}, b{20}, c{15}; cumsum in ascending c2 order,
        // mapped back to the original row positions [a10, b20, a30, c15].
        let df = run(
            make_vm(),
            "select r: cumsum c2 over `c1 order `c2 asc from t",
        );
        assert_eq!(i64s(&df, "r"), vec![10, 20, 40, 15]);
    }

    #[test]
    fn rolling_window_sum_over_partition() {
        // partition a has c3 {1.0, 3.0} in order -> [null, 4.0]; singletons -> null
        let df = run(
            make_vm(),
            "select r: sum c3 over `c1 order `c3 asc rolling 2 from t",
        );
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
        assert_eq!(strs(&df, "c1"), vec!["a", "b", "c"]);
        assert_eq!(i64s(&df, "total"), vec![40, 20, 15]);
    }

    #[test]
    fn by_key_reprojected_by_name_does_not_duplicate_the_column() {
        // projecting a `by` key by name mustn't duplicate the column
        let df = sorted(
            run(make_vm(), "select c1, c2, r: 1 diff c2 by c1 from t"),
            "c1",
        );
        // one row per group; "c1" appears once in the schema
        assert_eq!(strs(&df, "c1"), vec!["a", "b", "c"]);
        assert_eq!(
            df.get_column_names()
                .iter()
                .filter(|n| n.as_str() == "c1")
                .count(),
            1
        );
    }

    #[test]
    fn by_count() {
        let df = sorted(run(make_vm(), "select n: count c2 by c1 from t"), "c1");
        let ns: Vec<u32> = df
            .column("n")
            .unwrap()
            .u32()
            .unwrap()
            .into_no_null_iter()
            .collect();
        assert_eq!(strs(&df, "c1"), vec!["a", "b", "c"]);
        assert_eq!(ns, vec![2, 1, 1]);
    }

    #[test]
    fn by_max() {
        let df = sorted(run(make_vm(), "select hi: max c2 by c1 from t"), "c1");
        assert_eq!(strs(&df, "c1"), vec!["a", "b", "c"]);
        assert_eq!(i64s(&df, "hi"), vec![30, 20, 15]);
    }

    // --- virtual column i ---

    #[test]
    fn select_icol() {
        let df = run(make_vm(), "select i from t");
        // IColRef gets implicit alias "x"; polars adds it as u32
        let xs: Vec<u32> = df
            .column("x")
            .unwrap()
            .u32()
            .unwrap()
            .into_no_null_iter()
            .collect();
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
        assert_eq!(strs(&df, "c1"), vec!["a", "b"]);
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
        let df = sorted(
            run(make_vm(), "select c2 from select c1, c2 from t where c2>10"),
            "c2",
        );
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
        vm.globals.insert(
            "trades".into(),
            Value::Table(
                df![
                    "sym"   => ["a", "a", "b"],
                    "price" => [10i64, 20, 30],
                ]
                .unwrap(),
            ),
        );
        vm.globals.insert(
            "quotes".into(),
            Value::Table(
                df![
                    "sym" => ["a", "b", "c"],
                    "bid"  => [1i64, 2, 3],
                ]
                .unwrap(),
            ),
        );
        vm
    }

    #[test]
    fn join_right_side_is_a_bare_name_without_parens() {
        let df = sorted(
            run(
                make_join_vm(),
                "select price, bid from trades `sym lj quotes `sym",
            ),
            "price",
        );
        assert_eq!(i64s(&df, "price"), vec![10, 20, 30]);
        assert_eq!(opt_i64s(&df, "bid"), vec![Some(1), Some(1), Some(2)]);
    }

    #[test]
    fn join_right_side_rejects_a_table_expr_without_parens() {
        let tokens =
            tokenise("select price, bid from trades `sym lj distinct quotes `sym").expect("lex");
        assert!(parse(tokens).is_err());
    }

    #[test]
    fn join_right_side_accepts_a_parenthesised_table_expr() {
        let df = sorted(
            run(
                make_join_vm(),
                "select price, bid from trades `sym lj (select sym, bid from quotes where bid > 1) `sym",
            ),
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
        assert!(matches!(
            run_vm("l: lazy select from t", &mut vm),
            Ok(EvalResult::Stored)
        ));
        assert!(matches!(vm.globals.get("l"), Some(Value::Lazy(_))));
    }

    #[test]
    fn bare_lazy_expr_returns_a_plan() {
        let mut vm = make_vm();
        run_vm("l: lazy select from t", &mut vm).unwrap();
        assert!(matches!(
            run_vm("select c2 from l", &mut vm),
            Ok(EvalResult::Lazy(_))
        ));
    }

    #[test]
    fn collect_materialises_a_lazy_binding() {
        let mut vm = make_vm();
        run_vm("l: lazy select from t", &mut vm).unwrap();
        assert!(matches!(
            run_vm("m: collect l", &mut vm),
            Ok(EvalResult::Stored)
        ));
        assert!(matches!(vm.globals.get("m"), Some(Value::Table(_))));
        let df = run(vm, "select c2 from m");
        assert_eq!(i64s(&df, "c2"), vec![10, 20, 30, 15]);
    }

    #[test]
    fn update_assigned_back_to_a_lazy_binding_stays_lazy_and_extends_the_plan() {
        let mut vm = make_vm();
        run_vm("l: lazy select from t", &mut vm).unwrap();
        // explicit re-assignment is the only way to extend a lazy plan
        assert!(matches!(
            run_vm("l: update c2: c2 * 2 from l", &mut vm),
            Ok(EvalResult::Stored)
        ));
        assert!(matches!(vm.globals.get("l"), Some(Value::Lazy(_))));
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
        ]
        .unwrap();
        let mut vm = Vm::new();
        vm.globals.insert("t".into(), Value::Table(df));
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
    fn negative_limit_takes_last_n_rows() {
        let df = run(make_vm(), "-2 limit t");
        assert_eq!(i64s(&df, "c2"), vec![30, 15]);
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
        ]
        .unwrap();
        let mut vm = Vm::new();
        vm.globals.insert("t".into(), Value::Table(df));
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
        let df = run(
            win_vm(),
            "select rk: rank over `grp order `v asc, dr: drank over `grp order `v asc from t",
        );
        // x: v=[10,10,20] -> rank 1,1,3 / dense 1,1,2 ; y: [5,7] -> 1,2 / 1,2
        assert_eq!(i64s(&df, "rk"), vec![1, 1, 3, 1, 2]);
        assert_eq!(i64s(&df, "dr"), vec![1, 1, 2, 1, 2]);
    }

    #[test]
    fn window_rn_with_mixed_direction_multi_key_order() {
        let df = run(
            win_vm(),
            "select r: rn over `grp order `k asc `v desc from t",
        );
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

    // --- natives / verbs in value context ---

    fn scalar(vm: &mut Vm, src: &str) -> Value {
        match run_vm(src, vm).expect("run") {
            EvalResult::Scalar(v) => v,
            other => panic!("expected a scalar, got {other:?}"),
        }
    }

    #[test]
    fn take_and_index_on_a_plain_list() {
        let mut vm = Vm::new();
        run_vm("v: 10 20 30 40", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "2#v"), ast::int_vec(vec![10, 20]));
        assert_eq!(scalar(&mut vm, "-2#v"), ast::int_vec(vec![30, 40]));
        assert_eq!(scalar(&mut vm, "v[1]"), Value::Int(20));
        assert_eq!(scalar(&mut vm, "v[1 3]"), ast::int_vec(vec![20, 40]));
    }

    #[test]
    fn take_count_must_be_an_int() {
        let mut vm = Vm::new();
        run_vm("v: 1 2 3", &mut vm).unwrap();
        let err = run_vm("1.5#v", &mut vm).unwrap_err();
        assert!(err.to_string().contains("take count must be an int"));
    }

    #[test]
    fn index_out_of_range_is_a_runtime_error() {
        let mut vm = Vm::new();
        run_vm("v: 1 2 3", &mut vm).unwrap();
        assert!(run_vm("v[10]", &mut vm).is_err());
    }

    #[test]
    fn index_of_a_bare_name_calls_when_it_is_a_function() {
        let mut vm = Vm::new();
        run_vm("sq: {[x] x*x}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "sq[5]"), Value::Int(25));
    }

    #[test]
    fn table_column_expression_collapses_to_a_list_via_column() {
        let df = df!["price" => [1.0f64, 2.0, 3.0]].unwrap();
        let mut vm = Vm::new();
        vm.globals.insert("t".into(), Value::Table(df));
        assert_eq!(
            scalar(&mut vm, "t`price"),
            ast::float_vec(vec![1.0, 2.0, 3.0])
        );
    }

    #[test]
    fn zip_builds_a_table_and_rejects_a_non_dict() {
        let mut vm = Vm::new();
        let result = run_vm("zip `a`b!(1 2) (3 4)", &mut vm).unwrap();
        match result {
            EvalResult::Table(df) => {
                assert_eq!(i64s(&df, "a"), vec![1, 2]);
                assert_eq!(i64s(&df, "b"), vec![3, 4]);
            }
            other => panic!("expected a table, got {other:?}"),
        }
        assert!(run_vm("zip 5", &mut Vm::new()).is_err());
    }

    #[test]
    fn list_where_filters_a_list_and_rejects_a_non_list() {
        let mut vm = Vm::new();
        run_vm("v: 10 20 30 40", &mut vm).unwrap();
        assert_eq!(
            scalar(&mut vm, "v where x>15"),
            ast::int_vec(vec![20, 30, 40])
        );
        let err = run_vm("5 where x>1", &mut Vm::new()).unwrap_err();
        assert!(err.to_string().contains("'where' needs a list on the left"));
    }

    #[test]
    fn a_user_function_named_like_a_verb_wins_over_the_builtin() {
        // `sum` is an ordinary column verb, but a user-defined function of
        // the same name shadows it.
        let mut vm = Vm::new();
        run_vm("sum: {[x] 999}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "sum 1 2 3"), Value::Int(999));
    }

    #[test]
    fn a_user_function_named_til_wins_over_the_native() {
        let mut vm = Vm::new();
        run_vm("til: {[x] 999}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "til 5"), Value::Int(999));
    }

    #[test]
    fn a_verb_not_shadowed_still_works() {
        let mut vm = Vm::new();
        assert_eq!(scalar(&mut vm, "til 3"), ast::int_vec(vec![0, 1, 2]));
        assert_eq!(scalar(&mut vm, "sum 1 2 3"), Value::Int(6));
    }

    #[test]
    fn enlist_cannot_be_shadowed_by_a_same_named_function() {
        // `enlist`/`?` are called by id, so a same-named user function has no
        // effect (unlike `til`/`sum`)
        let mut vm = Vm::new();
        run_vm("enlist: {[x] 999}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "enlist 5"), ast::int_vec(vec![5]));
    }

    #[test]
    fn log_compiles_at_every_arity_in_bracket_form() {
        // every arity takes the same `Op::Call` path
        let mut vm = Vm::new();
        assert_eq!(scalar(&mut vm, "log[]"), Value::Str("".into()));
        assert_eq!(scalar(&mut vm, r#"log["a"]"#), Value::Str("a".into()));
        assert_eq!(
            scalar(&mut vm, r#"log["a";"b";"c"]"#),
            Value::Str("abc".into())
        );
    }

    #[test]
    fn a_user_function_named_log_wins_over_the_native() {
        // `log` resolves by name, so a user closure of that name wins at every
        // arity
        let mut vm = Vm::new();
        run_vm("log: {[x] 999}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, r#"log["hi"]"#), Value::Int(999));
    }

    #[test]
    fn dot_qpl_dt_cannot_be_reassigned() {
        // `.qpl.*` builtins are found before user bindings, and assigning to
        // one is rejected
        let mut vm = Vm::new();
        let err = run_vm(".qpl.dt: 5", &mut vm).unwrap_err().to_string();
        assert!(
            err.contains("'.qpl.dt' is a built-in and cannot be reassigned"),
            "{err}"
        );
    }
}
// Value-context behaviour (bare names, calls, casts, `zip`, `where`,
// closures, `while`/`?[..]`, interrupts, ...).
#[cfg(test)]
mod value_context_tests {
    use super::*;

    /// The eager table bound to `name` (panics otherwise).
    fn table<'a>(vm: &'a Vm, name: &str) -> &'a DataFrame {
        match vm.globals.get(name) {
            Some(Value::Table(df)) => df,
            other => panic!("'{name}' is not a table binding: {other:?}"),
        }
    }

    fn make_vm() -> Vm {
        let df = df![
            "c1" => ["a", "b", "a", "c"],
            "c2" => [10i64, 20, 30, 15],
            "c3" => [1.0f64, 2.0, 3.0, 4.0],
        ]
        .unwrap();
        let mut vm = Vm::new();
        vm.globals.insert("t".into(), Value::Table(df));
        vm
    }

    fn scalar(vm: &mut Vm, src: &str) -> Value {
        match run_vm(src, vm).expect("run") {
            EvalResult::Scalar(v) => v,
            other => panic!(
                "expected scalar, got a different result kind: {}",
                kind(&other)
            ),
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
        assert_eq!(
            scalar(&mut vm, r#"("string1" "string2")"#),
            ast::str_vec(vec!["string1".into(), "string2".into()])
        );
        assert_eq!(
            scalar(&mut vm, r#""a" "b" "c""#),
            ast::str_vec(vec!["a".into(), "b".into(), "c".into()])
        );
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
        assert_eq!(
            scalar(&mut vm, r#"enlist "hello""#),
            ast::str_vec(vec!["hello".into()])
        );
        assert_eq!(
            scalar(&mut vm, "enlist 2024.03.15"),
            ast::date_vec(vec![8840])
        );
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
            let got: Vec<i64> = v
                .as_vec()
                .unwrap()
                .1
                .i64()
                .unwrap()
                .into_no_null_iter()
                .collect();
            assert_eq!(got.len(), 50);
            assert!(got.iter().all(|n| (0..6).contains(n)), "{got:?}");
        }
    }

    #[test]
    fn roll_actually_varies() {
        let mut vm = make_vm();
        let v = scalar(&mut vm, "200?1000000");
        let got: Vec<i64> = v
            .as_vec()
            .unwrap()
            .1
            .i64()
            .unwrap()
            .into_no_null_iter()
            .collect();
        let distinct: std::collections::HashSet<_> = got.iter().collect();
        assert!(
            distinct.len() > 150,
            "200 draws from 1e6 had only {} distinct values",
            distinct.len()
        );
    }

    #[test]
    fn roll_from_a_list_picks_its_elements_with_replacement() {
        let mut vm = make_vm();
        let v = scalar(&mut vm, "2 ? 10 20 30 40");
        assert_eq!(v.as_vec().unwrap().1.len(), 2);
        let v = scalar(&mut vm, "30 ? 10 20");
        let got: Vec<i64> = v
            .as_vec()
            .unwrap()
            .1
            .i64()
            .unwrap()
            .into_no_null_iter()
            .collect();
        assert_eq!(got.len(), 30, "more draws than elements: replacement");
        assert!(got.iter().all(|n| *n == 10 || *n == 20));
        // works for any list kind, and for a column
        let v = scalar(&mut vm, "20 ? `a`b`c");
        assert!(
            matches!(v, Value::SymVec(_))
                && strs(&v)
                    .iter()
                    .all(|s| ["a", "b", "c"].contains(&s.as_str()))
        );
        let v = scalar(&mut vm, r#"20 ? ("x" "y")"#);
        assert!(matches!(v, Value::StrVec(_)) && strs(&v).iter().all(|s| s == "x" || s == "y"));
        let v = scalar(&mut vm, "20 ? t`c2");
        assert!(
            v.as_vec()
                .unwrap()
                .1
                .i64()
                .unwrap()
                .into_no_null_iter()
                .all(|n| [10, 20, 30, 15].contains(&n))
        );
    }

    #[test]
    fn roll_floats_and_zero_count() {
        let mut vm = make_vm();
        let v = scalar(&mut vm, "50?2.5");
        let got: Vec<f64> = v
            .as_vec()
            .unwrap()
            .1
            .f64()
            .unwrap()
            .into_no_null_iter()
            .collect();
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
        scalar_or_stored(
            &mut vm,
            "ts: `timestamp$1700000000000000000 1700086400123456789",
        );
        scalar_or_stored(
            &mut vm,
            "t2: zip `ts`day`tm`span!(ts) (`date$ts) (`time$ts) (`timespan$5 6)",
        );
        let df = table(&vm, "t2");
        let dtypes: Vec<_> = df.dtypes().iter().map(|d| d.to_string()).collect();
        assert_eq!(dtypes, ["datetime[ns]", "date", "time", "duration[ns]"]);
        assert_eq!(
            df.null_count()
                .sum_horizontal(NullStrategy::Ignore)
                .unwrap()
                .unwrap()
                .u32()
                .unwrap()
                .get(0),
            Some(0)
        );
        // and the values survive the round trip through the column
        assert_eq!(scalar(&mut vm, "t2`day"), scalar(&mut vm, "`date$ts"));
    }

    #[test]
    fn zip_applies_a_top_level_cast_to_the_column_keeping_its_dtype() {
        let mut vm = make_vm();
        scalar_or_stored(
            &mut vm,
            "t3: zip `a`b`c`d`e!(i8$10 20 30) (i32$1 2 3) (f32$3 ? 1.0) (`$`x`y`x) (`timestamp$1700000000000000000 1700000000000000001 1700000000000000002)",
        );
        let dtypes: Vec<_> = table(&vm, "t3")
            .dtypes()
            .iter()
            .map(|d| d.to_string())
            .collect();
        assert_eq!(dtypes, ["i8", "i32", "f32", "cat", "datetime[ns]"]);
        // an operand may itself be a roll
        scalar_or_stored(&mut vm, "t4: zip `a!(i8$5 ? 100)");
        assert_eq!(table(&vm, "t4").dtypes()[0].to_string(), "i8");
        assert_eq!(table(&vm, "t4").height(), 5);
    }

    #[test]
    fn table_col_materialises_to_a_list() {
        assert_eq!(
            scalar(&mut make_vm(), "t`c3"),
            ast::float_vec(vec![1.0, 2.0, 3.0, 4.0])
        );
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
        // a doubled `;;` between statements is an empty statement
        let mut vm = make_vm();
        run_vm("sq: {[x] tmp: x*x;; tmp}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "sq[9]"), Value::Int(81));
    }

    #[test]
    fn a_callee_cannot_see_its_caller_s_locals() {
        // lexical scoping: `callee` resolves `a` to the global (99), not
        // `caller`'s param, although `caller` is still on the stack
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
    fn recursion_to_depth_127_succeeds_128_is_the_cap() {
        // calls and `?[..]` don't recurse in Rust, so this runs on the default
        // stack: 128 activations succeed and the 129th hits `MAX_CALL_DEPTH`
        let mut vm = make_vm();
        run_vm("count: {[n] ?[n=0;0;1+count[n-1]]}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "count[127]"), Value::Int(127));
        assert!(vm.fp.is_none());
        let err = run_vm("count[128]", &mut vm);
        assert!(err.is_err());
        assert!(
            err.unwrap_err()
                .to_string()
                .contains("function recursion too deep (limit 128)")
        );
        assert!(vm.fp.is_none());
    }

    #[test]
    fn a_while_loop_of_100k_iterations_does_not_grow_the_rust_or_vm_stack() {
        // a `while` loop grows neither `Vm::stack` nor the Rust stack (default
        // stack, no custom thread)
        let mut vm = make_vm();
        run_stored(&mut vm, "n: 0");
        let base_depth = vm.stack.len();
        run_stored(&mut vm, "while[n<100000; n: n+1]");
        assert_eq!(scalar(&mut vm, "n"), Value::Int(100000));
        assert_eq!(
            vm.stack.len(),
            base_depth,
            "the VM stack must return to its starting height after the loop"
        );
    }

    #[test]
    fn a_nested_lambda_can_be_called_from_inside_its_defining_body() {
        // a lambda invoked where it's defined
        let mut vm = make_vm();
        run_vm("f: {[x] g: {[y] y*2}; g[x] + 1}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "f[10]"), Value::Int(21));
    }

    #[test]
    fn a_niladic_function_is_usable_inside_a_select_column() {
        // `LOAD_COL` calls a niladic function to completion and lifts its
        // scalar result into the column expression
        let mut vm = make_vm();
        run_vm("five: {[] 5}", &mut vm).unwrap();
        match run_vm("select c2, plus5: c2 + five from t", &mut vm).expect("run") {
            EvalResult::Table(df) => {
                let got: Vec<i64> = df
                    .column("plus5")
                    .unwrap()
                    .i64()
                    .unwrap()
                    .into_no_null_iter()
                    .collect();
                assert_eq!(got, vec![15, 25, 35, 20]);
            }
            other => panic!("expected a table, got {}", kind(&other)),
        }
        // a function returning a table gets a clear error
        run_vm("q: {[] select from t}", &mut vm).unwrap();
        let err = run_vm("select a: q from t", &mut vm)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("'q' returns a table — it can't be used inside a column expression"),
            "{err}"
        );
    }

    #[test]
    fn a_user_defined_zip_wins_over_the_builtin_dict_constructor() {
        // a dict-literal `zip` checks for a user `zip` at run time; a non-dict
        // argument (`zip[5]`) takes the ordinary call path
        let mut vm = make_vm();
        run_vm("zip: {[x] x+1}", &mut vm).unwrap();
        assert_eq!(scalar(&mut vm, "zip[5]"), Value::Int(6));
        let err = run_vm("zip `a`b!(1 2) (3 4)", &mut vm)
            .unwrap_err()
            .to_string();
        assert!(err.contains("shadowed"), "{err}");
    }

    #[test]
    fn a_function_can_return_a_table() {
        let mut vm = make_vm();
        run_vm("q: {[k] select c2 from t where c1 = k}", &mut vm).unwrap();
        match run_vm("q[`a]", &mut vm).expect("run") {
            EvalResult::Table(df) => {
                let got: Vec<i64> = df
                    .column("c2")
                    .unwrap()
                    .i64()
                    .unwrap()
                    .into_no_null_iter()
                    .collect();
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
        // run on an explicitly sized thread for headroom at MAX_CALL_DEPTH
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(|| {
                let mut vm = make_vm();
                run_vm("loop: {[n] loop[n+1]}", &mut vm).unwrap();
                let err = run_vm("loop[0]", &mut vm);
                assert!(err.is_err());
                // every call frame was popped on the way out through the error
                assert!(vm.fp.is_none());
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
        assert!(vm.fp.is_none());
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
        // ... and a bound name
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
        // no capture: `f` resolves `n` against globals, not `outer`'s frame
        let mut vm = make_vm();
        run_vm("outer: {[n] apply[{[y] y+n}; 1]}", &mut vm).unwrap();
        run_vm("apply: {[f,x] f[x]}", &mut vm).unwrap();
        assert!(run_vm("outer[10]", &mut vm).is_err());
        assert!(vm.fp.is_none());
    }

    #[test]
    fn applying_a_non_function_value_is_an_error() {
        let mut vm = make_vm();
        run_vm("notafn: 3", &mut vm).unwrap();
        assert!(run_vm("notafn[1]", &mut vm).is_err());
        assert!(run_vm("apply: {[f,x] f[x]}", &mut vm).is_ok());
        assert!(run_vm("apply[3; 1]", &mut vm).is_err());
        assert!(vm.fp.is_none());
    }

    #[test]
    fn a_function_value_cannot_be_used_as_a_column() {
        let mut vm = make_vm();
        run_vm("myfn: {[x] x+1}", &mut vm).unwrap();
        let err = run_vm("select a: myfn from t", &mut vm).unwrap_err();
        assert!(
            format!("{err:?}").contains("cannot be used in a query expression"),
            "{err:?}"
        );
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
        assert!(matches!(
            run_vm("px: select c2 from t", &mut vm),
            Ok(EvalResult::Stored)
        ));
        assert_eq!(
            vm.globals.get("px"),
            Some(&ast::int_vec(vec![10, 20, 30, 15]))
        );
    }

    #[test]
    fn bare_one_column_select_still_prints_a_table() {
        assert!(matches!(
            run_vm("select c2 from t", &mut make_vm()),
            Ok(EvalResult::Table(_))
        ));
    }

    #[test]
    fn where_filters_a_column_expression() {
        assert_eq!(
            scalar(&mut make_vm(), "t`c2 where c2 > 15"),
            ast::int_vec(vec![20, 30])
        );
    }

    #[test]
    fn list_where_filters_a_bare_list_by_its_own_elements() {
        let mut vm = make_vm();
        run_vm("l: 10 20 30 40 50", &mut vm).unwrap();
        assert_eq!(
            scalar(&mut vm, "l where x > 25"),
            ast::int_vec(vec![30, 40, 50])
        );
    }

    #[test]
    fn list_where_supports_comma_separated_predicates() {
        let mut vm = make_vm();
        run_vm("l: 10 20 30 40 50", &mut vm).unwrap();
        assert_eq!(
            scalar(&mut vm, "l where x > 10, x < 50"),
            ast::int_vec(vec![20, 30, 40])
        );
    }

    // `` t`c2 where <pred> `` is the table row-filter form, so list-where only
    // applies to an operand it didn't consume; parenthesising the result
    // makes it a plain noun for list-where.
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
        assert!(matches!(
            run_vm("m: max t`c2", &mut vm),
            Ok(EvalResult::Stored)
        ));
        assert_eq!(vm.globals.get("m"), Some(&Value::Int(30)));
    }

    #[test]
    fn reduction_over_a_one_column_select() {
        assert_eq!(
            scalar(&mut make_vm(), "first select c1 from t"),
            Value::Str("a".into())
        );
    }

    #[test]
    fn reducer_ignores_nulls_in_the_source_column() {
        // a reducer over `` t`col `` runs inside the lazy plan, so nulls in the
        // column are skipped rather than rejected
        let mut vm = make_vm();
        let df = df!["n" => [Some(10i64), None, Some(30)]].unwrap();
        vm.globals.insert("nt".into(), Value::Table(df));
        assert_eq!(scalar(&mut vm, "max nt`n"), Value::Int(30));
        // an all-null column still errors: the reduction result is itself null
        let df_all_null = df!["n" => [None::<i64>, None]].unwrap();
        vm.globals
            .insert("allnull".into(), Value::Table(df_all_null));
        assert!(run_vm("max allnull`n", &mut vm).is_err());
    }

    #[test]
    fn cast_then_reduce_a_column_expression() {
        // a cast on a column expression works under a reducer
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
        assert_eq!(
            scalar(&mut make_vm(), "-2#t`c2"),
            ast::int_vec(vec![30, 15])
        );
    }

    #[test]
    fn take_count_can_be_a_bound_global() {
        // a variable take count
        let mut vm = make_vm();
        run_vm("k: 2", &mut vm).expect("run");
        assert_eq!(scalar(&mut vm, "k#t`c2"), ast::int_vec(vec![10, 20]));
        assert_eq!(scalar(&mut vm, "-k#t`c2"), ast::int_vec(vec![30, 15]));
        assert!(matches!(run_vm("k#t", &mut vm), Ok(EvalResult::Table(_))));
        assert!(matches!(
            run_vm("(k+1)#t", &mut vm),
            Ok(EvalResult::Table(_))
        ));
    }

    #[test]
    fn positional_index_with_an_int_run() {
        assert_eq!(
            scalar(&mut make_vm(), "(t`c2) 0 3"),
            ast::int_vec(vec![10, 15])
        );
    }

    #[test]
    fn bracket_index_atom_vs_slice() {
        let mut vm = make_vm();
        assert!(matches!(
            run_vm("l: 5 6 7 8 9", &mut vm),
            Ok(EvalResult::Stored)
        ));
        // a single int picks an atom; an int run picks a sub-list
        assert_eq!(scalar(&mut vm, "l[0]"), Value::Int(5));
        assert_eq!(scalar(&mut vm, "l[1 3 4]"), ast::int_vec(vec![6, 8, 9]));
        // works directly on a column expression, and chains
        assert_eq!(scalar(&mut vm, "t`c2[2 1]"), ast::int_vec(vec![30, 20]));
        assert_eq!(scalar(&mut vm, "(t`c2)[0]"), Value::Int(10));
    }

    #[test]
    fn bare_table_name_prints_as_a_table() {
        assert!(matches!(
            run_vm("t", &mut make_vm()),
            Ok(EvalResult::Table(_))
        ));
    }

    #[test]
    fn assigning_a_bare_table_name_copies_it() {
        let mut vm = make_vm();
        assert!(matches!(run_vm("t2: t", &mut vm), Ok(EvalResult::Stored)));
        assert!(matches!(vm.globals.get("t2"), Some(Value::Table(_))));
    }

    #[test]
    fn take_over_a_bare_table_stays_a_table() {
        assert!(matches!(
            run_vm("2#t", &mut make_vm()),
            Ok(EvalResult::Table(_))
        ));
    }

    #[test]
    fn out_of_range_index_errors() {
        assert!(run_vm("(t`c2) 9", &mut make_vm()).is_err());
    }

    #[test]
    fn column_to_value_rejects_nulls() {
        let s = Series::new("x".into(), &[Some(1i64), None, Some(3)]);
        let col = Column::from(s);
        assert!(crate::ops::column_to_value(&col).is_err());
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
        run_stored(
            &mut vm,
            "while[k<2; t: select from t where c2 > 10; k: k+1]",
        );
        assert_eq!(table(&vm, "t").height(), 3);
    }

    #[test]
    fn while_inside_a_function_binds_locals_only() {
        let mut vm = make_vm();
        run_stored(&mut vm, "s: 100");
        run_stored(&mut vm, "g: {[n] s: 0; while[n>0; s: s+n; n: n-1]; s}");
        assert_eq!(scalar(&mut vm, "g[4]"), Value::Int(10));
        assert_eq!(
            scalar(&mut vm, "s"),
            Value::Int(100),
            "the global is untouched"
        );
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
            assert_eq!(
                run_err(&mut vm, src),
                "'cannot assign a no-op expression.",
                "{src}"
            );
        }
        assert!(!vm.globals.contains_key("x"));
    }

    #[test]
    fn a_noop_cannot_be_an_operand() {
        let mut vm = make_vm();
        run_stored(&mut vm, "g: {[a] a}");
        run_stored(&mut vm, "f: {[] noop}");
        for src in ["1 + noop", "g[noop]", "sum f[]"] {
            assert!(
                run_err(&mut vm, src).contains("no-op expression as a value"),
                "{src}"
            );
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
            let err = run_vm("while[1b; x: x+1]", &mut vm).expect_err("should be interrupted");
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
        assert!(matches!(
            run_vm("f[0]", &mut vm),
            Err(QplError::Interrupted)
        ));
        let _running = vm.interrupt.statement(); // a new statement clears the stale request
        assert_eq!(scalar(&mut vm, "1+1"), Value::Int(2));
    }

    #[test]
    fn nested_whiles_reset_their_inner_counter() {
        let mut vm = make_vm();
        run_stored(&mut vm, "a: 0");
        run_stored(&mut vm, "n: 0");
        run_stored(
            &mut vm,
            "while[a<3; b: 0; while[b<2; b: b+1; n: n+1]; a: a+1]",
        );
        assert_eq!(scalar(&mut vm, "a"), Value::Int(3));
        assert_eq!(scalar(&mut vm, "b"), Value::Int(2));
        assert_eq!(
            scalar(&mut vm, "n"),
            Value::Int(6),
            "inner body ran 3 x 2 times"
        );
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
        assert_eq!(
            scalar(&mut vm, "c"),
            Value::Int(1),
            "the first iteration got as far as `boom[]`"
        );
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
        assert_eq!(
            run_err(&mut vm, "y: f[3]"),
            "'cannot assign a no-op expression."
        );
    }

    #[test]
    fn a_while_in_a_function_reads_globals_but_leaves_them_alone() {
        let mut vm = make_vm();
        run_stored(&mut vm, "lim: 3");
        run_stored(&mut vm, "f: {[] c: 0; while[c<lim; c: c+1]; c}");
        assert_eq!(
            scalar(&mut vm, "f[]"),
            Value::Int(3),
            "the test reads the global"
        );
        // assigning the same name in the body binds a *local* that then shadows it
        run_stored(&mut vm, "g: {[] c: 0; while[c<lim; c: c+1; lim: 5]; c}");
        assert_eq!(scalar(&mut vm, "g[]"), Value::Int(5));
        assert_eq!(
            scalar(&mut vm, "lim"),
            Value::Int(3),
            "the global is untouched"
        );
    }

    #[test]
    fn a_noop_argument_to_a_call_is_an_error_for_every_kind_of_callee() {
        let mut vm = make_vm();
        run_stored(&mut vm, "id: {[a] a}");
        run_stored(&mut vm, "nil: {[] noop}");
        for src in [
            "id[noop]",
            "id[nil[]]",
            "sum noop",
            "count noop",
            "{[a] a}[noop]",
        ] {
            assert!(run_err(&mut vm, src).contains("no-op expression"), "{src}");
        }
    }

    #[test]
    fn a_noop_cannot_index_take_or_cast() {
        let mut vm = make_vm();
        for src in ["3#noop", "f64$noop", "noop[0]"] {
            let err = run_err(&mut vm, src);
            assert!(
                err.contains("no-op") || err.contains("noop"),
                "{src}: {err}"
            );
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
        assert!(matches!(
            run_vm("select from t", &mut vm),
            Err(QplError::Interrupted)
        ));
        assert!(matches!(
            run_vm("x: 1", &mut vm),
            Err(QplError::Interrupted)
        ));
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
            assert!(matches!(
                run_vm("g[0]", &mut vm),
                Err(QplError::Interrupted)
            ));
        }
        t.join().unwrap();
        assert!(
            vm.fp.is_none(),
            "every call frame was popped on the way out"
        );
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
        assert_eq!(
            scalar(&mut vm, "?[1011b; 1; 0]"),
            ast::int_vec(vec![1, 0, 1, 1])
        );
        assert_eq!(
            scalar(&mut vm, "?[1011b; 1 2 3 4; 5 6 7 8]"),
            ast::int_vec(vec![1, 6, 3, 4])
        );
        assert_eq!(
            scalar(&mut vm, r#"?[1011b; "yes"; "no"]"#),
            ast::str_vec(vec!["yes".into(), "no".into(), "yes".into(), "yes".into()])
        );
    }

    #[test]
    fn a_vector_condition_may_come_from_a_variable_or_a_comparison() {
        let mut vm = make_vm();
        run_stored(&mut vm, "x: 5 6 7 8");
        assert_eq!(
            scalar(&mut vm, "?[x>6; x; 0]"),
            ast::int_vec(vec![0, 0, 7, 8])
        );
        run_stored(&mut vm, "f: {[m] ?[m; 1; 0]}");
        assert_eq!(scalar(&mut vm, "f[10b]"), ast::int_vec(vec![1, 0]));
    }

    #[test]
    fn a_vector_branch_must_match_the_condition_length() {
        let mut vm = make_vm();
        for src in [
            "?[1011b; 1 2 3; 0]",
            "?[1011b; 1; 5 6 7 8 9]",
            "?[10b; 1 2 3 4; 0]",
        ] {
            let err = run_err(&mut vm, src);
            assert!(
                err.contains("has length") && err.contains("length of the condition"),
                "{src}: {err}"
            );
        }
    }

    #[test]
    fn a_chained_vector_conditional_takes_the_first_true_condition_per_element() {
        let mut vm = make_vm();
        assert_eq!(
            scalar(&mut vm, "?[1011b; 1; 0110b; 2; 3]"),
            ast::int_vec(vec![1, 2, 1, 1])
        );
        assert_eq!(
            scalar(&mut vm, "?[0110b; 1; 0011b; 2; 3]"),
            ast::int_vec(vec![3, 1, 1, 2])
        );
        // every later condition must be boolean and as long as the first
        assert!(run_err(&mut vm, "?[1011b; 1; 01b; 2; 3]").contains("condition has length 2"));
        assert!(run_err(&mut vm, "?[1011b; 1; 1; 2; 3]").contains("boolean scalar or vector"));
    }

    #[test]
    fn an_atom_condition_after_a_vector_one_is_broadcast() {
        let mut vm = make_vm();
        assert_eq!(
            scalar(&mut vm, "?[1011b; 1; 1b; 2; 3]"),
            ast::int_vec(vec![1, 2, 1, 1])
        );
        assert_eq!(
            scalar(&mut vm, "?[1011b; 1; 0b; 2; 3]"),
            ast::int_vec(vec![1, 3, 1, 1])
        );
    }

    #[test]
    fn atom_conditions_before_a_vector_one_still_short_circuit() {
        let mut vm = make_vm();
        assert_eq!(
            scalar(&mut vm, "?[0b; undefined_fn[1]; 1011b; 5; 6]"),
            ast::int_vec(vec![5, 6, 5, 5])
        );
        // a true atom returns its branch as-is, whatever its length
        assert_eq!(
            scalar(&mut vm, "?[1b; 1 2 3; 1011b; 5; 6]"),
            ast::int_vec(vec![1, 2, 3])
        );
    }

    #[test]
    fn a_vector_conditional_keeps_symbols_symbols_and_rejects_a_text_number_mix() {
        let mut vm = make_vm();
        assert_eq!(
            scalar(&mut vm, "?[1011b; `a`b`c`d; `z]"),
            ast::sym_vec(vec!["a".into(), "z".into(), "c".into(), "d".into()])
        );
        assert!(run_err(&mut vm, r#"?[1011b; 1; "a"]"#).contains("mix text and non-text"));
        assert_eq!(
            scalar(&mut vm, "?[1011b; 1; 2.5]"),
            ast::float_vec(vec![1.0, 2.5, 1.0, 1.0])
        );
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
