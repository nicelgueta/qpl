use crate::ast::self;
use crate::compiler::compile;
use crate::errors::QplError;
use crate::lexer::tokenise;
use crate::parser::{parse, parse_expr_seq};
use crate::tokens::{Token, TokenKind};
use crate::temporal;
use crate::opcodes::disassemble_instructions;
use crate::vm::{Vm, run_vm, EvalResult};
use crate::resolve;
use polars::prelude::*;
use rustyline::{DefaultEditor, error::ReadlineError};
use std::collections::HashMap;

/// Run a `.qpl` script file, printing results. Returns Err on the first failure.
pub fn run_script(path: &str, vm: &mut Vm) -> Result<(), QplError> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| QplError::Runtime(format!("cannot read '{path}': {e}")))?;
    for (lineno, stmt) in logical_statements(&src) {
        run_line(&stmt, vm, path, lineno)?;
    }
    Ok(())
}

/// `\d` / `\l` / `\i` / `\1` / an ordinary statement — everything a submitted
/// line can be *except* `\port`, which needs REPL-loop state (`PortSession`)
/// this function doesn't have. Shared by the REPL loop (interactive input and,
/// once a port is open, the polling loop's stdin lines) and [`run_script`], so
/// `\l`/`\i` also work nested inside a loaded/imported script, not just typed
/// at the prompt.
fn run_line(src: &str, vm: &mut Vm, path: &str, lineno: usize) -> Result<(), QplError> {
    if let Some(inner) = src.strip_prefix("\\d").map(str::trim) {
        println!("{}", disassemble(inner)?);
        return Ok(());
    }
    if let Some(target) = src.strip_prefix("\\l").map(str::trim) {
        return run_script(target, vm);
    }
    if let Some(target) = src.strip_prefix("\\i").map(str::trim) {
        return run_script_imported(&parse_quoted_path(target)?, vm);
    }
    if let Some(result) = system_command(src, vm) {
        return result;
    }
    match_run_vm(src, vm, path, lineno)
}

/// `\i`'s path argument is a quoted string (`\i "lib/utils.qpl"`), unlike
/// `\l`'s bare one — the namespace derives from it (see
/// [`namespace_from_path`]), so writing it as a string keeps that visually
/// distinct from an ordinary namespaced identifier appearing right after
/// `\i` on the same line.
fn parse_quoted_path(rest: &str) -> Result<String, QplError> {
    match tokenise(rest)?.as_slice() {
        [Token { kind: TokenKind::Str(s), .. }] => Ok(s.clone()),
        _ => Err(QplError::Runtime(format!(
            "\\i expects a quoted path, e.g. \\i \"lib/utils.qpl\", got '{rest}'"
        ))),
    }
}

/// Run a `.qpl` script (`\i <path>`) as a *namespaced import*: every table,
/// global, and function the script newly binds at its top level — anything
/// not already present before the run and not already namespaced itself —
/// is moved under `.<ns>.<name>`, where `<ns>` is derived from the file's
/// stem (`utils.qpl` -> `.utils`). Bare `\l` keeps loading flat into the
/// shared session scope; this is the opt-in alternative.
pub fn run_script_imported(path: &str, vm: &mut Vm) -> Result<(), QplError> {
    let ns = namespace_from_path(path);
    let before_tables: std::collections::HashSet<String> = vm.tables.keys().cloned().collect();
    let before_lazy: std::collections::HashSet<String> = vm.lazy_frames.keys().cloned().collect();
    let before_globals: std::collections::HashSet<String> = vm.globals.keys().cloned().collect();
    let before_functions: std::collections::HashSet<String> = vm.functions.keys().cloned().collect();

    run_script(path, vm)?;

    namespace_new_keys(&mut vm.tables, &before_tables, &ns);
    namespace_new_keys(&mut vm.lazy_frames, &before_lazy, &ns);
    namespace_new_keys(&mut vm.globals, &before_globals, &ns);
    namespace_new_keys(&mut vm.functions, &before_functions, &ns);
    Ok(())
}

/// Derive a namespace (`.utils`, `.my_lib`) from a `\i`-imported script's
/// file stem: non-identifier characters become `_`, and a leading digit gets
/// an `_` prefix so the result always lexes as a valid namespaced name.
fn namespace_from_path(path: &str) -> String {
    let stem = std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("ns");
    let mut cleaned: String = stem.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if cleaned.starts_with(|c: char| c.is_ascii_digit()) {
        cleaned.insert(0, '_');
    }
    format!(".{cleaned}")
}

/// Move every key in `map` that is new since `before` and not already
/// namespaced (doesn't start with `.`) under `<ns>.<key>`.
fn namespace_new_keys<V>(
    map: &mut HashMap<String, V>,
    before: &std::collections::HashSet<String>,
    ns: &str,
) {
    let new_keys: Vec<String> = map.keys()
        .filter(|k| !before.contains(*k) && !k.starts_with('.'))
        .cloned()
        .collect();
    for k in new_keys {
        let v = map.remove(&k).expect("key just listed from this map");
        map.insert(format!("{ns}.{k}"), v);
    }
}

/// Fold the physical lines of a script into logical statements.
///
/// A statement starts at a line with no leading indentation. Any following line
/// indented by a tab or four (or more) spaces is a continuation of that same
/// statement; the run of lines is joined with `\n` (which the lexer treats as
/// whitespace, and which correctly terminates any inline `/` comment). A blank
/// or non-indented line ends the current statement. Blank lines and lines whose
/// first non-space character is `/` are dropped unless they are continuations.
///
/// Returns `(zero-based line index where the statement began, statement text)`.
fn logical_statements(src: &str) -> Vec<(usize, String)> {
    let mut out: Vec<(usize, String)> = Vec::new();
    let mut buf: Vec<&str> = Vec::new();
    let mut start = 0usize;

    for (idx, raw) in src.lines().enumerate() {
        if !buf.is_empty() && (raw.starts_with('\t') || raw.starts_with("    ")) {
            buf.push(raw);
            continue;
        }
        if !buf.is_empty() {
            out.push((start, buf.join("\n")));
            buf.clear();
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('/') {
            continue;
        }
        buf.push(raw);
        start = idx;
    }
    if !buf.is_empty() {
        out.push((start, buf.join("\n")));
    }
    out
}

/// Does `src` look like an unfinished statement that should keep reading?
/// True while `(`/`[` are unbalanced, on a trailing `,`, on a lex error (e.g. an
/// unterminated string), or when the parse fails specifically because input ran
/// out (so a genuine syntax error still surfaces immediately). `\` commands are
/// always single-line.
fn wants_more(src: &str) -> bool {
    let trimmed = src.trim_start();
    if trimmed.starts_with('\\') || trimmed.starts_with(".qpl.cfg") {
        return false;
    }
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
        Err(QplError::Parse(msg)) => msg.contains("got Eof"),
        Err(_) => false,
    }
}

pub fn start(vm: &mut Vm) {
    let mut rl = DefaultEditor::new().expect("failed to create line editor");

    println!(
        "qpl v{} (Quick Polars Query Language) REPL - \\d disassemble, \\l <path> run a script, \\i \"<path>\" import as a namespace, \\1 <path> log stdout",
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
                    let result = vm.with_request_permission(mode, |vm| eval_for_dispatch(&command, vm));
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
                if buf.is_empty() { break; }
                buf.clear();
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => { eprintln!("readline error: {e}"); break; }
        }
    }
}

/// Everything a submitted REPL line can be *except* `\port`, which needs
/// REPL-loop state (`PortSession`) this function doesn't have — see
/// [`run_line`]. Shared by both the normal (rustyline) input path and, once a
/// port has been opened, the polling loop's stdin lines.
fn process_submitted(src: &str, vm: &mut Vm) {
    if let Err(e) = run_line(src, vm, "<main>", 0) {
        eprintln!("{}", fmt_repl_error(&e));
    }
}

/// `\port <n>` opens a listener (closing any previously open one first);
/// bare `\port` closes it. Only reachable from `start()` — `\port` doesn't
/// exist for script mode (`run_script` never calls this), per its being
/// meaningless outside a long-lived interactive session.
#[cfg(feature = "ipc")]
fn handle_port_directive(rest: &str, session: &mut PortSession) -> Result<(), QplError> {
    session.close_port();
    if rest.is_empty() {
        return Ok(());
    }
    let port: u16 = rest.parse()
        .map_err(|_| QplError::Runtime(format!("\\port: expected a port number, got '{rest}'")))?;
    session.open_port(port)
}

/// Evaluate one command received over `\port`, treating it exactly like a
/// REPL line — `.qpl.cfg` directives, `log` writes, and ordinary
/// statements (selects, updates, deletes, assignments, function defs) all
/// work. `\`-prefixed system commands (`\d`, `\l`, `\1`, `\port` itself)
/// are deliberately not reachable this way — they're local REPL/session
/// administration, not part of the query language a remote client dispatches.
#[cfg(feature = "ipc")]
fn eval_for_dispatch(line: &str, vm: &mut Vm) -> Result<EvalResult, QplError> {
    if let Some(args) = cfg_directive(line) {
        apply_cfg(args, vm)?;
        return Ok(EvalResult::Stored);
    }
    if let Some(arg) = log_target(line) {
        let mut text = String::new();
        for expr in parse_expr_seq(tokenise(arg)?)? {
            let val = match resolve::eval_value(vm, &expr)? {
                resolve::EvalValue::Scalar(v) => v,
                resolve::EvalValue::Frame { .. } => {
                    return Err(QplError::Runtime(
                        "log expects a scalar expression, got a table".into(),
                    ))
                }
            };
            text.push_str(&fmt_log_val(&val));
        }
        vm.emit(&text);
        return Ok(EvalResult::Stored);
    }
    run_vm(line, vm)
}

/// REPL-loop-side state for `\port`: a stdin-reader thread (spawned once, the
/// first time `\port` is used) feeding lines to the polling loop in `start()`,
/// plus whichever listener is currently open, if any.
#[cfg(feature = "ipc")]
struct PortSession {
    stdin_rx: std::sync::mpsc::Receiver<String>,
    port: Option<(crate::ipc::ServerHandle, std::sync::mpsc::Receiver<crate::ipc::PortRequest>)>,
}

#[cfg(feature = "ipc")]
enum PortEvent {
    Line(String),
    Request(crate::ipc::HandleMode, String, std::sync::mpsc::Sender<Vec<u8>>),
    StdinClosed,
}

#[cfg(feature = "ipc")]
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
        Self { stdin_rx: rx, port: None }
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
    ].expect("trades");
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
    ].expect("quotes");
    let quotes = quotes
        .lazy()
        .with_column(col("ts").cast(DataType::Datetime(TimeUnit::Nanoseconds, None)))
        .collect()
        .expect("cast quotes.ts");

    vm.tables.insert("trades".into(), trades);
    vm.tables.insert("quotes".into(), quotes);
}

fn match_run_vm(line: &str, vm: &mut Vm, path: &str, lineno: usize) -> Result<(), QplError> {
    match eval_line(line, vm) {
        Ok(())                       => Ok(()),
        Err(e) if path == "<main>"   => Err(e),
        Err(e) => Err(QplError::Runtime(format!("{}:{}: {e}", path, lineno + 1))),
    }
}

/// Evaluate one statement and print its result. All output goes through
/// [`Vm::emit`] so it is mirrored to the stdout log when one is configured.
fn eval_line(line: &str, vm: &mut Vm) -> Result<(), QplError> {
    if let Some(args) = cfg_directive(line) {
        return apply_cfg(args, vm);
    }
    if let Some(arg) = log_target(line) {
        // a `log` argument is a list of expressions; render and concatenate each.
        // Goes through `resolve::eval_value` (not the plain scalar folder) so a
        // reduction (`log max t`price`) or a cast on a column expression works
        // the same as it does in any other value position.
        let mut text = String::new();
        for expr in parse_expr_seq(tokenise(arg)?)? {
            let val = match resolve::eval_value(vm, &expr)? {
                resolve::EvalValue::Scalar(v) => v,
                resolve::EvalValue::Frame { .. } => {
                    return Err(QplError::Runtime(
                        "log expects a scalar expression, got a table".into(),
                    ))
                }
            };
            text.push_str(&fmt_log_val(&val));
        }
        vm.emit(&text);
        return Ok(());
    }
    match run_vm(line, vm)? {
        EvalResult::Table(df)   => vm.emit(&df.to_string()),
        EvalResult::Stored      => {}
        EvalResult::Scalar(val) => vm.emit(&fmt_val(&val)),
        EvalResult::Lazy(plan)  => vm.emit(&plan),
    }
    Ok(())
}

/// Recognise the stdout write: `log <expr>` (and bare `log`, which prints a
/// blank line). Returns the argument text to evaluate as a scalar.
fn log_target(line: &str) -> Option<&str> {
    if line == "log" {
        return Some("");
    }
    if let Some(rest) = line.strip_prefix("log ") {
        return Some(rest.trim());
    }
    None
}

/// Recognise the `.qpl.cfg` config function: `.qpl.cfg key=value key=value ...`.
/// Returns the argument text (possibly empty, for a bare `.qpl.cfg` which just
/// prints the current settings). Not a config line → `None`.
fn cfg_directive(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix(".qpl.cfg")?;
    match rest.chars().next() {
        None => Some(""),
        Some(c) if c.is_whitespace() => Some(rest.trim()),
        Some(_) => None, // e.g. `.qpl.cfgx` is not this directive
    }
}

/// Apply `.qpl.cfg` arguments: whitespace-separated `key=value` pairs. A bare
/// `.qpl.cfg` prints the current configuration.
fn apply_cfg(args: &str, vm: &mut Vm) -> Result<(), QplError> {
    if args.is_empty() {
        let current = vm.config.describe();
        vm.emit(&current);
        return Ok(());
    }
    for pair in args.split_whitespace() {
        let (key, value) = pair.split_once('=').ok_or_else(|| {
            QplError::Runtime(format!("expected key=value in `.qpl.cfg`, got '{pair}'"))
        })?;
        vm.config.set(key.trim(), value.trim())?;
    }
    Ok(())
}

/// Handle a `\` system command. Returns `Some(result)` if `line` is one.
/// Currently only `\1 <path>` (set the stdout log; bare `\1` detaches it).
fn system_command(line: &str, vm: &mut Vm) -> Option<Result<(), QplError>> {
    let path = line.strip_prefix("\\1")?.trim();
    Some(vm.set_stdout_log(path))
}

/// kdb-style tag for a vector kind, used as the `<tag>[<n>]:` prefix.
fn vec_tag(kind: ast::VecKind) -> &'static str {
    use ast::VecKind::*;
    match kind {
        Int => "i64", Float => "f64", Sym => "sym", Str => "str", Bool => "bool",
        Date => "date", Month => "month", Time => "time", Minute => "minute",
        Second => "second", Timestamp => "timestamp", Timespan => "timespan",
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
                ca_str().iter().flatten().map(|x| format!("\"{x}\" ")).collect::<String>().trim_end().to_string()
            } else {
                ca_str().iter().flatten().collect::<Vec<_>>().join(" ")
            }
        }
        Bool => s.bool().expect("BoolVec backed by a bool Series")
            .iter().flatten().map(|b| if b { "1" } else { "0" }).collect(),
        Int => s.i64().expect("IntVec backed by an i64 Series")
            .iter().flatten().map(|n| n.to_string()).collect::<Vec<_>>().join(" "),
        Float => s.f64().expect("FloatVec backed by an f64 Series")
            .iter().flatten().map(|f| f.to_string()).collect::<Vec<_>>().join(" "),
        Date | Month | Minute | Second => s.i32().expect("temporal vector backed by an i32 Series")
            .iter().flatten()
            .map(|n| temporal::format_temporal(&temporal_scalar_of(kind, n as i64)).unwrap())
            .collect::<Vec<_>>().join(" "),
        Time | Timestamp | Timespan => s.i64().expect("temporal vector backed by an i64 Series")
            .iter().flatten()
            .map(|n| temporal::format_temporal(&temporal_scalar_of(kind, n)).unwrap())
            .collect::<Vec<_>>().join(" "),
    }
}

/// Render a value for `log`: raw text, no type prefix or quoting.
fn fmt_log_val(v: &ast::Value) -> String {
    if let Some((kind, s)) = v.as_vec() {
        return fmt_vec_elems(kind, s, false);
    }
    match v {
        ast::Value::Str(s)   => s.clone(),
        ast::Value::Sym(s)   => s.clone(),
        ast::Value::Int(n)   => n.to_string(),
        ast::Value::Float(f) => f.to_string(),
        ast::Value::Bool(b)  => b.to_string(),
        _ => temporal::format_temporal(v).unwrap_or_else(|| format!("{v:?}")),
    }
}

fn fmt_val(v: &ast::Value) -> String {
    if let Some(text) = temporal::format_temporal(v) {
        let tag = match v {
            ast::Value::Date(_)      => "date",
            ast::Value::Month(_)     => "month",
            ast::Value::Time(_)      => "time",
            ast::Value::Minute(_)    => "minute",
            ast::Value::Second(_)    => "second",
            ast::Value::Timestamp(_) => "timestamp",
            _                        => "timespan",
        };
        return format!("{tag}: {text}");
    }
    if let Some((kind, s)) = v.as_vec() {
        return format!("{}[{}]: {}", vec_tag(kind), s.len(), fmt_vec_elems(kind, s, true));
    }
    match v {
        ast::Value::Int(n)   => format!("i64: {n}"),
        ast::Value::Float(f) => format!("f64: {f}"),
        ast::Value::Str(s)   => format!("str: \"{s}\""),
        ast::Value::Sym(s)   => format!("sym: `{s}"),
        ast::Value::Bool(b)  => format!("bool: {b}"),
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
    }
}

fn disassemble(source: &str) -> Result<String, QplError> {
    let tokens = tokenise(source)?;
    let stmt   = parse(tokens)?;
    let prog   = compile(&stmt)?;
    Ok(disassemble_instructions(&prog).join("\n"))
}

#[cfg(test)]
mod tests {
    use super::{cfg_directive, eval_line, logical_statements, log_target, wants_more};
    use super::{namespace_from_path, run_line, run_script_imported};
    use crate::vm::Vm;

    /// Run `line` through [`eval_line`] and return whatever it wrote via
    /// `Vm::emit`, by pointing the stdout-log tee at a scratch file.
    fn logged(vm: &mut Vm, line: &str) -> String {
        let path = std::env::temp_dir().join(format!("qpl_repl_test_{:?}", std::thread::current().id()));
        vm.stdout_log = Some(std::fs::File::create(&path).unwrap());
        eval_line(line, vm).expect("eval_line");
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
        let path = std::env::temp_dir().join(format!("qpl_i_test_{:?}.qpl", std::thread::current().id()));
        std::fs::write(&path, "greeting: \"hi\"\ndouble: {[x] x*2}\n").unwrap();
        let mut vm = Vm::new();
        run_script_imported(path.to_str().unwrap(), &mut vm).expect("run_script_imported");
        let _ = std::fs::remove_file(&path);

        let ns = namespace_from_path(path.to_str().unwrap());
        let ns_greeting = format!("{ns}.greeting");
        let ns_double = format!("{ns}.double");
        assert!(vm.globals.contains_key(&ns_greeting), "{:?}", vm.globals.keys().collect::<Vec<_>>());
        assert!(vm.functions.contains_key(&ns_double));
        assert!(!vm.globals.contains_key("greeting"));
        assert!(!vm.functions.contains_key("double"));
    }

    #[test]
    fn i_import_does_not_double_namespace_an_already_namespaced_binding() {
        // a script that itself `\i`s another script leaves that nested import's
        // already-namespaced bindings alone rather than re-prefixing them.
        let mut vm = Vm::new();
        vm.globals.insert(".inner.x".into(), crate::ast::Value::Int(1));
        let path = std::env::temp_dir().join(format!("qpl_i_nested_test_{:?}.qpl", std::thread::current().id()));
        std::fs::write(&path, "y: 2\n").unwrap();
        run_script_imported(path.to_str().unwrap(), &mut vm).expect("run_script_imported");
        let _ = std::fs::remove_file(&path);

        assert!(vm.globals.contains_key(".inner.x"));
        assert!(!vm.globals.contains_key("y"));
    }

    #[test]
    fn i_command_requires_a_quoted_path() {
        let path = std::env::temp_dir().join(format!("qpl_i_quoted_test_{:?}.qpl", std::thread::current().id()));
        std::fs::write(&path, "z: 1\n").unwrap();
        let mut vm = Vm::new();

        // bare/unquoted is rejected with a clear message
        let err = run_line(&format!("\\i {}", path.to_str().unwrap()), &mut vm, "<main>", 0)
            .expect_err("bare path should be rejected");
        assert!(matches!(&err, crate::errors::QplError::Runtime(m) if m.contains("quoted path")), "{err:?}");

        // quoted works, both at the REPL and (via run_script -> run_line) nested in a script
        run_line(&format!("\\i \"{}\"", path.to_str().unwrap()), &mut vm, "<main>", 0)
            .expect("quoted path should import");
        let _ = std::fs::remove_file(&path);
        let ns = namespace_from_path(path.to_str().unwrap());
        assert!(vm.globals.contains_key(&format!("{ns}.z")));
    }

    #[test]
    fn log_evaluates_a_reduction_directly() {
        // regression: `log max t`price` (with or without the parens the README
        // recommends for a call/reduction) used to fail — `eval_scalar` cannot
        // resolve a table/column expression, only a plain scalar fold.
        let mut vm = Vm::new();
        vm.tables.insert(
            "t".into(),
            polars::df!["price" => [1.0f64, 2.0, 3.0]].unwrap(),
        );
        assert_eq!(logged(&mut vm, "log (max t`price)"), "3");
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
        let path = std::env::temp_dir().join(format!("qpl_dispatch_log_test_{:?}", std::thread::current().id()));
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
        vm.tables.insert("t".into(), polars::df!["c" => [1i64, 2, 3]].unwrap());
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
        // a real syntax error is NOT "more" — surface it now
        assert!(!wants_more("selct from trades"));
        // a terminal lex error must not hang the prompt waiting for input
        assert!(!wants_more("select from t where a = 1 @"));
        // `\` commands are always single-line
        assert!(!wants_more("\\d select a: ?[c>1;`x"));
        assert!(!wants_more("\\l some/script.qpl"));
        // `.qpl.cfg` is a single-line directive, never "more"
        assert!(!wants_more(".qpl.cfg maxrow=5 maxcol=3"));
    }

    #[test]
    fn cfg_directive_recognises_the_config_function() {
        assert_eq!(cfg_directive(".qpl.cfg maxcol=8 maxrow=20"), Some("maxcol=8 maxrow=20"));
        assert_eq!(cfg_directive("  .qpl.cfg  round_type=HALF_UP "), Some("round_type=HALF_UP"));
        assert_eq!(cfg_directive(".qpl.cfg"), Some(""));
        assert_eq!(cfg_directive(".qpl.cfgx maxcol=1"), None);
        assert_eq!(cfg_directive("select from t"), None);
    }

    #[test]
    fn log_target_keyword() {
        assert_eq!(log_target(r#"log "hi""#), Some(r#""hi""#));
        assert_eq!(log_target("log x + 1"), Some("x + 1"));
    }

    #[test]
    fn log_target_bare_is_blank_line() {
        assert_eq!(log_target("log"), Some(""));
    }

    /// `log` is the only spelling that writes to stdout. A leading digit is
    /// always an ordinary expression, so arithmetic and row-count queries
    /// alike reach the VM untouched.
    #[test]
    fn log_target_ignores_leading_digits() {
        assert_eq!(log_target("1"), None);
        assert_eq!(log_target(r#"1 "hi""#), None);
        assert_eq!(log_target("1 + 1"), None);
        assert_eq!(log_target("1 limit select from trades"), None);
        assert_eq!(log_target("1 # select from trades"), None);
        assert_eq!(log_target("10 limit select from trades"), None);
    }

    #[test]
    fn log_target_ignores_ordinary_statements() {
        assert_eq!(log_target("select from trades"), None);
        assert_eq!(log_target("t: select from trades"), None);
    }

    #[test]
    fn single_line_statements_pass_through() {
        let got = logical_statements("select from trades\nselect from quotes\n");
        assert_eq!(got, vec![
            (0, "select from trades".to_string()),
            (1, "select from quotes".to_string()),
        ]);
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
        assert_eq!(got, vec![
            (0, "t: select a, b\n    by sym\n    where a > 1".to_string()),
            (3, "select from t".to_string()),
        ]);
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
        assert_eq!(got, vec![
            (0, "select a\n    from trades".to_string()),
            (3, "select from quotes".to_string()),
        ]);
    }

    #[test]
    fn shallow_indent_is_a_new_statement() {
        // one/two/three spaces is not a continuation
        let got = logical_statements("select from trades\n  select from quotes");
        assert_eq!(got, vec![
            (0, "select from trades".to_string()),
            (1, "  select from quotes".to_string()),
        ]);
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
        assert_eq!(got, vec![
            (0, "select a\n    / pick columns\n    from trades".to_string()),
        ]);
    }
}

