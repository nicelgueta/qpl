use clap::{ArgAction::SetTrue, Parser};
use qpl::{repl, vm};

// Matches Polars' own official builds — see the `mimalloc` entry in Cargo.toml.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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

fn main() {
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

    let mut vm = vm::Vm::new();
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
        None => repl::start(&mut vm),
        Some(ref path) => {
            if let Err(e) = repl::run_script(path, &mut vm) {
                eprintln!("{e}");
                let interrupted = matches!(e, qpl::errors::QplError::Interrupted);
                // `-i` after an interrupted script still drops into the REPL
                if !(interrupted && cli.interactive) {
                    std::process::exit(if interrupted { 130 } else { 1 });
                }
            }
            if cli.interactive {
                repl::start(&mut vm);
            }
        }
    }
}

/// `qpl -C script.qpl [-o out.qplc]`: compile `script` to a `.qplc` file and
/// exit without running it. Writes to a temp file in the same location as
/// the final output and renames it into place, so a compile error partway
/// through never leaves a truncated/corrupt artifact where the real output
/// was expected.
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

/// Ctrl-C while a statement runs asks it to stop at its next check point; a
/// second Ctrl-C (or one with nothing running) exits, as an unhandled SIGINT
/// would. Inside `readline` the terminal is in raw mode, so none of this fires.
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
    //! CLI behaviour tests: spawn the actual
    //! built binary rather than calling `main` in-process, since the thing
    //! under test is clap's argument handling (conflicts, `requires`) and
    //! process exit codes — both only observable from outside.
    //!
    //! `env!("CARGO_BIN_EXE_qpl")` (the usual way to locate a sibling
    //! binary's freshly-built path from a test) isn't available here: it's
    //! only set for *other* targets in the package that depend on this bin,
    //! not for the bin's own unit tests. Instead `qpl_bin_path` derives
    //! `target/<profile>/qpl` from this test binary's own
    //! `std::env::current_exe()` (which sits at
    //! `target/<profile>/deps/qpl-<hash>`), building it on demand
    //! (`cargo build --quiet`, once per test process) if it isn't there yet —
    //! robust to `cargo test` being run without a preceding `cargo build`.
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
        // running a bad file with no `file` positional consumed doesn't start
        // an interactive loop, but close stdin defensively in case it would.
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

    /// Fast, no-process-spawn checks of clap's wiring itself (`conflicts_with_all`/
    /// `requires`), complementing the slower end-to-end spawns above.
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
    }

    // Kept for completeness alongside the process-spawning tests above:
    // exercises the same temp-file-then-rename write path `compile_to_qplc`
    // uses, in-process, without needing a real qpl script.
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
}
