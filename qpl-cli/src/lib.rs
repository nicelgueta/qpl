//! The `qpl` command line. A library crate rather than a plain `main.rs` so
//! that a binary built with Rust extensions (see [`qpl::ext`]) gets exactly
//! the same front-end:
//!
//! ```ignore
//! fn main() {
//!     qpl_cli::run(vec![qpl::ext::Extension::new("geo").with::<haversine>()]);
//! }
//! ```

mod interactive;

use clap::{ArgAction::SetTrue, Parser};

use qpl::ext::Extension;
use qpl::{repl, vm};

/// The allocator the `qpl` binary uses (see `mimalloc` in Cargo.toml); an
/// extension binary can install it with `#[global_allocator]` too.
pub use mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "qpl", about = "Quick Polars Language", version)]
struct Cli {
    /// Script to execute — a `.qpl` source file, or a `.qplc` file compiled
    /// by `-C` (detected by its magic bytes, not the extension)
    file: Option<String>,

    /// Run script then drop into the REPL
    #[arg(short = 'i', requires = "file")]
    interactive: bool,

    #[arg(long = "load-demo", action = SetTrue)]
    load_demo: bool,

    /// Allow write actions (`sink`, `\1 <path>`, `w!hopen`) — without it the
    /// session is read-only
    #[arg(short = 'w', long = "write", action = SetTrue, conflicts_with_all = ["compile", "disassemble"])]
    write: bool,

    /// Compile a script to bytecode and exit (writes <script-stem>.qplc next
    /// to it, or -o's path) — doesn't run it
    #[arg(short = 'C', long = "compile", value_name = "SCRIPT", conflicts_with_all = ["file", "interactive", "command", "disassemble"])]
    compile: Option<String>,

    /// Output path for a compiled script (requires -C)
    #[arg(
        short = 'o',
        long = "output",
        value_name = "PATH",
        requires = "compile"
    )]
    output: Option<String>,

    /// Run a qpl command and exit, like `python -c` / `sh -c`
    #[arg(short = 'c', long = "command", value_name = "CMD", conflicts_with_all = ["file", "interactive", "compile", "disassemble"])]
    command: Option<String>,

    /// Print a `.qplc` file's bytecode (or a `.qpl` script's, compiled on the
    /// fly) as text and exit — doesn't run it
    #[arg(short = 'd', long = "disassemble", value_name = "FILE", conflicts_with_all = ["file", "interactive"])]
    disassemble: Option<String>,
}

/// Run the `qpl` command line with `extensions` registered in every session,
/// then exit.
pub fn run(extensions: Vec<Extension>) {
    let cli = Cli::parse();

    if let Some(script) = cli.compile.as_deref() {
        if let Err(e) = compile_to_qplc(script, cli.output.as_deref()) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }

    if let Some(path) = cli.disassemble.as_deref() {
        match repl::disassemble_file(path) {
            Ok(lines) => {
                use std::io::Write;
                let mut out = std::io::stdout().lock();
                // a closed pipe (`| head`) just ends the dump
                let _ = lines.iter().try_for_each(|l| writeln!(out, "{l}"));
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let mut vm = if cli.write {
        vm::Vm::new_writable()
    } else {
        vm::Vm::new()
    };
    // registered before any user extension, so a user extension can't claim
    // the `std` namespace root.
    for ext in qpl_std::extensions() {
        if let Err(e) = vm.register(ext) {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
    for ext in extensions {
        if let Err(e) = vm.register(ext) {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
    install_ctrl_c(&vm);

    if cli.load_demo {
        println!("Loading demo tables `trades` and `quotes`");
        repl::load_demo_tables(&mut vm)
    };

    if let Some(command) = cli.command.as_deref() {
        match repl::run_command(command, &mut vm) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("{e}");
                let interrupted = matches!(e, qpl::errors::QplError::Interrupted);
                std::process::exit(if interrupted { 130 } else { 1 });
            }
        }
    }

    match cli.file {
        None => interactive::start(&mut vm),
        Some(ref path) => {
            if let Err(e) = repl::run_script(path, &mut vm) {
                eprintln!("{e}");
                let interrupted = matches!(e, qpl::errors::QplError::Interrupted);
                // `-i`, or a script that left a `\port` open, carries on
                // into the REPL/serve loop even after an error
                if !(cli.interactive || repl::port_open(&vm)) {
                    std::process::exit(if interrupted { 130 } else { 1 });
                }
            }
            // a script that opened `\port` keeps serving, like `-i`
            if cli.interactive || repl::port_open(&vm) {
                interactive::start(&mut vm);
            }
        }
    }
}

/// `qpl -C script.qpl [-o out.qplc]`: compile without running. Writes a temp
/// file and renames it into place, so a failed compile never leaves a
/// partial artifact.
fn compile_to_qplc(script: &str, output: Option<&str>) -> Result<(), String> {
    let program = repl::compile_script(script).map_err(|e| e.to_string())?;
    let bytes = program.to_bytes().map_err(|e| e.to_string())?;

    let out_path = match output {
        Some(o) => std::path::PathBuf::from(o),
        None => std::path::Path::new(script).with_extension("qplc"),
    };
    let mut tmp_name = out_path.clone().into_os_string();
    tmp_name.push(".tmp");
    let tmp_path = std::path::PathBuf::from(tmp_name);

    let write_result = std::fs::write(&tmp_path, &bytes)
        .map_err(|e| format!("cannot write '{}': {e}", tmp_path.display()))
        .and_then(|()| {
            std::fs::rename(&tmp_path, &out_path)
                .map_err(|e| format!("cannot write '{}': {e}", out_path.display()))
        });
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    write_result
}

/// First Ctrl-C asks the running statement to stop at its next check point;
/// a second (or one with nothing running) exits. Doesn't fire inside
/// `readline`, where the terminal is in raw mode.
fn install_ctrl_c(vm: &vm::Vm) {
    let interrupt = vm.interrupt.clone();
    let _ = ctrlc::set_handler(move || match interrupt.on_ctrl_c() {
        qpl::interrupt::CtrlC::Exit => std::process::exit(130),
        qpl::interrupt::CtrlC::Interrupting => {
            eprintln!("^C interrupting... (Ctrl-C again to force quit)");
        }
    });
}

#[cfg(test)]
mod tests {
    //! CLI tests that spawn the built binary, since clap's argument handling
    //! and exit codes are only observable from outside. `CARGO_BIN_EXE_qpl`
    //! isn't set for a bin's own unit tests, so `qpl_bin_path` derives
    //! `target/<profile>/qpl` from `current_exe()`, building it if missing.
    use std::io::Write;
    use std::process::Command;

    fn qpl_bin_path() -> std::path::PathBuf {
        let mut path = std::env::current_exe().expect("current_exe");
        path.pop(); // drop the test binary's own file name
        if path.ends_with("deps") {
            path.pop();
        }
        path.push(if cfg!(windows) { "qpl.exe" } else { "qpl" });
        if !path.exists() {
            static BUILD: std::sync::Once = std::sync::Once::new();
            BUILD.call_once(|| {
                let status = Command::new(env!("CARGO"))
                    .args(["build", "--quiet", "--bin", "qpl"])
                    .status()
                    .expect("cargo build --bin qpl");
                assert!(status.success(), "cargo build --bin qpl failed");
            });
        }
        path
    }

    fn qpl() -> Command {
        Command::new(qpl_bin_path())
    }

    fn scratch_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "qpl_cli_test_{}_{:?}_{name}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn dash_c_runs_a_command_and_prints_its_result() {
        let out = qpl().args(["-c", "1+2"]).output().expect("run qpl -c");
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains('3'),
            "stdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    #[test]
    fn dash_c_with_load_demo_can_see_demo_tables() {
        let out = qpl()
            .args(["--load-demo", "-c", "select avg price by sym from trades"])
            .output()
            .expect("run qpl --load-demo -c");
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn dash_c_failing_command_exits_1_with_error_on_stderr() {
        let out = qpl()
            .args(["-c", "1 + `nope"])
            .output()
            .expect("run qpl -c");
        assert_eq!(out.status.code(), Some(1));
        assert!(!out.stderr.is_empty());
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn dash_c_rejects_file_interactive_and_compile() {
        let script = scratch_path("script.qpl");
        std::fs::write(&script, "1+1\n").unwrap();

        for args in [
            vec![
                "-c".to_string(),
                "1+1".to_string(),
                script.to_str().unwrap().to_string(),
            ],
            vec![
                "-i".to_string(),
                "-c".to_string(),
                "1+1".to_string(),
                script.to_str().unwrap().to_string(),
            ],
            vec![
                "-C".to_string(),
                script.to_str().unwrap().to_string(),
                "-c".to_string(),
                "1+1".to_string(),
            ],
        ] {
            let out = qpl().args(&args).output().expect("run qpl");
            assert!(!out.status.success(), "expected clap to reject {args:?}");
        }
        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn dash_capital_c_compiles_and_the_result_runs_like_the_source() {
        let script = scratch_path("cf.qpl");
        std::fs::write(&script, "x: 1\nwhile[x<3; x: x+1]\nx\n").unwrap();
        let out_qplc = scratch_path("cf.qplc");

        let compile = qpl()
            .args([
                "-C",
                script.to_str().unwrap(),
                "-o",
                out_qplc.to_str().unwrap(),
            ])
            .output()
            .expect("run qpl -C");
        assert!(
            compile.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&compile.stderr)
        );
        assert!(out_qplc.exists());

        let run_source = qpl().arg(&script).output().expect("run source");
        let run_compiled = qpl().arg(&out_qplc).output().expect("run compiled");
        assert_eq!(run_source.stdout, run_compiled.stdout);
        assert_eq!(run_source.status.code(), run_compiled.status.code());

        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&out_qplc);
    }

    #[test]
    fn dash_capital_c_default_output_path_is_the_script_stem_with_qplc() {
        let script = scratch_path("stem_test.qpl");
        std::fs::write(&script, "1+1\n").unwrap();
        let expected_out = script.with_extension("qplc");
        let _ = std::fs::remove_file(&expected_out);

        let compile = qpl().arg("-C").arg(&script).output().expect("run qpl -C");
        assert!(compile.status.success());
        assert!(expected_out.exists(), "expected {expected_out:?} to exist");

        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&expected_out);
    }

    #[test]
    fn dash_capital_c_on_a_bad_script_errors_and_writes_no_file() {
        let script = scratch_path("bad.qpl");
        std::fs::write(&script, "select from where\n").unwrap();
        let out_qplc = scratch_path("bad.qplc");
        let _ = std::fs::remove_file(&out_qplc);

        let compile = qpl()
            .args([
                "-C",
                script.to_str().unwrap(),
                "-o",
                out_qplc.to_str().unwrap(),
            ])
            .output()
            .expect("run qpl -C");
        assert!(!compile.status.success());
        assert!(!compile.stderr.is_empty());
        assert!(!out_qplc.exists());

        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn dash_o_without_dash_capital_c_is_rejected() {
        let out = qpl()
            .args(["-o", "/tmp/whatever.qplc"])
            .output()
            .expect("run qpl -o");
        assert!(!out.status.success());
    }

    #[test]
    fn a_qplc_file_still_runs_after_its_source_is_deleted() {
        let script = scratch_path("ephemeral.qpl");
        std::fs::write(&script, "1+41\n").unwrap();
        let out_qplc = scratch_path("ephemeral.qplc");

        let compile = qpl()
            .args([
                "-C",
                script.to_str().unwrap(),
                "-o",
                out_qplc.to_str().unwrap(),
            ])
            .output()
            .expect("run qpl -C");
        assert!(compile.status.success());
        std::fs::remove_file(&script).expect("delete source");

        let run = qpl().arg(&out_qplc).output().expect("run compiled");
        assert!(
            run.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert!(String::from_utf8_lossy(&run.stdout).contains("42"));

        let _ = std::fs::remove_file(&out_qplc);
    }

    #[test]
    fn a_corrupted_qplc_file_fails_cleanly_not_a_panic() {
        let bad = scratch_path("corrupt.qplc");
        std::fs::write(&bad, b"QPLCnotarealprogram").unwrap();

        let mut child = qpl()
            .arg(&bad)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn qpl");
        // close stdin in case an interactive loop starts
        drop(child.stdin.take());
        let out = child.wait_with_output().expect("wait");
        assert!(!out.status.success());
        assert!(!out.stderr.is_empty());

        let _ = std::fs::remove_file(&bad);
    }

    #[test]
    fn interactive_flag_still_requires_a_file() {
        let out = qpl().arg("-i").output().expect("run qpl -i");
        assert!(!out.status.success());
    }

    /// In-process checks of clap's `conflicts_with_all`/`requires` wiring.
    #[test]
    fn clap_conflicts_are_wired_as_specified() {
        use clap::Parser;
        let ok = |args: &[&str]| super::Cli::try_parse_from(args).is_ok();
        let err = |args: &[&str]| super::Cli::try_parse_from(args).is_err();

        assert!(ok(&["qpl", "script.qpl"]));
        assert!(ok(&["qpl", "-i", "script.qpl"]));
        assert!(ok(&["qpl", "-C", "script.qpl"]));
        assert!(ok(&["qpl", "-C", "script.qpl", "-o", "out.qplc"]));
        assert!(ok(&["qpl", "-c", "1+1"]));
        assert!(ok(&["qpl", "--load-demo", "-c", "1+1"]));

        assert!(err(&["qpl", "-i"]), "-i requires a file");
        assert!(err(&["qpl", "-o", "out.qplc"]), "-o requires -C");
        assert!(
            err(&["qpl", "-c", "1+1", "script.qpl"]),
            "-c conflicts with file"
        );
        assert!(
            err(&["qpl", "-i", "-c", "1+1", "script.qpl"]),
            "-c conflicts with -i"
        );
        assert!(
            err(&["qpl", "-C", "a.qpl", "-c", "1+1"]),
            "-C conflicts with -c"
        );
        assert!(
            err(&["qpl", "-C", "a.qpl", "script.qpl"]),
            "-C conflicts with file"
        );
        assert!(
            err(&["qpl", "-C", "a.qpl", "-i"]),
            "-C conflicts with -i (which also needs a file)"
        );

        assert!(ok(&["qpl", "-w"]));
        assert!(ok(&["qpl", "--write", "script.qpl"]));
        assert!(ok(&["qpl", "-w", "-c", "1+1"]));
        assert!(ok(&["qpl", "-wi", "script.qpl"]));
        assert!(
            err(&["qpl", "-w", "-C", "a.qpl"]),
            "-w conflicts with -C (nothing runs)"
        );
        assert!(
            err(&["qpl", "-w", "-d", "a.qplc"]),
            "-w conflicts with -d (nothing runs)"
        );
    }

    #[test]
    fn sessions_are_read_only_unless_started_with_write() {
        let path = scratch_path("read_only_cli.parquet");
        let _ = std::fs::remove_file(&path);
        let sink = format!(
            "a: 1 2\nt: zip `a!a\nlog \"rows \" a\nt sink \"{}\"",
            path.display()
        );

        let out = qpl().args(["-c", &sink]).output().expect("run qpl -c");
        assert!(!out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "rows 1 2");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(
                "Cannot perform write action in read-only session: sink (start qpl with -w"
            ),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!path.exists());

        let out = qpl()
            .args(["-w", "-c", &sink])
            .output()
            .expect("run qpl -w -c");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }

    // the temp-file-then-rename write path, in-process
    #[test]
    fn compile_to_qplc_writes_via_a_temp_file_and_renames_into_place() {
        let mut script = tempfile_like("direct.qpl");
        writeln!(script.1, "1+1").unwrap();
        let out_path = scratch_path("direct.qplc");
        let _ = std::fs::remove_file(&out_path);

        super::compile_to_qplc(script.0.to_str().unwrap(), Some(out_path.to_str().unwrap()))
            .expect("compile");
        assert!(out_path.exists());
        let tmp = {
            let mut t = out_path.clone().into_os_string();
            t.push(".tmp");
            std::path::PathBuf::from(t)
        };
        assert!(
            !tmp.exists(),
            "temp file should be renamed away, not left behind"
        );

        let _ = std::fs::remove_file(&script.0);
        let _ = std::fs::remove_file(&out_path);
    }

    fn tempfile_like(name: &str) -> (std::path::PathBuf, std::fs::File) {
        let path = scratch_path(name);
        let f = std::fs::File::create(&path).expect("create scratch file");
        (path, f)
    }

    /// `examples/stdlib.qpl`, run through the built `qpl` binary (which
    /// registers `.std` — see `run`), diffed against
    /// `examples/golden/stdlib.out`. `UPDATE_GOLDEN=1 cargo test -p qpl-cli
    /// stdlib_example` regenerates it, same convention as the library's own
    /// golden tests (`src/repl.rs`, which can't see `qpl-std`).
    #[test]
    fn stdlib_example_matches_golden_output() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("qpl-cli has a parent directory");
        let script = root.join("examples/stdlib.qpl");
        let golden = root.join("examples/golden/stdlib.out");

        let out = qpl().arg(&script).output().expect("run qpl");
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();

        if std::env::var("UPDATE_GOLDEN").is_ok() {
            std::fs::write(&golden, &stdout).expect("write golden file");
            return;
        }
        let expected = std::fs::read_to_string(&golden).unwrap_or_else(|_| {
            panic!("missing golden file {golden:?}; run with UPDATE_GOLDEN=1 to create it")
        });
        assert_eq!(stdout, expected, "golden output mismatch for 'stdlib'");
    }
}
