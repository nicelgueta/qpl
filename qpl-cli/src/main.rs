// matches Polars' official builds (see `mimalloc` in Cargo.toml)
#[global_allocator]
static GLOBAL: qpl_cli::MiMalloc = qpl_cli::MiMalloc;

fn main() {
    qpl_cli::run(Vec::new());
}
