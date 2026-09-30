// matches Polars' official builds (see `mimalloc` in Cargo.toml)
#[global_allocator]
static GLOBAL: qpl::cli::MiMalloc = qpl::cli::MiMalloc;

fn main() {
    qpl::cli::run(Vec::new());
}
