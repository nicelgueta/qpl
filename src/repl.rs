use crate::ast::self;
use crate::compiler::compile;
use crate::errors::QplError;
use crate::lexer::tokenise;
use crate::parser::{parse, parse_expr_seq};
use crate::opcodes::disassemble_instructions;
use crate::vm::{Vm, run_vm, EvalResult};
use polars::prelude::*;
use rustyline::{DefaultEditor, error::ReadlineError};

/// Run a `.qpl` script file, printing results. Returns Err on the first failure.
pub fn run_script(path: &str, vm: &mut Vm) -> Result<(), QplError> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| QplError::Runtime(format!("cannot read '{path}': {e}")))?;
    for (lineno, stmt) in logical_statements(&src) {
        if let Some(result) = system_command(&stmt, vm) {
            result?;
            continue;
        }
        match_run_vm(&stmt, vm, path, lineno)?;
    }
    Ok(())
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

pub fn start(vm: &mut Vm) {
    let mut rl = DefaultEditor::new().expect("failed to create line editor");

    println!("qpl (Quick Polars Query Language) REPL - \\d to disassemble, \\1 <path> to log stdout");

    loop {
        match rl.readline("qpl) ") {
            Ok(line) => {
                let line = line.trim().to_string();
                if line.is_empty() || line.starts_with('/') {
                    continue;
                }
                let _ = rl.add_history_entry(&line);
                if let Some(src) = line.strip_prefix("\\d").map(str::trim) {
                    match disassemble(src) {
                        Ok(listing) => println!("{listing}"),
                        Err(e) => eprintln!("{}", fmt_repl_error(&e)),
                    }
                    continue;
                }
                if let Some(result) = system_command(&line, vm) {
                    if let Err(e) = result {
                        eprintln!("{}", fmt_repl_error(&e));
                    }
                    continue;
                }
                if let Err(e) = match_run_vm(&line, vm, "<main>", 0) {
                    eprintln!("{}", fmt_repl_error(&e))
                };
            }
            Err(ReadlineError::Interrupted) | Err(ReadlineError::Eof) => break,
            Err(e) => { eprintln!("readline error: {e}"); break; }
        }
    }
}

pub fn load_demo_tables(vm: &mut Vm) {
    let trades = df![
        "sym"   => ["AAPL","AAPL","MSFT","MSFT","GOOG","GOOG","AAPL","MSFT"],
        "price" => [182.3f64, 183.1, 415.2, 416.0, 140.5, 141.2, 184.0, 414.8],
        "size"  => [100i64, 250, 80, 300, 150, 90, 500, 200],
        "side"  => ["buy","sell","buy","buy","sell","buy","sell","sell"],
    ].expect("trades");

    let quotes = df![
        "sym"   => ["AAPL","MSFT","GOOG","AAPL","MSFT"],
        "bid"   => [182.0f64, 415.0, 140.3, 183.8, 414.5],
        "ask"   => [182.5f64, 415.5, 140.8, 184.2, 415.0],
        "bsize" => [500i64, 300, 200, 400, 600],
        "asize" => [400i64, 250, 150, 350, 500],
    ].expect("quotes");

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
    if let Some(arg) = log_target(line) {
        // a `log` argument is a list of expressions; render and concatenate each
        let mut text = String::new();
        for expr in parse_expr_seq(tokenise(arg)?)? {
            text.push_str(&fmt_log_val(&vm.eval_scalar(&expr)?));
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

/// Recognise the kdb-style stdout write: `log <expr>` or `1 <expr>` (and bare
/// `log` / `1`, which print a blank line). Returns the argument text to
/// evaluate as a scalar. `1 limit ...` / `1 # ...` stay ordinary row-count
/// queries, not writes.
fn log_target(line: &str) -> Option<&str> {
    if line == "log" || line == "1" {
        return Some("");
    }
    if let Some(rest) = line.strip_prefix("log ") {
        return Some(rest.trim());
    }
    if let Some(rest) = line.strip_prefix("1 ") {
        let rest = rest.trim();
        if rest.starts_with("limit") || rest.starts_with('#') {
            return None;
        }
        return Some(rest);
    }
    None
}

/// Handle a `\` system command. Returns `Some(result)` if `line` is one.
/// Currently only `\1 <path>` (set the stdout log; bare `\1` detaches it).
fn system_command(line: &str, vm: &mut Vm) -> Option<Result<(), QplError>> {
    let path = line.strip_prefix("\\1")?.trim();
    Some(vm.set_stdout_log(path))
}

/// Render a value for `log` / `1`: raw text, no type prefix or quoting.
fn fmt_log_val(v: &ast::Value) -> String {
    match v {
        ast::Value::Str(s)   => s.clone(),
        ast::Value::Int(n)   => n.to_string(),
        ast::Value::Float(f) => f.to_string(),
        ast::Value::Bool(b)  => b.to_string(),
        other                => format!("{other:?}"),
    }
}

fn fmt_val(v: &ast::Value) -> String {
    match v {
        ast::Value::Int(n)   => format!("i64: {n}"),
        ast::Value::Float(f) => format!("f64: {f}"),
        ast::Value::Str(s)   => format!("str: \"{s}\""),
        ast::Value::Bool(b)  => format!("bool: {b}"),
        other                => format!("{other:?}"),
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
    use super::{logical_statements, log_target};

    #[test]
    fn log_target_keyword_and_digit() {
        assert_eq!(log_target(r#"log "hi""#), Some(r#""hi""#));
        assert_eq!(log_target(r#"1 "hi""#), Some(r#""hi""#));
        assert_eq!(log_target("log x + 1"), Some("x + 1"));
    }

    #[test]
    fn log_target_bare_is_blank_line() {
        assert_eq!(log_target("log"), Some(""));
        assert_eq!(log_target("1"), Some(""));
    }

    #[test]
    fn log_target_leaves_row_count_queries_alone() {
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

