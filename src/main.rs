use clap::{ArgAction::SetTrue, Parser};
use qpl::{repl, vm};

// Matches Polars' own official builds — see the `mimalloc` entry in Cargo.toml.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "qpl", about = "Quick Polars Language", version)]
struct Cli {
    /// Script file to execute (.qpl)
    file: Option<String>,

    /// Run script then drop into the REPL
    #[arg(short = 'i', requires = "file")]
    interactive: bool,

    #[arg(long = "load-demo", action = SetTrue)]
    load_demo: bool

}

fn main() {
    let cli = Cli::parse();
    let mut vm = vm::Vm::new();
    install_ctrl_c(&vm);

    if cli.load_demo {
        println!("Loading demo tables `trades` and `quotes`");
        repl::load_demo_tables(&mut vm)
    };
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
