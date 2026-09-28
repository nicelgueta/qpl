//! Bytecode program representation.
//!
//! A [`Program`] is a compiled statement: one byte per instruction in `code`,
//! decoded via [`Op::try_from`], plus a side `operands` stream that only
//! [`Op::Push`] reads. Every other opcode takes all of its inputs from the
//! VM's value stack — see `vm.rs`'s `run_compiled` for the interpreter loop.
//!
//! [`Program::to_bytes`]/[`Program::from_bytes`] serialise this to and from a `.qplc` file: opcode/operand/`Value`
//! tags are part of that on-disk format, so changing, reordering or removing
//! one requires bumping [`FORMAT_VERSION`].
//!
//! `lines` (ip -> source line, for error messages) is populated by whole-
//! program compilation (`compiler::compile_program`).

use crate::ast::{self, CastTarget, Value};
use crate::errors::QplError;
use crate::native::NativeId;
use polars::prelude::JoinType;
use std::fmt;
use std::sync::Arc;

/// A function literal's compile-time shape: parameter names, its entry point
/// `(ip, cp)` inside the enclosing [`Program`] (the body is appended after the
/// program's main code), and the display text (`{[x,y] ..}`) used by
/// [`Closure`]'s `Debug` impl and by error messages. Carried by
/// [`Operand::Func`]; turned into a runtime [`Closure`] by `Op::Push`, which
/// attaches the currently-running `Arc<Program>`.
#[derive(Debug, Clone)]
pub struct FuncProto {
    pub params: Vec<String>,
    pub entry: (u32, u32),
    pub display: String,
}

/// A runtime function value: [`FuncProto`]
/// plus the `Arc<Program>` its body lives in, so a closure created on one
/// REPL line is still callable — its own program stays alive via this `Arc` —
/// once that line's `Program` would otherwise have been dropped. `CALL`
/// switches the VM's `prog` register to this when invoking it; `RET` switches
/// back. See `vm::Vm::begin_closure_call`.
pub struct Closure {
    pub params: Vec<String>,
    pub entry: (u32, u32),
    pub program: Arc<Program>,
    pub display: String,
}

/// Prints as the source shape (`{[x,y] ..}`). User-facing text (error messages,
/// `\d`, `fmt_val`) must never dump the compiled body.
impl fmt::Debug for Closure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display)
    }
}

/// A function literal queued for compilation after the enclosing statement's
/// main code:
/// `Program::pending_closures` holds these; `compiler::compile`/`finish_pending`
/// drains the queue (a body may itself queue further, nested, closures),
/// compiling each body in turn and patching its placeholder `Operand::Func`
/// (at `operand_index`, pushed with a dummy `(0,0)` entry when the literal
/// was first seen) with its real entry point once known.
#[derive(Debug)]
pub(crate) struct PendingClosure {
    pub operand_index: usize,
    pub params: Vec<String>,
    pub body: Vec<ast::Stmt>,
    pub display: String,
}

/// Which window computation [`Op::Window`] performs. This is the payload
/// of an `Operand::Window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFn {
    /// apply the popped aggregate/column expression per partition (`.over`)
    Over,
    /// `rn`    — SQL `row_number()`: strict 1..n ordinal in the window order
    RowNumber,
    /// `rank`  — SQL `rank()`: ties share the lowest rank, then a gap
    Rank,
    /// `drank` — SQL `dense_rank()`: ties share a rank, no gaps
    DenseRank,
}

/// The window-function payload pushed ahead of [`Op::Window`]: which function,
/// its partition/order clauses, and (for `<agg> <col> <n>!rolling over ...`)
/// the rolling aggregate name and window size.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub func: WindowFn,
    pub partition: Vec<String>,
    pub order: Vec<(String, bool)>,
    pub rolling: Option<(String, usize)>,
}

/// A binary operator, resolved once at compile time from the parser's raw
/// operator string. `Other` is a fallback for any string that doesn't match a
/// known operator — unreachable via the current grammar (the parser only ever
/// produces one of the known spellings), but kept so an operator string that
/// somehow isn't recognised still fails at the same point, with the same
/// "unknown operator" text, that `vm::apply_binop` produced before this enum
/// existed, rather than becoming a `panic!` or a silent compile error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinOpKind {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Like,
    Other(Arc<str>),
}

impl BinOpKind {
    pub fn from_op_str(op: &str) -> Self {
        match op {
            "+" => Self::Add,
            "-" => Self::Sub,
            "*" => Self::Mul,
            "%" => Self::Div,
            "=" => Self::Eq,
            "!=" | "<>" => Self::Neq,
            "<" => Self::Lt,
            "<=" => Self::Le,
            ">" => Self::Gt,
            ">=" => Self::Ge,
            "&" => Self::And,
            "|" => Self::Or,
            "like" => Self::Like,
            other => Self::Other(other.into()),
        }
    }

    /// The canonical operator spelling — the inverse of [`Self::from_op_str`]
    /// for every known operator (an alternate spelling that collapses to the
    /// same variant, e.g. `<>` / `!=` both -> `Neq`, comes back out as the
    /// canonical one). Used by the value-context `BINOP` opcode's eager scalar
    /// path (`ops::value_binop`), which dispatches on the operator
    /// string.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "%",
            Self::Eq => "=",
            Self::Neq => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::And => "&",
            Self::Or => "|",
            Self::Like => "like",
            Self::Other(s) => s,
        }
    }
}

/// A payload pushed onto the value stack by [`Op::Push`] — the only opcode
/// that reads the `operands` stream. Everything here is
/// cheap to clone: `Value` clones are `Arc`/`Series`-backed, everything else
/// is a `Copy` or an `Arc`.
#[derive(Clone)]
pub enum Operand {
    /// A literal value. [`Op::Push`] turns this into `Slot::Scalar`, not
    /// `Slot::Operand` — every other `Operand` variant becomes
    /// `Slot::Operand` for the next opcode to interpret.
    Value(Value),
    /// A variable / column / table name.
    Name(Arc<str>),
    /// An arity, list length, or other small count.
    Count(u32),
    /// A jump target / closure entry point: `(ip, cp)` into `Program::code` /
    /// `Program::operands`.
    /// `JUMP`/`JUMP_IF_FALSE`/`JUMP_IF_VEC`
    /// set *both* registers, so a loop re-reads its own operands each
    /// iteration and a skipped branch skips the operands it would have
    /// consumed. A forward jump's target is back-patched once the label's
    /// position is known (see `compiler::compile_while`/`compile_case_value`);
    /// a backward jump (a `while` looping to its top) already knows both
    /// numbers when it's emitted.
    Target {
        ip: u32,
        cp: u32,
    },
    BinOp(BinOpKind),
    /// A column verb name (`sum`, `avg`, `shift`, ...), resolved to
    /// [`crate::vm::apply_call`] / [`crate::vm::apply_dyadic`] at run time.
    Verb(Arc<str>),
    Join(JoinType),
    /// `order`/`sort`'s (column, descending) pairs.
    Sort(Arc<[(String, bool)]>),
    /// `drop` / `dropnull` / `update`'s column-name lists.
    Names(Arc<[String]>),
    Cast(CastTarget),
    Window(Arc<WindowSpec>),
    /// An unconditional value-context primitive (`enlist`, `?` roll) — see
    /// [`NativeId`]. `Op::Call`'s callee when the call can never be shadowed
    /// by a user function.
    Native(NativeId),
    /// Free text carried by an opcode that isn't a name/column/count — a
    /// `dispatch` payload (the rest of the statement, already rendered to
    /// source text by the parser).
    Text(Arc<str>),
    /// A function literal: `Op::Push` turns
    /// this into `Slot::Val(Value::Closure(..))` by attaching the current
    /// `Arc<Program>` — the only `Operand` variant `Op::Push` treats specially
    /// (every other non-`Value` variant becomes `Slot::Operand` verbatim).
    Func(Arc<FuncProto>),
    /// A `\l`/`\i` target, embedded at compile time: the target script is
    /// read, parsed and compiled into its own `Program` when the *including*
    /// script is compiled, so `load_script`/`import_script`
    /// ([`crate::native::NativeId::LoadScript`] / `ImportScript`) just run it
    /// at call time — no source, lexing or parsing left to do then. This is
    /// also what will let a `qpl -c`-compiled artifact run with no
    /// original source file present at all.
    Program(Arc<Program>),
}

impl fmt::Debug for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Value(v) => write!(f, "Value({v:?})"),
            Operand::Name(n) => write!(f, "Name({n})"),
            Operand::Count(n) => write!(f, "Count({n})"),
            Operand::Target { ip, cp } => write!(f, "Target(ip={ip},cp={cp})"),
            Operand::BinOp(k) => write!(f, "BinOp({k:?})"),
            Operand::Verb(v) => write!(f, "Verb({v})"),
            Operand::Join(k) => write!(f, "Join({k:?})"),
            Operand::Sort(s) => write!(f, "Sort({s:?})"),
            Operand::Names(n) => write!(f, "Names({n:?})"),
            Operand::Cast(c) => write!(f, "Cast({c:?})"),
            Operand::Window(w) => write!(f, "Window({w:?})"),
            Operand::Native(id) => write!(f, "Native({id:?})"),
            Operand::Text(t) => write!(f, "Text({t})"),
            Operand::Func(p) => write!(f, "Func({})", p.display),
            Operand::Program(p) => write!(f, "Program(<{} bytes>)", p.code.len()),
        }
    }
}

/// One bytecode instruction. `#[repr(u8)]` with explicit discriminants: a
/// `.qplc` files serialise these, so the numbering must never shift
/// silently — see the `opcode_discriminants_are_pinned` test below.
///
/// Only [`Op::Push`] reads the operand stream; every other opcode takes all
/// of its inputs from the value stack (mirrored in `vm.rs`'s `run_compiled`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Push = 0,
    Source = 1,
    LoadFile = 2,
    RowIndex = 3,
    LoadCol = 4,
    LoadRowIdx = 5,
    BinOp = 6,
    Round = 7,
    Window = 8,
    Case = 9,
    Alias = 10,
    Cast = 11,
    /// Retired: used to evaluate a pushed
    /// `Operand::Ast` node. Byte 12 is never reused —
    /// `Op::try_from` rejects it — so a stale `.qplc` or a corrupt
    /// stream fails cleanly instead of silently decoding as something else.
    Store = 13,
    Sink = 14,
    Lazy = 15,
    Collect = 16,
    Filter = 17,
    Sort = 18,
    Distinct = 19,
    DropNull = 20,
    Limit = 21,
    Drop = 22,
    Cols = 23,
    Join = 24,
    /// Builds a `Slot::List` from `n` popped exprs — replaces
    /// `Instruction::BuildKeys` / `Instruction::BuildProj`, which differed
    /// only in name, not behaviour.
    List = 25,
    Select = 26,
    SelectBy = 27,
    Update = 28,
    /// A column verb call (`sum price`, `n shift price`, ...) — replaces
    /// `Instruction::Call`.
    Verb = 29,
    /// Value context: resolves a bare name —
    /// a niladic closure/builtin is called, a `Table`/`Lazy` global becomes a
    /// `Frame`, everything else a `Scalar`. See `vm::Vm::resolve_plain`,
    /// whose semantics this opcode reuses exactly.
    Load = 30,
    /// Value context: `(f a1..an n → result)`. `f` is either
    /// `Operand::Native` (an unconditional keyword: `enlist`, `?` roll) or
    /// `Operand::Name` (resolved at run time — a bound closure/builtin wins,
    /// else a handful of shadowable keywords, else a generic column verb; see
    /// `ops::call_by_name`) or a `Scalar(Closure)` already on the stack (a
    /// closure literal applied in place, `{[y] y*2}[x]`).
    Call = 31,
    /// Value context: `(count x → x)` — `` n#expr `` head/tail
    /// slicing of a frame or list.
    Take = 32,
    /// Value context: `(target a1..an n → x)` — a callable target
    /// (from `Op::LoadFn`, or a closure literal) calls; otherwise positional
    /// list indexing.
    Index = 33,
    /// Value context: `(Frame → Scalar)` — a one-column `select`'s
    /// materialisation to a list (`compile_value_expr`'s `Expr::Table` arm).
    Column = 34,
    /// Value context: `(v1..vn names n → Frame)` — `` zip `k!v ``.
    Zip = 35,
    /// Value context: `(conn text async → x)` — `` <conn> dispatch <cmd> ``.
    /// `ipc` feature only; a non-ipc build gives today's error text.
    Dispatch = 36,
    /// Value context: `(name → Val)` — like `Load`, but a niladic
    /// closure/builtin is *not* auto-called (a call-target position: `f[x]`,
    /// `x[i]` where `x` might be callable).
    LoadFn = 37,
    /// Internal to `zip`'s compiled lowering only: `(list cast name
    /// → Column)` — applies a top-level cast to a `zip` column's list value,
    /// keeping its exact width/dtype (a plain value-context cast would
    /// round-trip through `ast::Value` and lose it — see `ops::zip_value_to_series`).
    CastList = 38,
    /// Value context: `(list → Frame)` — the list half of `<list>
    /// where <preds>`'s lowering: checks `list` is list-shaped (today's
    /// "'where' needs a list on the left, got …" error otherwise) and aliases
    /// it to a one-column `x` frame for the query-context predicates that follow.
    ListWhereFrame = 39,
    /// `(x → )` — discard the top of the stack. Used between a function
    /// body's non-final statements: an
    /// expression statement's value is popped and dropped; `STORE` already
    /// has zero net stack effect and needs no `POP`.
    Pop = 40,
    /// `(result → result)` — return from the innermost `CALL`: unwinds the
    /// stack to `fp`, restores the caller's `prog`/`ip`/`cp`/`fp`, and leaves
    /// `result` on top for the caller. See
    /// `vm::Vm::begin_closure_call` / the `Op::Ret` arm in `run_compiled`.
    Ret = 41,
    /// No-op that jumps `ip` straight to the end of `code`: emitted once,
    /// right after a statement's own code, whenever that statement defined at
    /// least one function literal — the appended bodies that follow are only
    /// ever reached via `CALL`'s explicit jump to their entry point, never by
    /// falling off the end of the statement that defined them. See
    /// `compiler::finish_pending`.
    Halt = 42,
    /// `(target -> )` -- unconditional jump: sets both `ip` and `cp` to the
    /// popped `Operand::Target`.
    /// Checks `interrupt` when the target is behind the current `ip` (a
    /// `while` looping back to its top) -- a forward jump never checks.
    Jump = 43,
    /// `(cond target msg -> )` -- pops a `Bool` atom `cond`; jumps to
    /// `target` if it's `false`, otherwise falls through. Any other `cond`
    /// (a non-boolean scalar, a `Frame`, `Noop`, ...) is a runtime error
    /// using the popped `Operand::Text` `msg` verbatim -- `while`'s and
    /// `?[..]`'s atom paths push their own exact error text
    /// here. A deviation from the plan's literal
    /// `(cond target -> )` signature: the error text differs between
    /// `while` and `?[..]`'s callers, so the message travels as a third
    /// popped operand rather than being baked into the opcode.
    JumpIfFalse = 44,
    /// `(cond target -> cond)` -- peeks (never pops) `cond`; jumps iff it's
    /// a `BoolVec`. Used by value-context `?[..]` to divert a
    /// vector condition to its elementwise `CASE_VEC` tail while leaving the
    /// mask on the stack for that tail to consume.
    JumpIfVec = 45,
    /// `(mask v1 c2 v2 ... d n -> Val)` -- the elementwise form of `?[..]`
    /// every remaining condition/branch/
    /// default has already been evaluated eagerly (they're ordinary compiled
    /// sub-expressions, not thunks) and sits on the stack; this opcode folds
    /// them into one `when/then/otherwise` chain and extracts the result.
    /// Same semantics, arm order and error text as the scalar `?[..]` path -- see `ops::case_vec`.
    CaseVec = 46,
    /// `( -> Noop)` -- pushes `Slot::Noop`: `noop`'s value, and a `while`
    /// statement's own result once its loop exits.
    Noop = 47,
    /// `(x -> )` -- print a top-level expression statement's result as the
    /// REPL does: a lazy `Frame` prints its plan, an
    /// eager `Frame` collects and prints the table (or is captured into
    /// `Vm::last_table` under `Vm::capture_table`), a `Scalar` prints via
    /// `fmt_val`, and `Noop` prints nothing. Every top-level statement in a
    /// `Script`-mode program ends with this, `STORE`, or `POP`.
    Emit = 48,
}

impl TryFrom<u8> for Op {
    type Error = QplError;

    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        use Op::*;
        Ok(match byte {
            0 => Push,
            1 => Source,
            2 => LoadFile,
            3 => RowIndex,
            4 => LoadCol,
            5 => LoadRowIdx,
            6 => BinOp,
            7 => Round,
            8 => Window,
            9 => Case,
            10 => Alias,
            11 => Cast,
            // 12 was `Eval`, retired (see the variant's old doc
            // comment, kept as a comment on `Store` above) — never reused,
            // so a stale `.qplc` fails cleanly instead of silently
            // decoding as something else.
            13 => Store,
            14 => Sink,
            15 => Lazy,
            16 => Collect,
            17 => Filter,
            18 => Sort,
            19 => Distinct,
            20 => DropNull,
            21 => Limit,
            22 => Drop,
            23 => Cols,
            24 => Join,
            25 => List,
            26 => Select,
            27 => SelectBy,
            28 => Update,
            29 => Verb,
            30 => Load,
            31 => Call,
            32 => Take,
            33 => Index,
            34 => Column,
            35 => Zip,
            36 => Dispatch,
            37 => LoadFn,
            38 => CastList,
            39 => ListWhereFrame,
            40 => Pop,
            41 => Ret,
            42 => Halt,
            43 => Jump,
            44 => JumpIfFalse,
            45 => JumpIfVec,
            46 => CaseVec,
            47 => Noop,
            48 => Emit,
            other => {
                return Err(QplError::Runtime(format!(
                    "corrupt bytecode: unknown opcode byte {other}"
                )));
            }
        })
    }
}

impl Op {
    fn mnemonic(self) -> &'static str {
        use Op::*;
        match self {
            Push => "PUSH",
            Source => "SOURCE",
            LoadFile => "LOAD_FILE",
            RowIndex => "ROW_INDEX",
            LoadCol => "LOAD_COL",
            LoadRowIdx => "LOAD_ROWIDX",
            BinOp => "BINOP",
            Round => "ROUND",
            Window => "WINDOW",
            Case => "CASE",
            Alias => "ALIAS",
            Cast => "CAST",
            Store => "STORE",
            Sink => "SINK",
            Lazy => "LAZY",
            Collect => "COLLECT",
            Filter => "FILTER",
            Sort => "SORT",
            Distinct => "DISTINCT",
            DropNull => "DROPNULL",
            Limit => "LIMIT",
            Drop => "DROP",
            Cols => "COLS",
            Join => "JOIN",
            List => "LIST",
            Select => "SELECT",
            SelectBy => "SELECT_BY",
            Update => "UPDATE",
            Verb => "VERB",
            Load => "LOAD",
            Call => "CALL",
            Take => "TAKE",
            Index => "INDEX",
            Column => "COLUMN",
            Zip => "ZIP",
            Dispatch => "DISPATCH",
            LoadFn => "LOAD_FN",
            CastList => "CAST_LIST",
            ListWhereFrame => "LIST_WHERE_FRAME",
            Pop => "POP",
            Ret => "RET",
            Halt => "HALT",
            Jump => "JUMP",
            JumpIfFalse => "JUMP_IF_FALSE",
            JumpIfVec => "JUMP_IF_VEC",
            CaseVec => "CASE_VEC",
            Noop => "NOOP",
            Emit => "EMIT",
        }
    }
}

/// Maps a byte offset in `Program::code` to the source line it came from.
/// Unused until whole-program compilation needs `path:line:` error
/// prefixes spanning more than one statement; kept here now so `Program`'s
/// shape doesn't change again when that lands.
#[derive(Debug, Clone)]
pub struct LineEntry {
    pub ip: u32,
    pub path: Arc<str>,
    pub line: u32,
}

/// A compiled statement: one byte per instruction (`code`), a side operand
/// stream only [`Op::Push`] reads (`operands`), and an (for now, always
/// empty) source-line table.
#[derive(Debug, Default)]
pub struct Program {
    pub code: Vec<u8>,
    pub operands: Vec<Operand>,
    pub lines: Vec<LineEntry>,
    /// Compile-time only: function literals seen so far whose bodies haven't
    /// been appended yet. Never read at
    /// run time — drained by `compiler::finish_pending` before the `Program`
    /// is handed to the VM. Not part of the on-disk format.
    pub(crate) pending_closures: Vec<PendingClosure>,
}

impl Program {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `(path, line)` an opcode at `ip` belongs to, per `self.lines`
    /// the last entry whose own `ip` is at or
    /// before `ip` — entries are pushed in ascending `ip` order by
    /// `compiler::compile_program`, one per top-level statement (including
    /// each statement of an embedded `\l`/`\i` sub-`Program`, which has its
    /// own separate table). `None` for a `Program` with no line table at all
    /// (an ad hoc value-context helper program, never a whole script).
    pub fn line_at(&self, ip: usize) -> Option<(Arc<str>, u32)> {
        self.lines
            .iter()
            .rev()
            .find(|e| (e.ip as usize) <= ip)
            .map(|e| (e.path.clone(), e.line))
    }

    /// Emit a single opcode byte with no operand.
    pub fn emit(&mut self, op: Op) {
        self.code.push(op as u8);
    }

    /// Emit `PUSH operand` — the only way an operand ever gets appended to
    /// `operands`, keeping the two streams in lockstep.
    pub fn push_operand(&mut self, operand: Operand) {
        self.code.push(Op::Push as u8);
        self.operands.push(operand);
    }
}

/// One line per instruction: `ip`, mnemonic, and for `PUSH` the operand's
/// short form, e.g. `0007  PUSH       Name(price)` / `0008  LOAD_COL`. Used by
/// the REPL's `\d` and by the compiler's own tests.
pub fn disassemble(program: &Program) -> Vec<String> {
    let mut lines = Vec::with_capacity(program.code.len());
    let mut cp = 0usize;
    for (ip, &byte) in program.code.iter().enumerate() {
        let op = match Op::try_from(byte) {
            Ok(op) => op,
            Err(_) => {
                lines.push(format!("{ip:04}  <bad opcode {byte}>"));
                continue;
            }
        };
        if op == Op::Push {
            let repr = match program.operands.get(cp) {
                Some(operand) => format!("{operand:?}"),
                None => "<missing operand>".to_string(),
            };
            cp += 1;
            lines.push(format!("{ip:04}  {:<11}{repr}", op.mnemonic()));
        } else {
            lines.push(format!("{ip:04}  {}", op.mnemonic()));
        }
    }
    lines
}

// ---------------------------------------------------------------------------
// `.qplc` serialisation
// ---------------------------------------------------------------------------
//
// `qpl -C script.qpl` compiles a script straight to this format, so
// `qpl script.qplc` can run it with no lexing, parsing or compiling at all —
// see `repl::run_script`, which sniffs the magic bytes to tell a `.qplc` file
// from source text, and `main.rs`'s `-C`/`-o` handling. Layout, all integers
// little-endian:
//
// ```text
// magic "QPLC" (4 bytes)
// u16   format_version
// str   qpl_version           (informational only, never checked)
// ---- body (recursive: an embedded `Operand::Program` is one of these too) ----
// u32   code length, then that many raw instruction bytes
// u32   operand count, then that many tagged operands (see `OperandTag`)
// u32   path-table length, then that many length-prefixed path strings
// u32   line-entry count, then (u32 path index, u32 ip, u32 line) each
// ```
//
// `Op`/`Operand`/`Value` tag values are as fixed a part of this format as the
// bytes of a `.qpl` script's grammar are of the source format: renumbering,
// reordering or removing one is a breaking change to every `.qplc` already
// on disk, and must bump `FORMAT_VERSION` so a stale file fails cleanly
// (see `from_bytes`) instead of decoding as something else.
use crate::codec::{self, Reader};
use std::collections::HashMap;

/// `.qplc` magic bytes — the first 4 bytes of every compiled artifact.
/// `repl::run_script` checks these (not the file extension) to decide
/// whether a file is source or a compiled program.
pub const MAGIC: &[u8; 4] = b"QPLC";

/// The `.qplc` format version. Bump this whenever `Op`, `Operand`,
/// `OperandTag`, or any `Value`/enum tag used by the codec changes shape,
/// gains/loses/reorders a variant, or changes discriminant — see the module
/// doc above.
pub const FORMAT_VERSION: u16 = 1;

/// The one-byte tag identifying which [`Operand`] variant follows in a
/// `.qplc` file. Explicit discriminants, part of the on-disk format.
#[repr(u8)]
enum OperandTag {
    Value = 0,
    Name = 1,
    Count = 2,
    Target = 3,
    BinOp = 4,
    Verb = 5,
    Join = 6,
    Sort = 7,
    Names = 8,
    Cast = 9,
    Window = 10,
    Native = 11,
    Text = 12,
    Func = 13,
    Program = 14,
}

impl TryFrom<u8> for OperandTag {
    type Error = QplError;
    fn try_from(b: u8) -> Result<Self, QplError> {
        use OperandTag::*;
        Ok(match b {
            0 => Value,
            1 => Name,
            2 => Count,
            3 => Target,
            4 => BinOp,
            5 => Verb,
            6 => Join,
            7 => Sort,
            8 => Names,
            9 => Cast,
            10 => Window,
            11 => Native,
            12 => Text,
            13 => Func,
            14 => Program,
            other => {
                return Err(QplError::Runtime(format!(
                    "corrupt bytecode: unknown operand tag {other}"
                )));
            }
        })
    }
}

fn rt<E: std::fmt::Display>(e: E) -> QplError {
    QplError::Runtime(e.to_string())
}

fn binop_kind_to_u8(k: &BinOpKind) -> u8 {
    use BinOpKind::*;
    match k {
        Add => 0,
        Sub => 1,
        Mul => 2,
        Div => 3,
        Eq => 4,
        Neq => 5,
        Lt => 6,
        Le => 7,
        Gt => 8,
        Ge => 9,
        And => 10,
        Or => 11,
        Like => 12,
        Other(_) => 13,
    }
}

fn write_binop_kind(out: &mut Vec<u8>, k: &BinOpKind) {
    codec::push_u8(out, binop_kind_to_u8(k));
    if let BinOpKind::Other(s) = k {
        codec::push_str(out, s);
    }
}

fn read_binop_kind(r: &mut Reader) -> Result<BinOpKind, QplError> {
    use BinOpKind::*;
    Ok(match r.u8()? {
        0 => Add,
        1 => Sub,
        2 => Mul,
        3 => Div,
        4 => Eq,
        5 => Neq,
        6 => Lt,
        7 => Le,
        8 => Gt,
        9 => Ge,
        10 => And,
        11 => Or,
        12 => Like,
        13 => Other(r.string()?.into()),
        other => {
            return Err(rt(format!("corrupt bytecode: unknown BinOp tag {other}")));
        }
    })
}

fn join_type_to_u8(j: &JoinType) -> Result<u8, QplError> {
    match j {
        JoinType::Inner => Ok(0),
        JoinType::Left => Ok(1),
        JoinType::Right => Ok(2),
        other => Err(rt(format!("cannot serialise join type {other}"))),
    }
}

fn u8_to_join_type(b: u8) -> Result<JoinType, QplError> {
    Ok(match b {
        0 => JoinType::Inner,
        1 => JoinType::Left,
        2 => JoinType::Right,
        other => {
            return Err(rt(format!(
                "corrupt bytecode: unknown join type tag {other}"
            )));
        }
    })
}

fn cast_target_to_bytes(out: &mut Vec<u8>, c: &CastTarget) {
    match c {
        CastTarget::Prim(s) => {
            codec::push_u8(out, 0);
            codec::push_str(out, s);
        }
        CastTarget::Sym => codec::push_u8(out, 1),
        CastTarget::SymPhysical(s) => {
            codec::push_u8(out, 2);
            codec::push_str(out, s);
        }
        CastTarget::Enum(s) => {
            codec::push_u8(out, 3);
            codec::push_str(out, s);
        }
    }
}

fn read_cast_target(r: &mut Reader) -> Result<CastTarget, QplError> {
    Ok(match r.u8()? {
        0 => CastTarget::Prim(r.string()?),
        1 => CastTarget::Sym,
        2 => CastTarget::SymPhysical(r.string()?),
        3 => CastTarget::Enum(r.string()?),
        other => {
            return Err(rt(format!(
                "corrupt bytecode: unknown cast target tag {other}"
            )));
        }
    })
}

fn window_fn_to_u8(f: WindowFn) -> u8 {
    match f {
        WindowFn::Over => 0,
        WindowFn::RowNumber => 1,
        WindowFn::Rank => 2,
        WindowFn::DenseRank => 3,
    }
}

fn u8_to_window_fn(b: u8) -> Result<WindowFn, QplError> {
    Ok(match b {
        0 => WindowFn::Over,
        1 => WindowFn::RowNumber,
        2 => WindowFn::Rank,
        3 => WindowFn::DenseRank,
        other => {
            return Err(rt(format!(
                "corrupt bytecode: unknown window fn tag {other}"
            )));
        }
    })
}

fn write_window_spec(out: &mut Vec<u8>, w: &WindowSpec) {
    codec::push_u8(out, window_fn_to_u8(w.func));
    codec::push_u32(out, w.partition.len() as u32);
    for p in &w.partition {
        codec::push_str(out, p);
    }
    codec::push_u32(out, w.order.len() as u32);
    for (col, desc) in &w.order {
        codec::push_str(out, col);
        codec::push_u8(out, *desc as u8);
    }
    match &w.rolling {
        Some((name, size)) => {
            codec::push_u8(out, 1);
            codec::push_str(out, name);
            codec::push_u32(out, *size as u32);
        }
        None => codec::push_u8(out, 0),
    }
}

fn read_window_spec(r: &mut Reader) -> Result<WindowSpec, QplError> {
    let func = u8_to_window_fn(r.u8()?)?;
    let n = r.u32()?;
    let mut partition = Vec::with_capacity((n as usize).min(r.remaining()));
    for _ in 0..n {
        partition.push(r.string()?);
    }
    let n = r.u32()?;
    let mut order = Vec::with_capacity((n as usize).min(r.remaining()));
    for _ in 0..n {
        let col = r.string()?;
        let desc = r.u8()? != 0;
        order.push((col, desc));
    }
    let rolling = if r.u8()? != 0 {
        let name = r.string()?;
        let size = r.u32()? as usize;
        Some((name, size))
    } else {
        None
    };
    Ok(WindowSpec {
        func,
        partition,
        order,
        rolling,
    })
}

fn native_id_to_u8(id: NativeId) -> u8 {
    use NativeId::*;
    match id {
        Enlist => 0,
        Roll => 1,
        Cfg => 2,
        StdoutLog => 3,
        PrintText => 4,
        LoadScript => 5,
        ImportScript => 6,
    }
}

fn u8_to_native_id(b: u8) -> Result<NativeId, QplError> {
    use NativeId::*;
    Ok(match b {
        0 => Enlist,
        1 => Roll,
        2 => Cfg,
        3 => StdoutLog,
        4 => PrintText,
        5 => LoadScript,
        6 => ImportScript,
        other => {
            return Err(rt(format!(
                "corrupt bytecode: unknown native id tag {other}"
            )));
        }
    })
}

fn write_target(out: &mut Vec<u8>, ip: u32, cp: u32) {
    codec::push_u32(out, ip);
    codec::push_u32(out, cp);
}

/// Bounds-check a `(ip, cp)` pair against the sizes of the program currently
/// being read: `ip` may be `code_len` exactly (a jump/entry pointing one past
/// the end, e.g. a `while`'s exit target), `cp` may be `operand_count`
/// exactly for the same reason.
fn read_target(r: &mut Reader, code_len: u32, operand_count: u32) -> Result<(u32, u32), QplError> {
    let ip = r.u32()?;
    let cp = r.u32()?;
    if ip > code_len {
        return Err(rt(format!(
            "corrupt bytecode: jump target ip {ip} out of range (code length {code_len})"
        )));
    }
    if cp > operand_count {
        return Err(rt(format!(
            "corrupt bytecode: jump target cp {cp} out of range (operand count {operand_count})"
        )));
    }
    Ok((ip, cp))
}

fn write_operand(out: &mut Vec<u8>, op: &Operand) -> Result<(), QplError> {
    match op {
        Operand::Value(v) => {
            codec::push_u8(out, OperandTag::Value as u8);
            codec::encode_value(v, out)?;
        }
        Operand::Name(n) => {
            codec::push_u8(out, OperandTag::Name as u8);
            codec::push_str(out, n);
        }
        Operand::Count(n) => {
            codec::push_u8(out, OperandTag::Count as u8);
            codec::push_u32(out, *n);
        }
        Operand::Target { ip, cp } => {
            codec::push_u8(out, OperandTag::Target as u8);
            write_target(out, *ip, *cp);
        }
        Operand::BinOp(k) => {
            codec::push_u8(out, OperandTag::BinOp as u8);
            write_binop_kind(out, k);
        }
        Operand::Verb(v) => {
            codec::push_u8(out, OperandTag::Verb as u8);
            codec::push_str(out, v);
        }
        Operand::Join(j) => {
            codec::push_u8(out, OperandTag::Join as u8);
            codec::push_u8(out, join_type_to_u8(j)?);
        }
        Operand::Sort(s) => {
            codec::push_u8(out, OperandTag::Sort as u8);
            codec::push_u32(out, s.len() as u32);
            for (col, desc) in s.iter() {
                codec::push_str(out, col);
                codec::push_u8(out, *desc as u8);
            }
        }
        Operand::Names(n) => {
            codec::push_u8(out, OperandTag::Names as u8);
            codec::push_u32(out, n.len() as u32);
            for name in n.iter() {
                codec::push_str(out, name);
            }
        }
        Operand::Cast(c) => {
            codec::push_u8(out, OperandTag::Cast as u8);
            cast_target_to_bytes(out, c);
        }
        Operand::Window(w) => {
            codec::push_u8(out, OperandTag::Window as u8);
            write_window_spec(out, w);
        }
        Operand::Native(id) => {
            codec::push_u8(out, OperandTag::Native as u8);
            codec::push_u8(out, native_id_to_u8(*id));
        }
        Operand::Text(t) => {
            codec::push_u8(out, OperandTag::Text as u8);
            codec::push_str(out, t);
        }
        Operand::Func(f) => {
            codec::push_u8(out, OperandTag::Func as u8);
            codec::push_u32(out, f.params.len() as u32);
            for p in &f.params {
                codec::push_str(out, p);
            }
            write_target(out, f.entry.0, f.entry.1);
            codec::push_str(out, &f.display);
        }
        Operand::Program(p) => {
            codec::push_u8(out, OperandTag::Program as u8);
            write_body(p, out)?;
        }
    }
    Ok(())
}

/// Read one operand. `code_len`/`operand_count` are the *enclosing* program's
/// sizes (known already — `code` and the operand count are read before the
/// operands themselves, see `read_body`), used to bounds-check a `Target`'s
/// or a `Func`'s entry point.
fn read_operand(r: &mut Reader, code_len: u32, operand_count: u32) -> Result<Operand, QplError> {
    Ok(match OperandTag::try_from(r.u8()?)? {
        OperandTag::Value => Operand::Value(codec::decode_value(r)?),
        OperandTag::Name => Operand::Name(r.string()?.into()),
        OperandTag::Count => Operand::Count(r.u32()?),
        OperandTag::Target => {
            let (ip, cp) = read_target(r, code_len, operand_count)?;
            Operand::Target { ip, cp }
        }
        OperandTag::BinOp => Operand::BinOp(read_binop_kind(r)?),
        OperandTag::Verb => Operand::Verb(r.string()?.into()),
        OperandTag::Join => Operand::Join(u8_to_join_type(r.u8()?)?),
        OperandTag::Sort => {
            let n = r.u32()?;
            let mut v = Vec::with_capacity((n as usize).min(r.remaining()));
            for _ in 0..n {
                let col = r.string()?;
                let desc = r.u8()? != 0;
                v.push((col, desc));
            }
            Operand::Sort(v.into())
        }
        OperandTag::Names => {
            let n = r.u32()?;
            let mut v = Vec::with_capacity((n as usize).min(r.remaining()));
            for _ in 0..n {
                v.push(r.string()?);
            }
            Operand::Names(v.into())
        }
        OperandTag::Cast => Operand::Cast(read_cast_target(r)?),
        OperandTag::Window => Operand::Window(Arc::new(read_window_spec(r)?)),
        OperandTag::Native => Operand::Native(u8_to_native_id(r.u8()?)?),
        OperandTag::Text => Operand::Text(r.string()?.into()),
        OperandTag::Func => {
            let n = r.u32()?;
            let mut params = Vec::with_capacity((n as usize).min(r.remaining()));
            for _ in 0..n {
                params.push(r.string()?);
            }
            let entry = read_target(r, code_len, operand_count)?;
            let display = r.string()?;
            Operand::Func(Arc::new(FuncProto {
                params,
                entry,
                display,
            }))
        }
        OperandTag::Program => Operand::Program(Arc::new(read_body(r)?)),
    })
}

/// Every opcode byte decodes, and `Op::Push` occurrences line up exactly with
/// the number of operands provided — neither more (a `PUSH` reading past the
/// end of the stream) nor fewer (unused trailing operands). This is the same
/// walk `disassemble` does, but erroring instead of printing a placeholder.
fn validate_code_operand_lockstep(code: &[u8], operand_count: usize) -> Result<(), QplError> {
    let mut cp = 0usize;
    for &byte in code {
        let op = Op::try_from(byte)?;
        if op == Op::Push {
            if cp >= operand_count {
                return Err(rt(
                    "corrupt bytecode: PUSH reads past the end of the operand stream",
                ));
            }
            cp += 1;
        }
    }
    if cp != operand_count {
        return Err(rt(
            "corrupt bytecode: operand stream length does not match the code's PUSH count",
        ));
    }
    Ok(())
}

/// Serialise `program`'s body (code, operands, line table) — everything
/// except the file-level magic/version/qpl-version header, which only
/// `Program::to_bytes` writes once, at the top. An embedded `\l`/`\i`
/// sub-program (`Operand::Program`) is just another body, written recursively
/// right here with no header of its own — see the module doc's layout.
fn write_body(program: &Program, out: &mut Vec<u8>) -> Result<(), QplError> {
    codec::push_u32(out, program.code.len() as u32);
    out.extend_from_slice(&program.code);

    codec::push_u32(out, program.operands.len() as u32);
    for op in &program.operands {
        write_operand(out, op)?;
    }

    // Path table: dedup so a script with many top-level statements doesn't
    // repeat its own path once per `LineEntry`.
    let mut paths: Vec<&Arc<str>> = Vec::new();
    let mut index_of: HashMap<&str, u32> = HashMap::new();
    for entry in &program.lines {
        index_of.entry(&*entry.path).or_insert_with(|| {
            paths.push(&entry.path);
            (paths.len() - 1) as u32
        });
    }
    codec::push_u32(out, paths.len() as u32);
    for p in &paths {
        codec::push_str(out, p);
    }
    codec::push_u32(out, program.lines.len() as u32);
    for entry in &program.lines {
        let idx = index_of[&*entry.path];
        codec::push_u32(out, idx);
        codec::push_u32(out, entry.ip);
        codec::push_u32(out, entry.line);
    }
    Ok(())
}

/// Inverse of [`write_body`] — see [`Program::from_bytes`].
fn read_body(r: &mut Reader) -> Result<Program, QplError> {
    let code_len = r.u32()?;
    let code = r.take(code_len as usize)?.to_vec();

    let operand_count = r.u32()?;
    let mut operands = Vec::with_capacity((operand_count as usize).min(r.remaining()));
    for _ in 0..operand_count {
        operands.push(read_operand(r, code_len, operand_count)?);
    }

    let path_count = r.u32()?;
    let mut paths = Vec::with_capacity((path_count as usize).min(r.remaining()));
    for _ in 0..path_count {
        paths.push(Arc::<str>::from(r.string()?));
    }

    let line_count = r.u32()?;
    let mut lines = Vec::with_capacity((line_count as usize).min(r.remaining()));
    for _ in 0..line_count {
        let path_idx = r.u32()? as usize;
        let path = paths
            .get(path_idx)
            .cloned()
            .ok_or_else(|| rt("corrupt bytecode: path index out of range"))?;
        let ip = r.u32()?;
        let line = r.u32()?;
        lines.push(LineEntry { ip, path, line });
    }

    validate_code_operand_lockstep(&code, operands.len())?;

    Ok(Program {
        code,
        operands,
        lines,
        pending_closures: Vec::new(),
    })
}

impl Program {
    /// Serialise this program to a `.qplc` file's bytes — magic, format
    /// version, informational `qpl` version, then the recursive body (see the
    /// module doc). Fails if any `Operand::Value` holds a `Table`/`Lazy`/
    /// `Closure`/`Handle`/`Future` (`codec::encode_value` rejects those) —
    /// none of those can legitimately reach an operand (a closure literal is
    /// always `Operand::Func`, never a runtime `Value::Closure` baked in at
    /// compile time), but this makes that a clean error rather than silently
    /// writing garbage a `from_bytes` could later misinterpret.
    pub fn to_bytes(&self) -> Result<Vec<u8>, QplError> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        codec::push_u16(&mut out, FORMAT_VERSION);
        codec::push_str(&mut out, env!("CARGO_PKG_VERSION"));
        write_body(self, &mut out)?;
        Ok(out)
    }

    /// Deserialise a `.qplc` file's bytes back into a `Program`, ready to
    /// hand straight to `Vm::run_compiled` — no lexing, parsing or compiling
    /// involved. Never panics: a truncated file, an unknown opcode/operand/
    /// value tag, an out-of-range jump target, a format-version mismatch, or
    /// trailing garbage after the program all come back as `Err`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Program, QplError> {
        if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
            return Err(rt("not a qpl bytecode file (bad magic)"));
        }
        let mut r = Reader::new(&bytes[MAGIC.len()..]);
        let version = r.u16()?;
        if version != FORMAT_VERSION {
            return Err(rt(format!(
                "compiled with an incompatible qpl (format {version}, expected {FORMAT_VERSION}) — recompile with qpl -C"
            )));
        }
        let _qpl_version = r.string()?; // informational only, never checked
        let program = read_body(&mut r)?;
        if !r.is_empty() {
            return Err(rt("corrupt bytecode: trailing data after program"));
        }
        Ok(program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcode_discriminants_are_pinned() {
        // `.qplc` files serialise these as raw bytes — renumbering silently would
        // corrupt any `.qplc` compiled against an older layout.
        assert_eq!(Op::Push as u8, 0);
        assert_eq!(Op::Source as u8, 1);
        assert_eq!(Op::LoadCol as u8, 4);
        assert_eq!(Op::Select as u8, 26);
        assert_eq!(Op::Verb as u8, 29);
        assert_eq!(Op::Load as u8, 30);
        assert_eq!(Op::Call as u8, 31);
        assert_eq!(Op::Take as u8, 32);
        assert_eq!(Op::Index as u8, 33);
        assert_eq!(Op::Column as u8, 34);
        assert_eq!(Op::Zip as u8, 35);
        assert_eq!(Op::Dispatch as u8, 36);
        assert_eq!(Op::LoadFn as u8, 37);
        assert_eq!(Op::CastList as u8, 38);
        assert_eq!(Op::ListWhereFrame as u8, 39);
        assert_eq!(Op::Pop as u8, 40);
        assert_eq!(Op::Ret as u8, 41);
        assert_eq!(Op::Halt as u8, 42);
        assert_eq!(Op::Jump as u8, 43);
        assert_eq!(Op::JumpIfFalse as u8, 44);
        assert_eq!(Op::JumpIfVec as u8, 45);
        assert_eq!(Op::CaseVec as u8, 46);
        assert_eq!(Op::Noop as u8, 47);
        assert_eq!(Op::Emit as u8, 48);
    }

    #[test]
    fn try_from_rejects_unknown_byte() {
        assert!(Op::try_from(255u8).is_err());
        assert!(Op::try_from(49u8).is_err());
    }

    #[test]
    fn try_from_rejects_the_retired_eval_byte() {
        // byte 12 was `Op::Eval`, since deleted —
        // it must never silently decode as some other opcode.
        assert!(Op::try_from(12u8).is_err());
    }

    #[test]
    fn try_from_accepts_every_known_byte() {
        for b in 0..=48u8 {
            if b == 12 {
                continue; // retired — see `try_from_rejects_the_retired_eval_byte`
            }
            assert!(Op::try_from(b).is_ok(), "byte {b} should decode");
        }
    }

    #[test]
    fn disassemble_push_then_plain_op() {
        let mut p = Program::new();
        p.push_operand(Operand::Name("trades".into()));
        p.emit(Op::Source);
        p.push_operand(Operand::Name("price".into()));
        p.emit(Op::LoadCol);
        let lines = disassemble(&p);
        assert_eq!(
            lines,
            vec![
                "0000  PUSH       Name(trades)".to_string(),
                "0001  SOURCE".to_string(),
                "0002  PUSH       Name(price)".to_string(),
                "0003  LOAD_COL".to_string(),
            ]
        );
    }

    #[test]
    fn disassemble_reports_unknown_opcode_byte_without_panicking() {
        let mut p = Program::new();
        p.code.push(200); // not a valid Op
        let lines = disassemble(&p);
        assert_eq!(lines, vec!["0000  <bad opcode 200>".to_string()]);
    }

    // -- `.qplc` serialisation --------------------------------------------

    /// A small but non-trivial program: every operand kind that isn't
    /// exercised by a golden-example round trip elsewhere (`repl.rs`'s
    /// `golden` module) — `Sort`, `Names`, `Window`, a nested `Func`, and a
    /// `LineEntry` table with a couple of distinct paths.
    fn sample_program() -> Program {
        let mut p = Program::new();
        p.lines.push(LineEntry {
            ip: 0,
            path: Arc::from("a.qpl"),
            line: 0,
        });
        p.push_operand(Operand::Name("trades".into()));
        p.emit(Op::Source);
        p.push_operand(Operand::Sort(
            vec![("price".to_string(), true), ("sym".to_string(), false)].into(),
        ));
        p.emit(Op::Sort);
        p.push_operand(Operand::Names(
            vec!["a".to_string(), "b".to_string()].into(),
        ));
        p.emit(Op::Drop);
        p.push_operand(Operand::Window(Arc::new(WindowSpec {
            func: WindowFn::Rank,
            partition: vec!["sym".to_string()],
            order: vec![("price".to_string(), true)],
            rolling: Some(("sum".to_string(), 5)),
        })));
        p.emit(Op::Window);
        p.push_operand(Operand::Cast(CastTarget::SymPhysical("u16".to_string())));
        p.emit(Op::Cast);
        p.push_operand(Operand::Join(JoinType::Left));
        p.emit(Op::Join);
        p.push_operand(Operand::BinOp(BinOpKind::Other("weird".into())));
        p.emit(Op::BinOp);
        p.push_operand(Operand::Native(NativeId::ImportScript));
        p.push_operand(Operand::Func(Arc::new(FuncProto {
            params: vec!["x".to_string()],
            entry: (3, 1),
            display: "{[x] x+1}".to_string(),
        })));
        p.lines.push(LineEntry {
            ip: p.code.len() as u32,
            path: Arc::from("b.qpl"),
            line: 4,
        });
        p.emit(Op::Pop);
        p
    }

    #[test]
    fn qplc_round_trip_preserves_disassembly() {
        let p = sample_program();
        let before = disassemble(&p);
        let bytes = p.to_bytes().expect("to_bytes");
        assert_eq!(&bytes[..4], MAGIC);
        let p2 = Program::from_bytes(&bytes).expect("from_bytes");
        let after = disassemble(&p2);
        assert_eq!(before, after);
        assert_eq!(p.lines.len(), p2.lines.len());
        for (a, b) in p.lines.iter().zip(p2.lines.iter()) {
            assert_eq!(a.ip, b.ip);
            assert_eq!(a.line, b.line);
            assert_eq!(&*a.path, &*b.path);
        }
    }

    #[test]
    fn qplc_round_trip_preserves_embedded_sub_program() {
        let mut outer = Program::new();
        outer.lines.push(LineEntry {
            ip: 0,
            path: Arc::from("outer.qpl"),
            line: 0,
        });
        outer.push_operand(Operand::Program(Arc::new(sample_program())));
        outer.push_operand(Operand::Count(1));
        outer.push_operand(Operand::Native(NativeId::LoadScript));
        outer.emit(Op::Call);

        let before = disassemble(&outer);
        let bytes = outer.to_bytes().expect("to_bytes");
        let round_tripped = Program::from_bytes(&bytes).expect("from_bytes");
        assert_eq!(before, disassemble(&round_tripped));
    }

    #[test]
    fn from_bytes_rejects_bad_magic() {
        assert!(Program::from_bytes(b"NOPE").is_err());
        assert!(Program::from_bytes(b"").is_err());
    }

    #[test]
    fn from_bytes_rejects_format_version_mismatch() {
        let p = Program::new();
        let mut bytes = p.to_bytes().unwrap();
        // format_version is the u16 right after the 4-byte magic.
        bytes[4] = 0xFF;
        bytes[5] = 0xFF;
        let err = Program::from_bytes(&bytes).unwrap_err();
        assert!(
            err.to_string().contains("recompile with qpl -C"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn from_bytes_rejects_truncated_input() {
        let p = sample_program();
        let bytes = p.to_bytes().unwrap();
        for cut in [bytes.len() / 2, MAGIC.len() + 2, 1] {
            assert!(
                Program::from_bytes(&bytes[..cut]).is_err(),
                "expected an error truncating at {cut}"
            );
        }
    }

    #[test]
    fn from_bytes_rejects_trailing_garbage() {
        let p = sample_program();
        let mut bytes = p.to_bytes().unwrap();
        bytes.push(0xAB);
        assert!(Program::from_bytes(&bytes).is_err());
    }

    #[test]
    fn from_bytes_rejects_unknown_opcode_byte() {
        let mut p = Program::new();
        p.emit(Op::Noop);
        let mut bytes = p.to_bytes().unwrap();
        let last = bytes.len() - 1;
        bytes[last] = 200; // not a valid Op byte
        assert!(Program::from_bytes(&bytes).is_err());
    }

    #[test]
    fn from_bytes_rejects_out_of_range_jump_target() {
        let mut p = Program::new();
        p.push_operand(Operand::Target { ip: 999, cp: 0 });
        p.emit(Op::Jump);
        let bytes = p.to_bytes().unwrap();
        assert!(Program::from_bytes(&bytes).is_err());
    }

    #[test]
    fn to_bytes_rejects_unserialisable_operand_values() {
        let mut p = Program::new();
        p.push_operand(Operand::Value(Value::Table(
            polars::prelude::DataFrame::empty(),
        )));
        assert!(p.to_bytes().is_err());
    }

    #[test]
    fn qplc_round_trip_preserves_nulls_in_a_literal_vector_operand() {
        use polars::prelude::NamedFrom;
        let s = polars::prelude::Series::new("".into(), Vec::<Option<i64>>::from([None, Some(7)]));
        let mut p = Program::new();
        p.push_operand(Operand::Value(Value::IntVec(s)));
        p.emit(Op::Pop);
        let bytes = p.to_bytes().unwrap();
        let p2 = Program::from_bytes(&bytes).unwrap();
        match &p2.operands[0] {
            Operand::Value(Value::IntVec(s2)) => {
                let ca = s2.i64().unwrap();
                let got: Vec<Option<i64>> = ca.iter().collect();
                assert_eq!(got, vec![None, Some(7)]);
            }
            other => panic!("expected an IntVec operand, got {other:?}"),
        }
    }
}
