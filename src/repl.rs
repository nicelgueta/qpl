use crate::ast;
use crate::compiler::CompileCtx;
use crate::errors::QplError;
use crate::lexer::tokenise;
use crate::parser::{normalize_function_body_newlines, parse, parse_program};
use crate::temporal;
use crate::tokens::TokenKind;
#[cfg(all(feature = "ipc", feature = "cli"))]
use crate::vm::EvalResult;
use crate::vm::Vm;
use polars::prelude::*;
#[cfg(feature = "cli")]
use rustyline::{DefaultEditor, error::ReadlineError};
use std::sync::Arc;

/// Run a `.qpl` script file, printing results. Returns Err on the first failure.
/// The *whole* file is parsed and compiled
/// into one [`crate::program::Program`] before anything runs — a parse or
/// compile error anywhere in it (including inside a `\l`/`\i` target, which
/// is itself read/parsed/compiled right here) aborts before even the first
/// statement executes.
///
/// `path` may equally be a `.qplc` file:
/// detected by its magic bytes, not its extension, so it runs with no
/// lexing, parsing or compiling at all — see [`crate::program::Program::from_bytes`].
pub fn run_script(path: &str, vm: &mut Vm) -> Result<(), QplError> {
    let bytes =
        std::fs::read(path).map_err(|e| QplError::Runtime(format!("cannot read '{path}': {e}")))?;
    if bytes.starts_with(crate::program::MAGIC) {
        let program = crate::program::Program::from_bytes(&bytes)?;
        return run_compiled_program(vm, Arc::new(program));
    }
    let src = String::from_utf8(bytes)
        .map_err(|e| QplError::Runtime(format!("cannot read '{path}': {e}")))?;
    run_source(vm, &src, path)
}

/// Disassemble `path` — a `.qplc` file (detected by magic bytes) or a `.qpl`
/// source file, compiled but not run. Backs `qpl -d`.
pub fn disassemble_file(path: &str) -> Result<Vec<String>, QplError> {
    let bytes =
        std::fs::read(path).map_err(|e| QplError::Runtime(format!("cannot read '{path}': {e}")))?;
    let program = if bytes.starts_with(crate::program::MAGIC) {
        crate::program::Program::from_bytes(&bytes)?
    } else {
        compile_script(path)?
    };
    Ok(crate::program::disassemble(&program))
}

/// Compile `path` (source only — a `.qplc` file has nothing left to compile)
/// into one whole-program [`crate::program::Program`], without running it.
/// The `qpl -C` CLI flag is this function plus
/// `Program::to_bytes`; also used by the round-trip tests in `repl::golden`.
pub fn compile_script(path: &str) -> Result<crate::program::Program, QplError> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| QplError::Runtime(format!("cannot read '{path}': {e}")))?;
    compile_program_for(path, &src)
}

/// Parse `src` (as `path`) and compile it into a whole `Script`-mode
/// [`crate::program::Program`] — the shared front half of [`run_source`] and
/// [`compile_script`].
fn compile_program_for(path: &str, src: &str) -> Result<crate::program::Program, QplError> {
    let stmts = parse_program(src, path)?;
    let ctx = CompileCtx::script(path);
    crate::compiler::compile_program(stmts, ctx)
}

/// Parse, compile and run `src` as a whole `Script`-mode program against
/// `vm` — see [`run_compiled_program`] for the run/error-reporting half.
fn run_source(vm: &mut Vm, src: &str, path: &str) -> Result<(), QplError> {
    let program = compile_program_for(path, src)?;
    run_compiled_program(vm, Arc::new(program))
}

/// Run an already-compiled whole-program [`crate::program::Program`] against
/// `vm`, reporting a runtime failure as `path:line:` (see `compiler::wrap_line_error`)
/// via whichever `Program` was actually executing when it failed
/// (`Vm::take_error_site`): the main program for an ordinary failure, or an
/// embedded `\l`/`\i` target's own `Program` (which carries its own path) if
/// the failure happened while that was running. `<main>` (the REPL) is never
/// prefixed. One [`crate::interrupt::Interrupt::statement`] guard covers the
/// whole run, however many statements the program contains. Shared by
/// [`run_source`] (a freshly compiled program) and a `.qplc` file run
/// straight from bytes (`run_script`'s magic-byte path, and `qpl -c`'s
/// ad hoc command via [`run_command`]).
fn run_compiled_program(
    vm: &mut Vm,
    program: Arc<crate::program::Program>,
) -> Result<(), QplError> {
    let _running = vm.interrupt.statement();
    let outcome = vm.run_compiled(program);
    let site = vm.take_error_site();
    outcome.map(|_| ()).map_err(|e| match site {
        Some((prog, ip)) => match prog.line_at(ip) {
            Some((p, line)) => crate::compiler::wrap_line_error(e, &p, line),
            None => e,
        },
        None => e,
    })
}

/// Run `src` as an ad hoc command (`qpl -c '<command>'`): parsed/compiled/run exactly like a REPL line typed at `<main>`
/// — errors are reported unprefixed, and `src` may hold several statements
/// (the usual multi-line rules apply, since it goes through
/// [`crate::parser::parse_program`] like a real script).
pub fn run_command(src: &str, vm: &mut Vm) -> Result<(), QplError> {
    run_source(vm, src, "<main>")
}

/// Everything a submitted line can be *except* `\port`, which needs
/// REPL-loop state (`PortSession`) this function doesn't have. Shared by the
/// REPL loop (interactive input and, once a port is open, the polling loop's
/// stdin lines). `lineno` is accepted for source-compatibility with earlier
/// callers but is no longer meaningful: [`run_source`] parses `src` itself
/// and reports errors using its own line numbers (this is only ever called
/// with `path == "<main>"`, where no prefix is added anyway).
fn run_line(src: &str, vm: &mut Vm, path: &str, _lineno: usize) -> Result<(), QplError> {
    run_source(vm, src, path)
}

/// Run a `.qpl` script as a *namespaced import* — test-only compatibility
/// shim for the many `\i`-behaviour tests below, which predate whole-program
/// compilation and call this directly with an absolute scratch-file path
/// rather than going through a `\i "..."` statement themselves. Namespace
/// qualification itself is compile-time now (`compiler::qualify_program`);
/// this just spells out the equivalent `\i` statement.
#[cfg(test)]
fn run_script_imported(path: &str, vm: &mut Vm) -> Result<(), QplError> {
    run_source(vm, &format!("\\i \"{path}\""), "<main>")
}

/// Does `src` look like an unfinished statement that should keep reading?
/// True while `(`/`[` are unbalanced, on a trailing `,`, on a lex error (e.g. an
/// unterminated string), or when the parse fails specifically because input ran
/// out (so a genuine syntax error still surfaces immediately). `\` commands are
/// always single-line.
pub fn wants_more(src: &str) -> bool {
    // nothing but blank / comment lines is a complete no-op (see `eval_capture`),
    // not an unfinished statement — otherwise a host that feeds a script line
    // by line glues a leading comment onto the statement after it
    if src
        .lines()
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with('/'))
    {
        return false;
    }
    let trimmed = src.trim_start();
    if trimmed.starts_with('\\') || trimmed.starts_with(".qpl.cfg") {
        return false;
    }
    // decide completeness against the same text `run_line` will actually
    // execute (see `normalize_function_body_newlines`) — otherwise a function
    // body relying on the implicit per-line statement rule looks like a
    // "complete but wrong" statement (missing `;`) the moment its second line
    // is typed, and the REPL submits it before the closing `}` even arrives.
    let src = &normalize_function_body_newlines(src);
    let toks = match tokenise(src) {
        Ok(toks) => toks,
        // an unterminated string may be finished on the next line; every other
        // lex error is terminal, so stop reading and let it surface.
        Err(QplError::Lex(msg)) => return msg.contains("Unterminated"),
        Err(_) => return false,
    };
    let mut depth: i32 = 0;
    for t in &toks {
        match t.kind {
            TokenKind::LBracket | TokenKind::LParen | TokenKind::LBrace => depth += 1,
            TokenKind::RBracket | TokenKind::RParen | TokenKind::RBrace => depth -= 1,
            _ => {}
        }
    }
    if depth > 0 || matches!(toks.last().map(|t| &t.kind), Some(TokenKind::Comma)) {
        return true;
    }
    match parse(toks) {
        Ok(_) => false,
        // input ran out mid-expression: either a specific token was expected
        // ("expected RParen, got Eof") or any primary was ("Unexpected token in
        // primary: Eof", e.g. a trailing operator or a dict literal missing values)
        Err(QplError::Parse(msg)) => msg.contains("got Eof") || msg.ends_with("primary: Eof"),
        Err(_) => false,
    }
}

#[cfg(feature = "cli")]
pub fn start(vm: &mut Vm) {
    let mut rl = DefaultEditor::new().expect("failed to create line editor");

    println!(
        "qpl v{} (Quick Polars Language) REPL - \\d disassemble, \\l <path> run a script, \\i \"<path>\" import as a namespace, \\1 <path> log stdout",
        env!("CARGO_PKG_VERSION")
    );

    let mut buf: Vec<String> = Vec::new();
    #[cfg(feature = "ipc")]
    let mut port_session: Option<PortSession> = None;

    loop {
        // Once `\port` has been used at least once, service both stdin and
        // any open listener by polling instead of a blocking `readline()` —
        // that's what lets a request arriving over the socket interleave with
        // whatever the operator is typing. This does mean losing rustyline's
        // line-editing/history from that point on for the rest of the
        // session; there's no clean way to hand stdin back to rustyline once
        // another thread owns reading it.
        #[cfg(feature = "ipc")]
        if let Some(session) = port_session.as_mut() {
            match session.poll() {
                PortEvent::Line(line) => process_submitted(&line, vm),
                PortEvent::Request(mode, command, reply_tx) => {
                    let _running = vm.interrupt.statement();
                    let result =
                        vm.with_request_permission(mode, |vm| eval_for_dispatch(&command, vm));
                    let _ = reply_tx.send(crate::ipc::encode_result(&result));
                }
                PortEvent::StdinClosed => break,
            }
            continue;
        }

        let prompt = if buf.is_empty() { "qpl) " } else { "  ...  " };
        match rl.readline(prompt) {
            Ok(line) => {
                let blank = line.trim().is_empty();
                if buf.is_empty() {
                    if blank || line.trim_start().starts_with('/') {
                        continue;
                    }
                    buf.push(line);
                } else if !blank {
                    buf.push(line);
                }
                // (a blank line while buf is non-empty force-submits)
                let src = buf.join("\n");
                if !blank && wants_more(&src) {
                    continue;
                }
                buf.clear();
                let src = src.trim().to_string();
                if src.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(&src);
                #[cfg(feature = "ipc")]
                if let Some(rest) = src.strip_prefix("\\port").map(str::trim) {
                    let session = port_session.get_or_insert_with(PortSession::new);
                    if let Err(e) = handle_port_directive(rest, session) {
                        eprintln!("{}", fmt_repl_error(&e));
                    }
                    continue;
                }
                process_submitted(&src, vm);
            }
            Err(ReadlineError::Interrupted) => {
                // abandon a partial statement, or exit at an empty prompt
                if buf.is_empty() {
                    break;
                }
                buf.clear();
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        }
    }
}

/// Everything a submitted REPL line can be *except* `\port`, which needs
/// REPL-loop state (`PortSession`) this function doesn't have — see
/// [`run_line`]. Shared by both the normal (rustyline) input path and, once a
/// port has been opened, the polling loop's stdin lines.
#[cfg(feature = "cli")]
fn process_submitted(src: &str, vm: &mut Vm) {
    if let Err(e) = run_line(src, vm, "<main>", 0) {
        eprintln!("{}", fmt_repl_error(&e));
    }
}

/// Evaluate one submitted line with its output captured instead of printed —
/// the non-terminal equivalent of [`process_submitted`], used by the wasm REPL.
/// Returns `(output, error)`: `output` is exactly what the CLI would have
/// printed to stdout (including anything emitted before a failure), and `error`
/// is the message the CLI would have put on stderr, if the line failed.
pub fn eval_capture(src: &str, vm: &mut Vm) -> (String, Option<String>) {
    // the terminal REPL drops blank and comment-only lines before they reach
    // `run_line` (they tokenise to nothing, which doesn't parse); a host that
    // submits a script line by line gets the same treatment here
    if src
        .lines()
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with('/'))
    {
        return (String::new(), None);
    }
    let outer = vm.capture.replace(String::new());
    let err = run_line(src, vm, "<main>", 0)
        .err()
        .map(|e| fmt_repl_error(&e));
    let out = std::mem::replace(&mut vm.capture, outer).unwrap_or_default();
    (out, err)
}

/// What [`eval_capture_table`] returns: the text output and error exactly as
/// [`eval_capture`] would give them, plus the result table itself when the
/// statement produced one (in which case it is *not* also in `output`).
#[cfg(feature = "wasm")]
pub struct TableEval {
    pub output: String,
    pub error: Option<String>,
    pub table: Option<polars::prelude::DataFrame>,
}

/// Like [`eval_capture`], but a table result comes back as a `DataFrame` —
/// untruncated, with its types — rather than as display text. Non-table results
/// (scalars, lists, plans) still land in `output`.
#[cfg(feature = "wasm")]
pub fn eval_capture_table(src: &str, vm: &mut Vm) -> TableEval {
    let outer_table = std::mem::replace(&mut vm.capture_table, true);
    vm.last_table = None;
    let (output, error) = eval_capture(src, vm);
    vm.capture_table = outer_table;
    TableEval {
        output,
        error,
        table: vm.last_table.take(),
    }
}

/// `\port <n>` opens a listener (closing any previously open one first);
/// bare `\port` closes it. Only reachable from `start()` — `\port` doesn't
/// exist for script mode (`run_script` never calls this), per its being
/// meaningless outside a long-lived interactive session.
#[cfg(all(feature = "ipc", feature = "cli"))]
fn handle_port_directive(rest: &str, session: &mut PortSession) -> Result<(), QplError> {
    session.close_port();
    if rest.is_empty() {
        return Ok(());
    }
    let port: u16 = rest
        .parse()
        .map_err(|_| QplError::Runtime(format!("\\port: expected a port number, got '{rest}'")))?;
    session.open_port(port)
}

/// Evaluate one command received over `\port`, treating it exactly like a
/// REPL line — `.qpl.cfg` directives, `log` writes, and ordinary
/// statements (selects, updates, deletes, assignments, function defs) all
/// work. `\`-prefixed system commands (`\d`, `\l`, `\1`, `\port` itself)
/// are deliberately not reachable this way — they're local REPL/session
/// administration, not part of the query language a remote client dispatches,
/// so a [`crate::ast::Stmt::System`] is rejected outright. `.qpl.cfg`/bareword `log` compile and
/// run exactly like the equivalent ordinary statement, in `CompileMode::Result`
/// so the (only) statement's value comes back as an [`EvalResult`].
#[cfg(all(feature = "ipc", feature = "cli"))]
fn eval_for_dispatch(line: &str, vm: &mut Vm) -> Result<EvalResult, QplError> {
    let stmts = parse_program(line, "<main>")?;
    if let Some((_, ast::Stmt::System { cmd, .. })) = stmts
        .iter()
        .find(|(_, s)| matches!(s, ast::Stmt::System { .. }))
    {
        return Err(QplError::Runtime(format!(
            "'\\{cmd}' is not allowed over a dispatched connection"
        )));
    }
    let ctx = CompileCtx::result("<main>");
    let program = crate::compiler::compile_program(stmts, ctx)?;
    vm.eval(program)
}

/// REPL-loop-side state for `\port`: a stdin-reader thread (spawned once, the
/// first time `\port` is used) feeding lines to the polling loop in `start()`,
/// plus whichever listener is currently open, if any.
#[cfg(all(feature = "ipc", feature = "cli"))]
struct PortSession {
    stdin_rx: std::sync::mpsc::Receiver<String>,
    port: Option<(
        crate::ipc::ServerHandle,
        std::sync::mpsc::Receiver<crate::ipc::PortRequest>,
    )>,
}

#[cfg(all(feature = "ipc", feature = "cli"))]
enum PortEvent {
    Line(String),
    Request(
        crate::ipc::HandleMode,
        String,
        std::sync::mpsc::Sender<Vec<u8>>,
    ),
    StdinClosed,
}

#[cfg(all(feature = "ipc", feature = "cli"))]
impl PortSession {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::stdin().lock().lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            stdin_rx: rx,
            port: None,
        }
    }

    fn open_port(&mut self, port: u16) -> Result<(), QplError> {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = crate::ipc::start_server(port, tx)?;
        self.port = Some((handle, rx));
        Ok(())
    }

    fn close_port(&mut self) {
        if let Some((handle, _)) = self.port.take() {
            handle.close();
        }
    }

    /// Block until either a stdin line or a socket request is available.
    fn poll(&mut self) -> PortEvent {
        loop {
            match self.stdin_rx.try_recv() {
                Ok(line) => return PortEvent::Line(line),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return PortEvent::StdinClosed,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            if let Some((_, rx)) = &self.port
                && let Ok((mode, command, reply_tx)) = rx.try_recv()
            {
                return PortEvent::Request(mode, command, reply_tx);
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    }
}

/// Rebase a kdb-style timestamp literal (as accepted by [`temporal::parse_temporal`])
/// to ns-since-Unix-epoch, for building a Polars `Datetime` demo column in Rust —
/// the same rebasing [`crate::vm::ast_val_to_expr`] applies to a `Timestamp` literal
/// at query time.
fn demo_ts(literal: &str) -> i64 {
    match temporal::parse_temporal(literal) {
        Some(ast::Value::Timestamp(ns)) => ns + temporal::NS_2000_TO_1970,
        _ => panic!("bad demo timestamp literal: {literal}"),
    }
}

pub fn load_demo_tables(vm: &mut Vm) {
    let trades = df![
        "sym"   => ["AAPL","AAPL","MSFT","MSFT","GOOG","GOOG","AAPL","MSFT"],
        "price" => [182.3f64, 183.1, 415.2, 416.0, 140.5, 141.2, 184.0, 414.8],
        "size"  => [100i64, 250, 80, 300, 150, 90, 500, 200],
        "side"  => ["buy","sell","buy","buy","sell","buy","sell","sell"],
        // one trading morning, 2024.03.15 — used by the temporal examples
        "ts"    => [
            demo_ts("2024.03.15D09:30:00.000000000"),
            demo_ts("2024.03.15D09:31:15.000000000"),
            demo_ts("2024.03.15D09:32:40.000000000"),
            demo_ts("2024.03.15D09:45:05.000000000"),
            demo_ts("2024.03.15D10:01:30.000000000"),
            demo_ts("2024.03.15D10:02:50.000000000"),
            demo_ts("2024.03.15D10:15:00.000000000"),
            demo_ts("2024.03.15D10:20:35.000000000"),
        ],
    ]
    .expect("trades");
    let trades = trades
        .lazy()
        .with_column(col("ts").cast(DataType::Datetime(TimeUnit::Nanoseconds, None)))
        .collect()
        .expect("cast trades.ts");

    let quotes = df![
        "sym"   => ["AAPL","MSFT","GOOG","AAPL","MSFT"],
        "bid"   => [182.0f64, 415.0, 140.3, 183.8, 414.5],
        "ask"   => [182.5f64, 415.5, 140.8, 184.2, 415.0],
        "bsize" => [500i64, 300, 200, 400, 600],
        "asize" => [400i64, 250, 150, 350, 500],
        "ts"    => [
            demo_ts("2024.03.15D09:29:55.000000000"),
            demo_ts("2024.03.15D09:32:35.000000000"),
            demo_ts("2024.03.15D10:01:25.000000000"),
            demo_ts("2024.03.15D10:14:55.000000000"),
            demo_ts("2024.03.15D10:20:30.000000000"),
        ],
    ]
    .expect("quotes");
    let quotes = quotes
        .lazy()
        .with_column(col("ts").cast(DataType::Datetime(TimeUnit::Nanoseconds, None)))
        .collect()
        .expect("cast quotes.ts");

    vm.globals
        .insert("trades".into(), ast::Value::Table(trades));
    vm.globals
        .insert("quotes".into(), ast::Value::Table(quotes));
}

/// kdb-style tag for a vector kind, used as the `<tag>[<n>]:` prefix.
fn vec_tag(kind: ast::VecKind) -> &'static str {
    use ast::VecKind::*;
    match kind {
        Int => "i64",
        Float => "f64",
        Sym => "sym",
        Str => "str",
        Bool => "bool",
        Date => "date",
        Month => "month",
        Time => "time",
        Minute => "minute",
        Second => "second",
        Timestamp => "timestamp",
        Timespan => "timespan",
    }
}

/// The scalar `Value` a raw temporal-vector element (its kdb integer offset)
/// corresponds to, so it can be rendered through `temporal::format_temporal`.
fn temporal_scalar_of(kind: ast::VecKind, n: i64) -> ast::Value {
    use ast::VecKind::*;
    match kind {
        Date => ast::Value::Date(n as i32),
        Month => ast::Value::Month(n as i32),
        Time => ast::Value::Time(n),
        Minute => ast::Value::Minute(n as i32),
        Second => ast::Value::Second(n as i32),
        Timestamp => ast::Value::Timestamp(n),
        Timespan => ast::Value::Timespan(n),
        _ => unreachable!("not a temporal vector kind"),
    }
}

/// Space-separated rendering of a vector `Value`'s elements. `quote_str`
/// selects the pretty (`"a" "b"`) vs. raw (`log`, `a b`) string form.
fn fmt_vec_elems(kind: ast::VecKind, s: &polars::prelude::Series, quote_str: bool) -> String {
    use ast::VecKind::*;
    let ca_str = || s.str().expect("SymVec/StrVec backed by a string Series");
    match kind {
        Sym => ca_str().iter().flatten().map(|x| format!("`{x}")).collect(),
        Str => {
            if quote_str {
                ca_str()
                    .iter()
                    .flatten()
                    .map(|x| format!("\"{x}\" "))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            } else {
                ca_str().iter().flatten().collect::<Vec<_>>().join(" ")
            }
        }
        Bool => s
            .bool()
            .expect("BoolVec backed by a bool Series")
            .iter()
            .flatten()
            .map(|b| if b { "1" } else { "0" })
            .collect(),
        Int => s
            .i64()
            .expect("IntVec backed by an i64 Series")
            .iter()
            .flatten()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(" "),
        Float => s
            .f64()
            .expect("FloatVec backed by an f64 Series")
            .iter()
            .flatten()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(" "),
        Date | Month | Minute | Second => s
            .i32()
            .expect("temporal vector backed by an i32 Series")
            .iter()
            .flatten()
            .map(|n| temporal::format_temporal(&temporal_scalar_of(kind, n as i64)).unwrap())
            .collect::<Vec<_>>()
            .join(" "),
        Time | Timestamp | Timespan => s
            .i64()
            .expect("temporal vector backed by an i64 Series")
            .iter()
            .flatten()
            .map(|n| temporal::format_temporal(&temporal_scalar_of(kind, n)).unwrap())
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Render a value for `log`: raw text, no type prefix or quoting. Shared with
/// `ops::native_log`, which does this same rendering whichever way `log` was
/// spelled (bareword, `log[..]`, or `run_log`'s statement-level forms).
pub(crate) fn fmt_log_val(v: &ast::Value) -> String {
    if let Some((kind, s)) = v.as_vec() {
        return fmt_vec_elems(kind, s, false);
    }
    match v {
        ast::Value::Str(s) => s.clone(),
        ast::Value::Sym(s) => s.clone(),
        ast::Value::Int(n) => n.to_string(),
        ast::Value::Float(f) => f.to_string(),
        ast::Value::Bool(b) => b.to_string(),
        _ => temporal::format_temporal(v).unwrap_or_else(|| format!("{v:?}")),
    }
}

pub(crate) fn fmt_val(v: &ast::Value) -> String {
    if let Some(text) = temporal::format_temporal(v) {
        let tag = match v {
            ast::Value::Date(_) => "date",
            ast::Value::Month(_) => "month",
            ast::Value::Time(_) => "time",
            ast::Value::Minute(_) => "minute",
            ast::Value::Second(_) => "second",
            ast::Value::Timestamp(_) => "timestamp",
            _ => "timespan",
        };
        return format!("{tag}: {text}");
    }
    if let Some((kind, s)) = v.as_vec() {
        return format!(
            "{}[{}]: {}",
            vec_tag(kind),
            s.len(),
            fmt_vec_elems(kind, s, true)
        );
    }
    match v {
        ast::Value::Int(n) => format!("i64: {n}"),
        ast::Value::Float(f) => format!("f64: {f}"),
        ast::Value::Str(s) => format!("str: \"{s}\""),
        ast::Value::Sym(s) => format!("sym: `{s}"),
        ast::Value::Bool(b) => format!("bool: {b}"),
        ast::Value::Closure(f) => format!("func: {{[{}] ..}}", f.params.join(",")),
        // temporal variants are handled by the early return above; this keeps
        // the match total without a panic path if a new `Value` is added
        other => temporal::format_temporal(other).unwrap_or_else(|| format!("{other:?}")),
    }
}

fn fmt_repl_error(error: &QplError) -> String {
    match error {
        QplError::Lex(message)
        | QplError::Parse(message)
        | QplError::Compile(message)
        | QplError::Runtime(message) => format!("'{message}"),
        QplError::Interrupted => "'interrupted".to_string(),
    }
}

/// Golden-output tests: runs every non-excluded
/// `examples/*.qpl` script against a fresh `Vm` and compares captured stdout
/// plus the error text (if any) against `examples/golden/<name>.out`.
///
/// `UPDATE_GOLDEN=1 cargo test golden` regenerates the snapshot files. Must be
/// run from the repo root so the scripts' relative `examples/data/...` paths
/// resolve.
///
/// Excluded entirely:
/// - `ipc_client` / `ipc_server`: need two live processes talking over a
///   socket, not a fit for a single-process snapshot test.
/// - `setup_data`: writes into `examples/data`, which is committed input for
///   every other example — running it would mutate the fixtures under test.
/// - `namespace_lib`: not meant to run standalone; it's exercised as an
///   import by `namespaces.qpl`.
///
/// `lists`, `random_table`, and `temporal` use `?` (roll) or the `.qpl.dt` /
/// `.qpl.tm` / `.qpl.ts` now-functions, so their output is different on every
/// run. Rather than normalise the random bytes out of the snapshot (fragile,
/// and easy to accidentally make the test pass while silently losing
/// coverage), this harness runs them and only asserts they complete without
/// error — the golden snapshot's job (catching an accidental change to
/// deterministic output) doesn't apply to them anyway.
#[cfg(test)]
mod golden {
    use super::{QplError, Vm, compile_script, load_demo_tables, run_compiled_program, run_script};
    use crate::compiler::namespace_from_path;
    use std::path::{Path, PathBuf};

    /// Fully deterministic — compared byte-for-byte against a golden file.
    const DETERMINISTIC: &[&str] = &[
        "basics",
        "column_expressions",
        "config_and_round",
        "control_flow",
        "lazy_and_collect",
        "lazy_join_pipeline",
        "logging",
        "multiline",
        "namespaces",
        "symbols_and_enums",
        "window_functions",
    ];

    /// Random / now-based output — run for a clean exit only (see module doc).
    /// `functions` is here (not deterministic) because it calls `.qpl.ts`
    /// (wall-clock now) to demonstrate niladic functions.
    const NONDETERMINISTIC: &[&str] = &["functions", "lists", "random_table", "temporal"];

    fn golden_path(name: &str) -> PathBuf {
        Path::new("examples/golden").join(format!("{name}.out"))
    }

    /// Run `examples/<name>.qpl` (or, for `logging`, a copy with its `\1`
    /// target redirected to a scratch file so the test never writes into the
    /// repo) via `run` and return everything it emitted plus the error text
    /// of the first failing statement, if any. `run` is the strategy under
    /// test: `run_script` for the ordinary source-file path, or
    /// a closure that compiles the file to a `.qplc` byte string and back
    /// before running it, to prove that round trip is behaviour-preserving.
    fn run_example_captured_with(
        name: &str,
        run: impl FnOnce(&str, &mut Vm) -> Result<(), QplError>,
    ) -> (String, Option<String>) {
        let mut vm = Vm::new();
        load_demo_tables(&mut vm);
        vm.capture = Some(String::new());

        let is_logging = name == "logging";
        let path: String = if is_logging {
            let src = std::fs::read_to_string("examples/logging.qpl").expect("read logging.qpl");
            let log_target = std::env::temp_dir().join(format!(
                "qpl_golden_logging_{}_{:?}.log",
                std::process::id(),
                std::thread::current().id()
            ));
            let modified = src.replace("\\1 run.log", &format!("\\1 {}", log_target.display()));
            let scratch_script = std::env::temp_dir().join(format!(
                "qpl_golden_logging_{}_{:?}.qpl",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::write(&scratch_script, modified).expect("write scratch logging.qpl");
            scratch_script.to_str().unwrap().to_string()
        } else {
            format!("examples/{name}.qpl")
        };

        let result = run(&path, &mut vm);
        if is_logging {
            let _ = std::fs::remove_file(&path);
        }
        let out = vm.capture.take().unwrap_or_default();
        let err = result.err().map(|e| e.to_string());
        (out, err)
    }

    fn run_example_captured(name: &str) -> (String, Option<String>) {
        run_example_captured_with(name, run_script)
    }

    /// `compile_script(path) -> to_bytes -> from_bytes -> run_compiled_program`
    /// the same compile/run path `qpl -C`
    /// followed by running the resulting `.qplc` exercises, without touching
    /// the filesystem for the intermediate bytes.
    fn run_compiled_roundtrip(path: &str, vm: &mut Vm) -> Result<(), QplError> {
        let program = compile_script(path)?;
        let bytes = program.to_bytes()?;
        let program2 = crate::program::Program::from_bytes(&bytes)?;
        run_compiled_program(vm, std::sync::Arc::new(program2))
    }

    /// Polars group-by (`select ... by ...`) doesn't guarantee output row
    /// order (hash-based grouping), so a couple of the deterministic examples
    /// still vary run-to-run in which order their *rows* come out, even
    /// though the row *contents* are fixed. Rather than special-case those
    /// examples out of the strict comparison, sort each contiguous run of
    /// printed-table data rows (lines starting with the box-drawing `│` that
    /// aren't a separator) before comparing — this still catches any change
    /// to the actual output while ignoring row order the language doesn't
    /// promise anyway.
    fn normalize_table_row_order(s: &str) -> String {
        let mut out = Vec::new();
        let mut run: Vec<&str> = Vec::new();
        let mut in_data_section = false;
        let flush = |run: &mut Vec<&str>, out: &mut Vec<String>| {
            run.sort_unstable();
            out.extend(run.drain(..).map(str::to_string));
        };
        for line in s.lines() {
            if line.starts_with('╞') {
                in_data_section = true;
                out.push(line.to_string());
            } else if line.starts_with('└') {
                flush(&mut run, &mut out);
                in_data_section = false;
                out.push(line.to_string());
            } else if in_data_section && line.starts_with('│') {
                run.push(line);
            } else {
                out.push(line.to_string());
            }
        }
        flush(&mut run, &mut out);
        let mut result = out.join("\n");
        if s.ends_with('\n') {
            result.push('\n');
        }
        result
    }

    fn check_or_update(name: &str, out: &str, err: &Option<String>) {
        let mut content = out.to_string();
        if let Some(e) = err {
            content.push_str("=== ERROR ===\n");
            content.push_str(e);
            content.push('\n');
        }
        let content = normalize_table_row_order(&content);
        let path = golden_path(name);
        if std::env::var("UPDATE_GOLDEN").is_ok() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &content).unwrap();
            return;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!("missing golden file {path:?}; run with UPDATE_GOLDEN=1 to create it")
        });
        assert_eq!(content, expected, "golden output mismatch for '{name}'");
    }

    // Both example groups run inside *one* `#[test]` fn, not two, because
    // `config_and_round` and `random_table` change process-wide
    // `POLARS_FMT_*` env vars via `.qpl.cfg` (see `VmConfig::export_render_limits`)
    // — two separate tests running in parallel threads (cargo test's default)
    // would race on that shared state and make the deterministic comparisons
    // flaky. Keeping everything sequential in one test sidesteps the race
    // without touching the (documented, deliberate) global-env design.
    #[test]
    fn examples_match_golden_output() {
        // Each example runs twice back-to-back — source, then the `.qplc`
        // round trip — rather than as two
        // separate full passes over `DETERMINISTIC`/`NONDETERMINISTIC`: a
        // full second pass would re-run `config_and_round` a second time
        // *after* its process-wide `POLARS_FMT_*` env var mutation already
        // happened once, changing what every later example in that second
        // pass sees relative to the golden file (captured from a single
        // pass). Interleaving keeps each name's two variants observing the
        // same global state the golden file was captured under.
        for name in DETERMINISTIC {
            let (out, err) = run_example_captured(name);
            check_or_update(name, &out, &err);
            let (out, err) = run_example_captured_with(name, run_compiled_roundtrip);
            check_or_update(name, &out, &err);
        }
        for name in NONDETERMINISTIC {
            let (_, err) = run_example_captured(name);
            assert!(err.is_none(), "{name} failed: {err:?}");
            let (_, err) = run_example_captured_with(name, run_compiled_roundtrip);
            assert!(err.is_none(), "{name} (compiled) failed: {err:?}");
        }
    }

    /// `Program::from_bytes(to_bytes(p))` disassembles identically to `p`,
    /// for a real compiled example (not just the small hand-built programs
    /// in `program.rs`'s own tests) — exercises every opcode/operand shape a
    /// realistic script actually emits, including an embedded `\l`/`\i`
    /// sub-program (`namespaces.qpl` imports `namespace_lib.qpl`).
    #[test]
    fn compiled_examples_disassemble_identically_after_a_qplc_round_trip() {
        for name in DETERMINISTIC.iter().chain(NONDETERMINISTIC) {
            let path = format!("examples/{name}.qpl");
            let program = compile_script(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
            let before = crate::program::disassemble(&program);
            let bytes = program.to_bytes().unwrap_or_else(|e| panic!("{name}: {e}"));
            let program2 = crate::program::Program::from_bytes(&bytes)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let after = crate::program::disassemble(&program2);
            assert_eq!(
                before, after,
                "{name} disassembly changed after a round trip"
            );
        }
    }

    /// Run `src` as a scratch script (so errors get the `path:line:` prefix
    /// exactly as a real script would) with a fresh `Vm` (plus demo tables),
    /// and return the error text with the scratch file's own (unpredictable,
    /// pid/thread-based) path replaced by the stable placeholder `<script>`.
    fn run_source_expect_err(src: &str) -> String {
        run_source_expect_err_with(Vm::new(), src)
    }

    fn run_source_expect_err_with(mut vm: Vm, src: &str) -> String {
        load_demo_tables(&mut vm);
        let path = std::env::temp_dir().join(format!(
            "qpl_golden_err_{}_{:?}.qpl",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, src).expect("write scratch script");
        let result = run_script(path.to_str().unwrap(), &mut vm);
        let _ = std::fs::remove_file(&path);
        let msg = result
            .expect_err("expected this scratch script to fail")
            .to_string();
        msg.replace(path.to_str().unwrap(), "<script>")
    }

    /// ~10 error-path snapshots: exact `path:line:`-prefixed
    /// messages for the common failure modes, asserted inline rather than via
    /// golden files since each is a single short string. These lock in
    /// the exact error text.
    /// `f: {[n] f[n]}` recurses through the native Rust call stack today (one
    /// `apply_function` per level), so hitting `MAX_CALL_DEPTH` needs more
    /// headroom than the default test-thread stack reliably provides — run on
    /// an explicitly-sized thread, same as
    /// `vm::tests::unbounded_recursion_hits_the_depth_cap_and_unwinds_cleanly`.
    #[test]
    fn error_paths_have_stable_messages() {
        std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn(error_paths_have_stable_messages_body)
            .unwrap()
            .join()
            .unwrap();
    }

    fn error_paths_have_stable_messages_body() {
        assert_eq!(
            run_source_expect_err("select sym from nosuchtable"),
            "'<script>:1: 'unknown table 'nosuchtable'"
        );
        assert_eq!(
            run_source_expect_err("x: 1\ny: x + nosuchname"),
            "'<script>:2: 'undefined name 'nosuchname' (not a variable, table or lazy frame)"
        );
        assert_eq!(
            run_source_expect_err("f: {[a,b] a + b}\nf[1]"),
            "'<script>:2: 'function 'f' takes 2 argument(s), got 1"
        );
        assert_eq!(
            run_source_expect_err("f: {[n] f[n]}\nf[1]"),
            "'<script>:2: 'function recursion too deep (limit 128)"
        );
        assert_eq!(
            run_source_expect_err("x: noop"),
            "'<script>:1: 'cannot assign a no-op expression."
        );
        assert_eq!(
            run_source_expect_err("?[1; 1; 2]"),
            "'<script>:1: 'a `?[..]` condition must be a boolean scalar or vector in value context"
        );
        assert_eq!(
            run_source_expect_err("while[1; noop]"),
            "'<script>:1: 'a `while` condition must be a boolean scalar in value context"
        );
        assert_eq!(
            run_source_expect_err(".qpl.dt[1]"),
            "'<script>:1: ''.qpl.dt' takes 0 argument(s), got 1"
        );
        // an error partway through a `\i` import rolls the whole session back
        // to its pre-import state — the importing script's own line is what's
        // reported, and the previously-bound name is untouched.
        let import_lib = std::env::temp_dir().join(format!(
            "qpl_golden_err_lib_{}_{:?}.qpl",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&import_lib, "ok: 1\nbad: nosuchname\n").unwrap();
        let importer = format!("\\i \"{}\"\n", import_lib.to_str().unwrap());
        let mut vm = Vm::new();
        load_demo_tables(&mut vm);
        let importer_path = std::env::temp_dir().join(format!(
            "qpl_golden_err_importer_{}_{:?}.qpl",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&importer_path, &importer).unwrap();
        let result = run_script(importer_path.to_str().unwrap(), &mut vm);
        let err = result
            .expect_err("import of a failing script should fail")
            .to_string();
        assert!(
            err.contains("undefined name 'nosuchname'"),
            "unexpected import error text: {err}"
        );
        let ns = namespace_from_path(import_lib.to_str().unwrap());
        assert!(
            !vm.globals.contains_key(&format!("{ns}.ok")),
            "a failed \\i import must leave no partial namespace bindings"
        );
        let _ = std::fs::remove_file(&import_lib);
        let _ = std::fs::remove_file(&importer_path);
    }
}

#[cfg(test)]
mod tests {
    use super::{eval_capture, run_line, run_script_imported, wants_more};
    use crate::ast::Value;
    use crate::compiler::namespace_from_path;
    use crate::parser::{logical_statements, normalize_function_body_newlines};
    use crate::vm::{EvalResult, Vm, run_vm};

    /// Test-only helper: the eager table bound to `name` (panics if it isn't
    /// one) — table-shaped bindings live in `vm.globals`, not a separate map.
    fn table<'a>(vm: &'a Vm, name: &str) -> &'a polars::prelude::DataFrame {
        match vm.globals.get(name) {
            Some(Value::Table(df)) => df,
            other => panic!("'{name}' is not a table binding: {other:?}"),
        }
    }

    /// Run `line` through [`run_line`] and return whatever it wrote via
    /// `Vm::emit`, by pointing the stdout-log tee at a scratch file.
    fn logged(vm: &mut Vm, line: &str) -> String {
        let path =
            std::env::temp_dir().join(format!("qpl_repl_test_{:?}", std::thread::current().id()));
        vm.stdout_log = Some(std::fs::File::create(&path).unwrap());
        run_line(line, vm, "<main>", 0).expect("run_line");
        vm.stdout_log = None;
        let out = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        out.trim_end().to_string()
    }

    #[test]
    fn namespace_from_path_sanitises_the_file_stem() {
        assert_eq!(namespace_from_path("utils.qpl"), ".utils");
        assert_eq!(namespace_from_path("lib/my-lib.qpl"), ".my_lib");
        assert_eq!(namespace_from_path("lib/9lives.qpl"), "._9lives");
    }

    #[test]
    fn i_import_namespaces_new_functions_globals_and_tables() {
        let path =
            std::env::temp_dir().join(format!("qpl_i_test_{:?}.qpl", std::thread::current().id()));
        std::fs::write(&path, "greeting: \"hi\"\ndouble: {[x] x*2}\n").unwrap();
        let mut vm = Vm::new();
        run_script_imported(path.to_str().unwrap(), &mut vm).expect("run_script_imported");
        let _ = std::fs::remove_file(&path);

        let ns = namespace_from_path(path.to_str().unwrap());
        let ns_greeting = format!("{ns}.greeting");
        let ns_double = format!("{ns}.double");
        assert!(
            vm.globals.contains_key(&ns_greeting),
            "{:?}",
            vm.globals.keys().collect::<Vec<_>>()
        );
        assert!(matches!(
            vm.globals.get(&ns_double),
            Some(crate::ast::Value::Closure(_))
        ));
        assert!(!vm.globals.contains_key("greeting"));
        assert!(!vm.globals.contains_key("double"));
    }

    #[test]
    fn i_import_lets_a_namespaced_function_call_an_unqualified_sibling() {
        // regression: `\i` renames a script's top-level bindings to `.ns.*`
        // but doesn't rewrite cross-references *inside* their bodies, so a
        // function calling another top-level helper by its bare name used to
        // break after import with "'<helper>' is not a function".
        let path = std::env::temp_dir().join(format!(
            "qpl_i_sibling_test_{:?}.qpl",
            std::thread::current().id()
        ));
        std::fs::write(&path, "_log: {[s] s}\ninfo: {[s] _log[s]}\n").unwrap();
        let mut vm = Vm::new();
        run_script_imported(path.to_str().unwrap(), &mut vm).expect("run_script_imported");
        let _ = std::fs::remove_file(&path);

        let ns = namespace_from_path(path.to_str().unwrap());
        run_line(&format!("l: {ns}.info \"hi\""), &mut vm, "<main>", 0)
            .expect("call should resolve");
        assert_eq!(
            vm.globals.get("l"),
            Some(&crate::ast::Value::Str("hi".into()))
        );
    }

    #[test]
    fn i_import_does_not_double_namespace_an_already_namespaced_binding() {
        // a script that itself `\i`s another script leaves that nested import's
        // already-namespaced bindings alone rather than re-prefixing them.
        let mut vm = Vm::new();
        vm.globals
            .insert(".inner.x".into(), crate::ast::Value::Int(1));
        let path = std::env::temp_dir().join(format!(
            "qpl_i_nested_test_{:?}.qpl",
            std::thread::current().id()
        ));
        std::fs::write(&path, "y: 2\n").unwrap();
        run_script_imported(path.to_str().unwrap(), &mut vm).expect("run_script_imported");
        let _ = std::fs::remove_file(&path);

        assert!(vm.globals.contains_key(".inner.x"));
        assert!(!vm.globals.contains_key("y"));
    }

    #[test]
    fn i_command_requires_a_quoted_path() {
        let path = std::env::temp_dir().join(format!(
            "qpl_i_quoted_test_{:?}.qpl",
            std::thread::current().id()
        ));
        std::fs::write(&path, "z: 1\n").unwrap();
        let mut vm = Vm::new();

        // bare/unquoted is rejected with a clear message
        let err = run_line(
            &format!("\\i {}", path.to_str().unwrap()),
            &mut vm,
            "<main>",
            0,
        )
        .expect_err("bare path should be rejected");
        assert!(
            matches!(&err, crate::errors::QplError::Runtime(m) if m.contains("quoted path")),
            "{err:?}"
        );

        // quoted works, both at the REPL and (via run_script -> run_line) nested in a script
        run_line(
            &format!("\\i \"{}\"", path.to_str().unwrap()),
            &mut vm,
            "<main>",
            0,
        )
        .expect("quoted path should import");
        let _ = std::fs::remove_file(&path);
        let ns = namespace_from_path(path.to_str().unwrap());
        assert!(vm.globals.contains_key(&format!("{ns}.z")));
    }

    #[test]
    fn d_command_prints_a_disassembly_listing() {
        let mut vm = Vm::new();
        let (out, err) = eval_capture("\\d 1+1", &mut vm);
        assert!(err.is_none(), "{err:?}");
        assert!(out.contains("BINOP"), "{out:?}");
        assert!(out.contains("PUSH"), "{out:?}");
    }

    #[test]
    fn embedded_l_target_survives_deleting_the_source_after_compile() {
        // a `\l`/`\i` target is read, parsed and
        // compiled into the *including* script's own `Program` at compile
        // time — the source file plays no further role once that's done.
        let dir = scratch_dir("l_embed");
        let lib = dir.join("lib.qpl");
        std::fs::write(&lib, "x: 41\n").unwrap();
        let main = dir.join("main.qpl");
        std::fs::write(&main, format!("\\l {}\ny: x + 1\n", lib.to_str().unwrap())).unwrap();

        let src = std::fs::read_to_string(&main).unwrap();
        let stmts = crate::parser::parse_program(&src, main.to_str().unwrap()).unwrap();
        let ctx = crate::compiler::CompileCtx::script(main.to_str().unwrap());
        let program = crate::compiler::compile_program(stmts, ctx).unwrap();

        // the target no longer exists on disk once compilation is done
        std::fs::remove_file(&lib).unwrap();

        let mut vm = Vm::new();
        vm.run_compiled(std::sync::Arc::new(program))
            .expect("embedded program should run with no source file present");
        assert_eq!(vm.globals.get("y"), Some(&crate::ast::Value::Int(42)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn l_cycle_is_a_compile_time_error() {
        let dir = scratch_dir("l_cycle");
        let a = dir.join("a.qpl");
        let b = dir.join("b.qpl");
        std::fs::write(&a, format!("\\l \"{}\"\n", b.to_str().unwrap())).unwrap();
        std::fs::write(&b, format!("\\l \"{}\"\n", a.to_str().unwrap())).unwrap();
        let mut vm = Vm::new();
        let err = super::run_script(a.to_str().unwrap(), &mut vm).expect_err("cycle");
        assert!(err.to_string().contains("cycle"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn i_cycle_is_a_compile_time_error() {
        let dir = scratch_dir("i_cycle");
        let a = dir.join("a.qpl");
        let b = dir.join("b.qpl");
        std::fs::write(&a, format!("\\i \"{}\"\n", b.to_str().unwrap())).unwrap();
        std::fs::write(&b, format!("\\i \"{}\"\n", a.to_str().unwrap())).unwrap();
        let mut vm = Vm::new();
        let err = super::run_script(a.to_str().unwrap(), &mut vm).expect_err("cycle");
        assert!(err.to_string().contains("cycle"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_parse_error_on_a_later_line_prevents_earlier_statements_from_running() {
        // the whole script is parsed before anything runs, so `x` is never bound.
        let dir = scratch_dir("parse_abort");
        let path = dir.join("bad.qpl");
        std::fs::write(&path, "x: 1\nsel from t\n").unwrap();
        let mut vm = Vm::new();
        let err = super::run_script(path.to_str().unwrap(), &mut vm).expect_err("parse error");
        assert!(err.to_string().contains(":2:"), "{err}");
        assert!(
            !vm.globals.contains_key("x"),
            "an earlier statement must not have run"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_param_shadows_a_namespaced_name_of_the_same_import() {
        // compile-time namespace qualification: inside a function
        // body, a param of the same name as one of the file's own top-level
        // bindings refers to the param, not `.lib.x`.
        let dir = scratch_dir("i_param_shadow");
        let lib = dir.join("lib.qpl");
        std::fs::write(&lib, "x: 100\nf: {[x] x+1}\n").unwrap();
        let mut vm = Vm::new();
        run_script_imported(lib.to_str().unwrap(), &mut vm).expect("import");
        run_line("r: .lib.f[1]", &mut vm, "<main>", 0)
            .map_err(|e| e.to_string())
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(vm.globals.get("r"), Some(&crate::ast::Value::Int(2)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(all(feature = "ipc", feature = "cli"))]
    #[test]
    fn dispatch_supports_qpl_cfg() {
        let mut vm = Vm::new();
        match super::eval_for_dispatch(".qpl.cfg", &mut vm) {
            Ok(EvalResult::Stored) => {}
            other => panic!("expected Stored, got {other:?}"),
        }
    }

    #[cfg(all(feature = "ipc", feature = "cli"))]
    #[test]
    fn dispatch_rejects_system_commands() {
        let mut vm = Vm::new();
        let err = super::eval_for_dispatch("\\l some/script.qpl", &mut vm)
            .expect_err("\\l should be rejected over a dispatched connection");
        assert!(err.to_string().contains("dispatched"), "{err}");
    }

    #[test]
    fn stack_height_is_unchanged_across_a_whole_script() {
        let dir = scratch_dir("stack_height");
        let path = dir.join("s.qpl");
        std::fs::write(
            &path,
            "x: 1\nlog x\nf: {[a] a*2}\ny: f[x]\nselect from trades\nlog[\"done\"]\n.qpl.cfg\n",
        )
        .unwrap();
        let mut vm = Vm::new();
        super::load_demo_tables(&mut vm);
        super::run_script(path.to_str().unwrap(), &mut vm).expect("script should run cleanly");
        assert!(
            vm.stack.is_empty(),
            "the VM stack must be empty after a whole Script-mode program runs"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A fresh scratch directory for one test's script files.
    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("qpl_{tag}_{:?}", std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn i_import_never_overwrites_an_existing_session_name() {
        // regression: a library assigning `thr` must not silently replace the
        // session's own `thr`; it lands under `.lib`.
        //
        // Namespace qualification happens at compile time: a bare
        // reference to a name the file binds at its own top level is *always*
        // qualified, regardless of where in the file it appears relative to
        // that binding (so a library statement can't read the
        // session's value of a name it is about to redefine under the same
        // top-level binding). This script therefore reads the *session's*
        // `src` (never one of `lib`'s own top-level names) to avoid that
        // ill-defined case, and asserts the same core guarantee: an import
        // never touches an existing session name, and its own bindings land
        // under `.lib.*`.
        let dir = scratch_dir("i_clobber");
        let lib = dir.join("lib.qpl");
        std::fs::write(
            &lib,
            "thr: 99\nout: select from src where c > 1\nn: count out\n",
        )
        .unwrap();
        let mut vm = Vm::new();
        vm.globals.insert(
            "src".into(),
            Value::Table(polars::df!["c" => [1i64, 2, 3]].unwrap()),
        );
        vm.globals.insert("thr".into(), crate::ast::Value::Int(1));
        run_script_imported(lib.to_str().unwrap(), &mut vm).expect("import");

        assert_eq!(vm.globals.get("thr"), Some(&crate::ast::Value::Int(1)));
        assert_eq!(
            vm.globals.get(".lib.thr"),
            Some(&crate::ast::Value::Int(99))
        );
        assert_eq!(table(&vm, "src").height(), 3, "session table untouched");
        assert_eq!(table(&vm, ".lib.out").height(), 2);
        assert_eq!(vm.globals.get(".lib.n"), Some(&crate::ast::Value::Int(2)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn i_import_sibling_is_not_hijacked_by_a_session_name() {
        // regression: the namespaced fallback used to run only after the
        // session's own bindings, so a session `_log` bound after the import
        // replaced the library's `_log` inside `.lg.info`.
        let dir = scratch_dir("i_hijack");
        let lib = dir.join("lg.qpl");
        std::fs::write(&lib, "_log: {[s] s}\ninfo: {[s] _log[s]}\n").unwrap();
        let mut vm = Vm::new();
        run_script_imported(lib.to_str().unwrap(), &mut vm).expect("import");
        run_line("_log: {[s] 0}", &mut vm, "<main>", 0).unwrap();
        run_line(r#"l: .lg.info "hi""#, &mut vm, "<main>", 0).unwrap();
        assert_eq!(
            vm.globals.get("l"),
            Some(&crate::ast::Value::Str("hi".into()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn i_import_is_rolled_back_when_the_script_fails() {
        // regression: a failing import used to leave everything bound before
        // the failure in the session under its *bare* name.
        let dir = scratch_dir("i_rollback");
        let lib = dir.join("bad.qpl");
        std::fs::write(&lib, "good: 1\nthr: 5\noops: nosuchname + 1\n").unwrap();
        let mut vm = Vm::new();
        vm.globals.insert("thr".into(), crate::ast::Value::Int(1));
        vm.globals
            .insert(".bad.old".into(), crate::ast::Value::Int(7));
        run_script_imported(lib.to_str().unwrap(), &mut vm).expect_err("import should fail");

        assert!(!vm.globals.contains_key("good"));
        assert!(!vm.globals.contains_key(".bad.good"));
        assert_eq!(vm.globals.get("thr"), Some(&crate::ast::Value::Int(1)));
        // the previous import's contents survive a failed re-import
        assert_eq!(vm.globals.get(".bad.old"), Some(&crate::ast::Value::Int(7)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn i_reimport_replaces_the_namespace_wholesale() {
        let dir = scratch_dir("i_reimport");
        let lib = dir.join("u.qpl");
        std::fs::write(&lib, "a: 1\nb: 2\n").unwrap();
        let mut vm = Vm::new();
        run_script_imported(lib.to_str().unwrap(), &mut vm).expect("first import");
        std::fs::write(&lib, "a: 10\n").unwrap();
        run_script_imported(lib.to_str().unwrap(), &mut vm).expect("re-import");
        assert_eq!(vm.globals.get(".u.a"), Some(&crate::ast::Value::Int(10)));
        assert!(
            !vm.globals.contains_key(".u.b"),
            "a binding dropped from the library goes away"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nested_l_and_i_paths_are_relative_to_the_including_script() {
        // regression: `\l`/`\i` inside a script resolved against the working
        // directory, not the script's own location.
        let dir = scratch_dir("i_relpath");
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/helpers.qpl"), "twice: {[x] x*2}\n").unwrap();
        std::fs::write(dir.join("lib/flat.qpl"), "f: 3\n").unwrap();
        std::fs::write(
            dir.join("lib/report.qpl"),
            "\\i \"helpers.qpl\"\n\\l flat.qpl\nr: .helpers.twice[f]\n",
        )
        .unwrap();
        let mut vm = Vm::new();
        // imported by absolute path, from a working directory that is not `dir`
        run_script_imported(dir.join("lib/report.qpl").to_str().unwrap(), &mut vm).expect("import");
        assert_eq!(
            vm.globals.get(".report.r"),
            Some(&crate::ast::Value::Int(6))
        );
        assert_eq!(
            vm.globals.get(".report.f"),
            Some(&crate::ast::Value::Int(3)),
            "`\\l` inside an import lands in its namespace"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn script_relative_leaves_prompt_and_absolute_paths_alone() {
        use crate::compiler::script_relative;
        assert_eq!(script_relative("a.qpl", "<main>"), "a.qpl");
        assert_eq!(script_relative("/x/a.qpl", "lib/b.qpl"), "/x/a.qpl");
        assert_eq!(script_relative("a.qpl", "b.qpl"), "a.qpl");
        assert_eq!(
            script_relative("a.qpl", "lib/b.qpl"),
            std::path::Path::new("lib").join("a.qpl").to_string_lossy()
        );
    }

    #[cfg(feature = "ipc")]
    #[test]
    fn read_handle_can_print_but_not_change_config() {
        // regression: `.qpl.cfg key=value` bypassed the read-only gate, so a
        // read handle could change e.g. `round_type`, which changes answers.
        use crate::ipc::HandleMode;
        let mut vm = Vm::new();
        vm.with_request_permission(HandleMode::Read, |vm| {
            super::eval_for_dispatch(".qpl.cfg", vm)
        })
        .expect("printing the settings is a read");
        let err = vm
            .with_request_permission(HandleMode::Read, |vm| {
                super::eval_for_dispatch(".qpl.cfg round_type=HALF_UP", vm)
            })
            .expect_err("changing a knob is a write");
        assert!(
            matches!(&err, crate::errors::QplError::Runtime(m) if m.contains("read-only")),
            "{err:?}"
        );
        assert!(vm.config.describe().contains("round_type=HALF_TO_EVEN"));
        vm.with_request_permission(HandleMode::Write, |vm| {
            super::eval_for_dispatch(".qpl.cfg round_type=HALF_UP", vm)
        })
        .expect("a write handle may change it");
    }

    #[test]
    fn log_evaluates_a_reduction_directly() {
        // regression: `log max t`price` (with or without the parens the README
        // recommends for a call/reduction) must evaluate the
        // table/column expression, not just a plain scalar fold.
        let mut vm = Vm::new();
        vm.globals.insert(
            "t".into(),
            Value::Table(polars::df!["price" => [1.0f64, 2.0, 3.0]].unwrap()),
        );
        assert_eq!(logged(&mut vm, "log (max t`price)"), "3");
    }

    #[test]
    fn log_bracket_call_concatenates_like_the_bareword_form() {
        let mut vm = Vm::new();
        assert_eq!(logged(&mut vm, r#"log["a" "b"]"#), "ab");
    }

    #[test]
    fn log_bracket_call_is_usable_inside_a_function_body() {
        // regression: `log` only ever existed as a whole-line REPL directive
        // (`repl::log_target`), so it was unreachable from inside a function
        // body — `{[s] log s}` failed with "unknown function 'log'". `log[..]`
        // now compiles through the ordinary `Expr::Call` path
        // (`compiler::compile_value_expr`'s `log` arm, `ops::native_log`), so
        // it works there too.
        let mut vm = Vm::new();
        run_line(
            r#"info: {[s] log[str$"tag" " - " s]}"#,
            &mut vm,
            "<main>",
            0,
        )
        .expect("define info");
        // `info` returns whatever `log` wrote (its only/last statement), so a
        // bare call at top level also echoes that return value like any other
        // function call — only a standalone `log[..]` statement suppresses it.
        assert_eq!(
            logged(&mut vm, r#"info["hi"]"#),
            "tag - hi\nstr: \"tag - hi\""
        );
    }

    #[test]
    fn log_bracket_call_as_a_bare_statement_does_not_echo_its_return_value() {
        // unlike an ordinary function call, a standalone `log[..]` statement
        // only prints what it logged, matching the bareword `log ..` form.
        let mut vm = Vm::new();
        assert_eq!(logged(&mut vm, r#"log["only once"]"#), "only once");
    }

    #[test]
    fn log_bracket_call_supports_zero_one_and_three_args() {
        // every arity compiles the same way (`compiler::compile_value_expr`'s `log`
        // arm) and behaves identically as a top-level statement.
        let mut vm = Vm::new();
        assert_eq!(logged(&mut vm, "log[]"), "");
        assert_eq!(logged(&mut vm, r#"log["solo"]"#), "solo");
        assert_eq!(logged(&mut vm, r#"log["a";"b";"c"]"#), "abc");
    }

    #[test]
    fn log_bareword_supports_zero_one_and_three_args() {
        let mut vm = Vm::new();
        assert_eq!(logged(&mut vm, "log"), "");
        assert_eq!(logged(&mut vm, r#"log "solo""#), "solo");
        // the bareword form takes a single expression, so three values are
        // written as one string-concatenation expression rather than three
        // comma/semicolon-separated arguments (that's `log[..]`'s job above).
        assert_eq!(logged(&mut vm, r#"log "a" "b" "c""#), "abc");
    }

    #[cfg(feature = "ipc")]
    #[test]
    fn dispatch_evaluates_an_expression_starting_with_a_digit() {
        // regression: `render_tokens` renders `1+1` as `1 + 1`, which a
        // stdout-write shorthand keyed on a leading "1 " used to misread as a
        // directive rather than as the literal 1. `log` is now the only
        // spelling that writes, so dispatched text like this is unambiguous.
        let mut vm = Vm::new();
        match super::eval_for_dispatch("1 + 1", &mut vm) {
            Ok(crate::vm::EvalResult::Scalar(crate::ast::Value::Int(2))) => {}
            other => panic!("expected Scalar(2), got {other:?}"),
        }
    }

    #[cfg(feature = "ipc")]
    #[test]
    fn dispatch_still_supports_the_log_keyword_shorthand() {
        let mut vm = Vm::new();
        let path = std::env::temp_dir().join(format!(
            "qpl_dispatch_log_test_{:?}",
            std::thread::current().id()
        ));
        vm.stdout_log = Some(std::fs::File::create(&path).unwrap());
        super::eval_for_dispatch(r#"log "hi""#, &mut vm).expect("eval_for_dispatch");
        vm.stdout_log = None;
        let out = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(out.trim_end(), "hi");
    }

    #[cfg(feature = "ipc")]
    #[test]
    fn dispatch_runs_an_ordinary_statement() {
        let mut vm = Vm::new();
        vm.globals.insert(
            "t".into(),
            Value::Table(polars::df!["c" => [1i64, 2, 3]].unwrap()),
        );
        match super::eval_for_dispatch("select c from t where c > 1", &mut vm) {
            Ok(crate::vm::EvalResult::Table(df)) => assert_eq!(df.height(), 2),
            other => panic!("expected a table, got {other:?}"),
        }
    }

    #[test]
    fn wants_more_detects_unfinished_input() {
        // complete
        assert!(!wants_more("select from trades"));
        assert!(!wants_more("x: 1 + 2"));
        // unbalanced bracket / paren
        assert!(wants_more("select a: ?[c>1;`x"));
        assert!(wants_more("select (1 + "));
        // trailing comma
        assert!(wants_more("select a: price,"));
        // cut off before `from`
        assert!(wants_more("select price"));
        assert!(wants_more("select price from"));
        // unterminated string
        assert!(wants_more("log \"oops"));
        // a trailing operator / a dict literal still missing values: parse ran out in a primary
        assert!(wants_more("x: 1 +"));
        assert!(wants_more("zip `a`b!"));
        assert!(wants_more("zip `a`b!(1 2 3)"));
        assert!(!wants_more("zip `a`b!(1 2 3) (4 5 6)"));
        // a real syntax error is NOT "more" — surface it now
        assert!(!wants_more("selct from trades"));
        // blank / comment-only input is finished (a no-op), not unfinished
        assert!(!wants_more("/ just a comment"));
        assert!(!wants_more(""));
        // a terminal lex error must not hang the prompt waiting for input
        assert!(!wants_more("select from t where a = 1 @"));
        // `\` commands are always single-line
        assert!(!wants_more("\\d select a: ?[c>1;`x"));
        assert!(!wants_more("\\l some/script.qpl"));
        // `.qpl.cfg` is a single-line directive, never "more"
        assert!(!wants_more(".qpl.cfg maxrow=5 maxcol=3"));
    }

    #[test]
    fn single_line_statements_pass_through() {
        let got = logical_statements("select from trades\nselect from quotes\n");
        assert_eq!(
            got,
            vec![
                (0, "select from trades".to_string()),
                (1, "select from quotes".to_string()),
            ]
        );
    }

    #[test]
    fn blank_and_comment_lines_are_dropped() {
        let got = logical_statements("\n/ a comment\nselect from trades\n\n/ another\n");
        assert_eq!(got, vec![(2, "select from trades".to_string())]);
    }

    #[test]
    fn four_space_indent_continues_statement() {
        let src = "t: select a, b\n    by sym\n    where a > 1\nselect from t";
        let got = logical_statements(src);
        assert_eq!(
            got,
            vec![
                (0, "t: select a, b\n    by sym\n    where a > 1".to_string()),
                (3, "select from t".to_string()),
            ]
        );
    }

    #[test]
    fn tab_indent_continues_statement() {
        let got = logical_statements("select a\n\tfrom trades");
        assert_eq!(got, vec![(0, "select a\n\tfrom trades".to_string())]);
    }

    #[test]
    fn blank_line_terminates_multiline_statement() {
        let src = "select a\n    from trades\n\nselect from quotes";
        let got = logical_statements(src);
        assert_eq!(
            got,
            vec![
                (0, "select a\n    from trades".to_string()),
                (3, "select from quotes".to_string()),
            ]
        );
    }

    #[test]
    fn shallow_indent_is_a_new_statement() {
        // one/two/three spaces is not a continuation
        let got = logical_statements("select from trades\n  select from quotes");
        assert_eq!(
            got,
            vec![
                (0, "select from trades".to_string()),
                (1, "  select from quotes".to_string()),
            ]
        );
    }

    #[test]
    fn leading_indent_with_no_open_statement_starts_one() {
        let got = logical_statements("    select from trades");
        assert_eq!(got, vec![(0, "    select from trades".to_string())]);
    }

    #[test]
    fn indented_comment_inside_statement_is_kept_as_continuation() {
        let src = "select a\n    / pick columns\n    from trades";
        let got = logical_statements(src);
        assert_eq!(
            got,
            vec![(
                0,
                "select a\n    / pick columns\n    from trades".to_string()
            ),]
        );
    }

    #[test]
    fn function_body_newlines_become_implicit_semicolons() {
        let src = "f: {[x]\n    a: x+1\n    b: a*2\n    b\n    }";
        assert_eq!(
            normalize_function_body_newlines(src),
            "f: {[x]\n    a: x+1\n;    b: a*2\n;    b\n    }"
        );
    }

    #[test]
    fn function_body_deeper_indent_stays_a_continuation() {
        // `by`/`from`/`order` are more deeply indented than the `select`
        // above them, so they extend that one statement rather than starting
        // new ones.
        let src = "f: {[x]\n    select tot: sum x\n        by sym\n        from t\n    }";
        assert_eq!(
            normalize_function_body_newlines(src),
            "f: {[x]\n    select tot: sum x\n        by sym\n        from t\n    }"
        );
    }

    #[test]
    fn function_body_explicit_semicolon_still_works_and_a_redundant_one_is_harmless() {
        // an already-`;`-terminated line followed by a new baseline-indent
        // line just doubles up (`;;`), which the parser tolerates.
        let src = "f: {[x]\n    a: x+1;\n    a\n    }";
        assert_eq!(
            normalize_function_body_newlines(src),
            "f: {[x]\n    a: x+1;\n;    a\n    }"
        );
        let mut vm = Vm::new();
        run_line(src, &mut vm, "<main>", 0).expect("run_line");
        match run_vm("f[5]", &mut vm) {
            Ok(EvalResult::Scalar(v)) => assert_eq!(v, crate::ast::Value::Int(6)),
            other => panic!("expected Scalar(6), got {other:?}"),
        }
    }

    #[test]
    fn single_line_function_definition_is_untouched() {
        let src = "add: {[x,y] x+y}";
        assert_eq!(normalize_function_body_newlines(src), src);
    }

    #[test]
    fn blank_and_comment_lines_inside_a_function_body_are_not_new_statements() {
        let src = "f: {[x]\n    a: x+1\n\n    / a comment\n    a\n    }";
        assert_eq!(
            normalize_function_body_newlines(src),
            "f: {[x]\n    a: x+1\n\n    / a comment\n;    a\n    }"
        );
    }

    #[test]
    fn multiline_function_body_runs_without_explicit_semicolons() {
        // the original report: a function body written one statement per
        // line, indented, with no `;` at all — should behave exactly like
        // the semicolon-separated single-line form.
        let mut vm = Vm::new();
        let src = "f: {[x]\n    a: x+1\n    b: a*2\n    b\n    }";
        run_line(src, &mut vm, "<main>", 0).expect("run_line");
        match run_vm("f[5]", &mut vm) {
            Ok(EvalResult::Scalar(v)) => assert_eq!(v, crate::ast::Value::Int(12)),
            other => panic!("expected Scalar(12), got {other:?}"),
        }
    }

    /// `eval_capture` must return exactly what the CLI would have printed —
    /// including output emitted *before* a statement failed — and must leave
    /// the VM's capture state as it found it.
    #[test]
    fn eval_capture_returns_output_and_error_separately() {
        let mut vm = Vm::new();
        assert_eq!(eval_capture("x: 41", &mut vm), (String::new(), None));
        assert_eq!(
            eval_capture("x + 1", &mut vm),
            ("i64: 42\n".to_string(), None)
        );

        let (out, err) = eval_capture("nosuchtable", &mut vm);
        assert!(out.is_empty(), "failed statement emitted {out:?}");
        assert!(err.is_some_and(|e| e.contains("nosuchtable")));
        assert!(vm.capture.is_none(), "capture buffer outlived the call");
    }

    /// Blank and comment-only submissions are no-ops, as at the terminal REPL —
    /// a host feeding a script line by line submits them too. A comment above
    /// code in the same submission still runs the code.
    #[test]
    fn eval_capture_ignores_blank_and_comment_only_input() {
        let mut vm = Vm::new();
        for src in [
            "",
            "   ",
            "\n",
            "/ just a note",
            "  / indented\n/ two lines",
        ] {
            assert_eq!(eval_capture(src, &mut vm), (String::new(), None), "{src:?}");
        }
        assert_eq!(eval_capture("/ note\nx: 5", &mut vm), (String::new(), None));
        assert_eq!(eval_capture("x", &mut vm), ("i64: 5\n".to_string(), None));
    }

    /// The example script, fed the way the terminal REPL feeds it (line by
    /// line, accumulating while `wants_more`, blank/comment lines included)
    /// — what a wasm host driving `eval`/`wantsMore` does.
    #[test]
    fn a_multiline_script_runs_when_fed_line_by_line_with_wants_more() {
        let src = "n: 3\n\n/ note\nt: zip `a`b!\n    (til n)\n    (n ? 5)\n\nt\n";
        let mut vm = Vm::new();
        let mut buf = String::new();
        for line in src.lines() {
            buf = if buf.is_empty() {
                line.to_string()
            } else {
                format!("{buf}\n{line}")
            };
            if wants_more(&buf) {
                continue;
            }
            let (_, err) = eval_capture(&buf, &mut vm);
            assert!(err.is_none(), "{buf:?}: {err:?}");
            buf.clear();
        }
        assert_eq!(table(&vm, "t").shape(), (3, 2));
    }

    /// A `log` write happens before the statement's own failure, so it has to
    /// survive in the captured output rather than being discarded with it.
    #[test]
    fn eval_capture_keeps_output_emitted_before_a_failure() {
        let mut vm = Vm::new();
        eval_capture("f: {[] log[\"before\"]; nosuchtable}", &mut vm);
        let (out, err) = eval_capture("f[]", &mut vm);
        assert_eq!(out, "before\n");
        assert!(err.is_some());
    }

    #[test]
    fn while_may_span_lines_in_a_script_and_in_the_repl() {
        // script: indented continuation lines fold into the one statement
        let stmts = logical_statements("while[k<3;\n    log k;\n    k: k+1]\nx: 1\n");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].1.contains("k: k+1]"));
        // REPL: an open bracket keeps reading
        assert!(wants_more("while[k<3;"));
        assert!(wants_more("while[k<3;\n  k: k+1"));
        assert!(!wants_more("while[k<3;\n  k: k+1]"));

        let mut vm = Vm::new();
        run_line("k: 0", &mut vm, "<main>", 0).unwrap();
        let (out, err) = eval_capture("while[k<3;\n    log[k];\n    k: k+1]", &mut vm);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(out, "0\n1\n2\n");
    }

    #[test]
    fn run_script_stops_at_an_interrupted_statement() {
        let path = std::env::temp_dir().join(format!(
            "qpl_interrupt_test_{:?}.qpl",
            std::thread::current().id()
        ));
        std::fs::write(&path, "before: 1\nwhile[1b; noop]\nafter: 1\n").unwrap();
        let mut vm = Vm::new();
        let interrupt = vm.interrupt.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            interrupt.request();
        });
        let err = super::run_script(path.to_str().unwrap(), &mut vm).expect_err("interrupted");
        t.join().unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(err, crate::errors::QplError::Interrupted),
            "{err:?}"
        );
        assert!(vm.globals.contains_key("before"));
        assert!(
            !vm.globals.contains_key("after"),
            "later lines must not run"
        );
    }

    #[test]
    fn while_and_noop_print_nothing() {
        let mut vm = Vm::new();
        let (out, err) = eval_capture("n: 0", &mut vm);
        assert_eq!((out.as_str(), err), ("", None));
        let (out, err) = eval_capture("while[n<3; n: n+1]", &mut vm);
        assert_eq!((out.as_str(), err), ("", None));
        let (out, err) = eval_capture("noop", &mut vm);
        assert_eq!((out.as_str(), err), ("", None));
    }

    #[test]
    fn a_noop_assignment_reports_the_error_and_the_session_continues() {
        let mut vm = Vm::new();
        let (out, err) = eval_capture("x: noop", &mut vm);
        assert_eq!(out, "");
        assert_eq!(err.as_deref(), Some("'cannot assign a no-op expression."));
        let (out, err) = eval_capture("1+1", &mut vm);
        assert_eq!((out.as_str(), err), ("i64: 2\n", None));
    }

    #[test]
    fn an_interrupted_statement_reports_interrupted_and_the_session_continues() {
        let mut vm = Vm::new();
        let interrupt = vm.interrupt.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            interrupt.request();
        });
        let (out, err) = eval_capture("while[1b; noop]", &mut vm);
        t.join().unwrap();
        assert_eq!(out, "");
        assert_eq!(err.as_deref(), Some("'interrupted"));
        let (out, err) = eval_capture("1+1", &mut vm);
        assert_eq!((out.as_str(), err), ("i64: 2\n", None));
    }

    #[test]
    fn an_interrupt_in_a_loaded_script_keeps_its_variant() {
        // `\l` runs the script under its own path, which normally wraps a
        // failure as `path:line: ..` text — an interrupt must stay an interrupt.
        let path = std::env::temp_dir().join(format!(
            "qpl_interrupt_nested_{:?}.qpl",
            std::thread::current().id()
        ));
        std::fs::write(&path, "while[1b; noop]\n").unwrap();
        let mut vm = Vm::new();
        let interrupt = vm.interrupt.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            interrupt.request();
        });
        let err = run_line(
            &format!("\\l {}", path.to_str().unwrap()),
            &mut vm,
            "<main>",
            0,
        )
        .expect_err("interrupted");
        t.join().unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(err, crate::errors::QplError::Interrupted),
            "{err:?}"
        );
        // and the outer statement's guard has been released
        assert!(vm.interrupt.check().is_ok());
    }

    #[test]
    fn a_script_error_in_a_while_body_names_the_script_line() {
        let path = std::env::temp_dir().join(format!(
            "qpl_while_err_{:?}.qpl",
            std::thread::current().id()
        ));
        std::fs::write(&path, "ok: 1\nwhile[1b; nosuch[1]]\n").unwrap();
        let mut vm = Vm::new();
        let err = super::run_script(path.to_str().unwrap(), &mut vm).expect_err("should fail");
        let _ = std::fs::remove_file(&path);
        assert!(err.to_string().contains(":2:"), "{err}");
    }

    #[test]
    fn a_while_with_a_trailing_comment_and_blank_continuation_folds_into_one_statement() {
        let stmts = logical_statements("while[k<2;   / loop\n    k: k+1]\n\nafter: 1\n");
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].1.contains("k: k+1]"));
    }
}
