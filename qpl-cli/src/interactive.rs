//! The terminal REPL loop: line editing (`rustyline`) and, while a `\port` is
//! open, polling stdin and the listener together instead of a blocking
//! `readline()`.

use qpl::repl::{self, wants_more};
use qpl::vm::Vm;
use rustyline::{DefaultEditor, error::ReadlineError};

pub fn start(vm: &mut Vm) {
    let mut rl = DefaultEditor::new().expect("failed to create line editor");

    println!(
        "qpl v{} (Quick Polars Language) REPL - \\d disassemble, \\l <path> run a script, \\i \"<path>\" import as a namespace, \\1 <path> log stdout, \\port <n> open a listener",
        env!("CARGO_PKG_VERSION")
    );

    let mut buf: Vec<String> = Vec::new();
    // a script run with `-i` (or that left a port open) may have opened a
    // port already
    #[cfg(feature = "ipc")]
    let mut port_session: Option<PortSession> = if vm.port.is_some() {
        Some(PortSession::new())
    } else {
        None
    };

    loop {
        // While a port is open, poll stdin and the listener instead of a
        // blocking `readline()`, so requests interleave with typed input. This
        // gives up rustyline's line editing for the rest of the session: stdin
        // can't be handed back once another thread reads it.
        #[cfg(feature = "ipc")]
        if let Some(session) = port_session.as_mut() {
            // no `readline()` here, so print the prompt by hand, once per
            // read cycle
            if session.needs_prompt {
                print_port_prompt(vm);
                session.needs_prompt = false;
            }
            match session.poll(vm) {
                PortEvent::Line(line) => {
                    process_submitted(&line, vm);
                    session.needs_prompt = true;
                }
                PortEvent::Request(mode, command, reply_tx) => {
                    let _running = vm.interrupt.statement();
                    let result = vm
                        .with_request_permission(mode, |vm| repl::eval_for_dispatch(&command, vm));
                    let _ = reply_tx.send(qpl::ipc::encode_result(&result));
                    session.needs_prompt = true;
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
                // a blank line with a partial statement force-submits
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
                process_submitted(&src, vm);
                // switch to polling if that statement opened a port
                #[cfg(feature = "ipc")]
                if vm.port.is_some() && port_session.is_none() {
                    port_session = Some(PortSession::new());
                }
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

/// Run one submitted REPL line, printing any error.
fn process_submitted(src: &str, vm: &mut Vm) {
    if let Err(e) = repl::run_command(src, vm) {
        eprintln!("{}", repl::fmt_repl_error(&e));
    }
}

/// Run-loop state while serving: the stdin-reader thread (spawned the first
/// time a port opens) and whether a prompt is owed. The listener itself lives
/// on `Vm::port`.
#[cfg(feature = "ipc")]
struct PortSession {
    stdin_rx: std::sync::mpsc::Receiver<String>,
    needs_prompt: bool,
}

#[cfg(feature = "ipc")]
enum PortEvent {
    Line(String),
    Request(
        qpl::ipc::HandleMode,
        String,
        std::sync::mpsc::Sender<Vec<u8>>,
    ),
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
        Self {
            stdin_rx: rx,
            needs_prompt: true,
        }
    }

    /// Wait for a stdin line or a request. `vm.port` is re-read each time since
    /// `\port` can close or reopen it at any point.
    fn poll(&mut self, vm: &mut Vm) -> PortEvent {
        loop {
            match self.stdin_rx.try_recv() {
                Ok(line) => return PortEvent::Line(line),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return PortEvent::StdinClosed,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            if let Some(state) = &vm.port
                && let Ok((mode, command, reply_tx)) = state.rx.try_recv()
            {
                return PortEvent::Request(mode, command, reply_tx);
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    }
}

/// The polling loop's prompt: `qpl [127.0.0.1:<port>]) ` while a port is open,
/// `qpl) ` after it closes (polling continues either way). Flushed by hand
/// since no line editor draws it.
#[cfg(feature = "ipc")]
fn print_port_prompt(vm: &Vm) {
    use std::io::Write;
    match &vm.port {
        Some(state) => print!("qpl [127.0.0.1:{}] ) ", state.port),
        None => print!("qpl) "),
    }
    let _ = std::io::stdout().flush();
}
