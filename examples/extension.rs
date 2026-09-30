//! A `qpl` binary with a Rust extension: every `qpl` feature, plus a `.geo`
//! namespace. Run from the repo root:
//!
//!   cargo run --example extension -- -c '.geo.km[51.51;-0.13;40.71;-74.01]'
//!   cargo run --example extension -- -w -c '.geo.note["notes.txt"; "hello"]'
//!
//! `.geo.km` is a read, so it works in every session. `.geo.note` writes to a
//! file, so it's refused unless the session was started with `-w`.

use std::io::Write;

// the same allocator the stock `qpl` binary uses (optional)
#[global_allocator]
static GLOBAL: qpl::cli::MiMalloc = qpl::cli::MiMalloc;

/// Great-circle distance in km between two latitude/longitude points.
#[qpl::native(read)]
fn km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    6371.0 * 2.0 * a.sqrt().asin()
}

/// Append `text` as a line to the file at `path`.
#[qpl::native(write)]
fn note(path: String, text: String) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{text}")
}

fn main() {
    let geo = qpl::ext::Extension::new("geo").with::<km>().with::<note>();
    qpl::cli::run(vec![geo]);
}
