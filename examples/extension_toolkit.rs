//! A `qpl` binary with two Rust extensions and several functions each,
//! covering every kind of argument and result. Run from the repo root:
//!
//!   cargo run --example extension_toolkit -- -c '.stats.mean[1 2 3 4]'
//!   cargo run --example extension_toolkit -- --load-demo -c '.stats.zscore[trades`price]'
//!   cargo run --example extension_toolkit -- --load-demo -c '.stats.top[trades; `price; 3]'
//!   cargo run --example extension_toolkit -- -c '.stats.about'
//!   cargo run --example extension_toolkit -- -w -c '.files.append["notes.txt"; ("one" "two")]'
//!
//! `.stats` is all reads. `.files` mixes a read (`exists`) with a write
//! (`append`), which is refused unless the session was started with `-w`.

use qpl::ext::Extension;
use qpl::ext::polars::prelude::*;
use std::io::Write;

// ---- .stats: reads only ----

/// The mean of a list; an int list is accepted too. Empty is an error.
#[qpl::native(read)]
fn mean(xs: Vec<f64>) -> Result<f64, String> {
    if xs.is_empty() {
        return Err("mean of an empty list".into());
    }
    Ok(xs.iter().sum::<f64>() / xs.len() as f64)
}

/// A list in, a list out: each value's distance from the mean in standard
/// deviations.
#[qpl::native(read)]
fn zscore(xs: Vec<f64>) -> Result<Vec<f64>, String> {
    let n = xs.len() as f64;
    if n < 2.0 {
        return Err("zscore needs at least two values".into());
    }
    let mu = xs.iter().sum::<f64>() / n;
    let sd = (xs.iter().map(|x| (x - mu).powi(2)).sum::<f64>() / n).sqrt();
    Ok(xs.iter().map(|x| (x - mu) / sd).collect())
}

/// A table in, a table out: the `n` rows with the largest `col`. Called as
/// `.stats.top`, not `.stats.top_n`. A Polars error becomes a qpl error.
#[qpl::native(read, name = "top")]
fn top_n(t: DataFrame, col: String, n: i64) -> PolarsResult<DataFrame> {
    let sorted = t.sort(
        [col.as_str()],
        SortMultipleOptions::default().with_order_descending(true),
    )?;
    Ok(sorted.head(Some(n.max(0) as usize)))
}

/// A lazy table stays lazy: this only adds to the plan, and nothing runs
/// until the caller collects or sinks the result.
#[qpl::native(read)]
fn sample(t: LazyFrame, n: i64) -> LazyFrame {
    t.limit(n.max(0) as IdxSize)
}

/// No arguments: called by naming it, like `.qpl.dt`.
#[qpl::native(read)]
fn about() -> &'static str {
    "stats: mean, zscore, top, sample"
}

// ---- .files: a read and a write in one namespace ----

#[qpl::native(read)]
fn exists(path: String) -> bool {
    std::path::Path::new(&path).exists()
}

/// Append each string as a line; returns how many were written.
#[qpl::native(write)]
fn append(path: String, lines: Vec<String>) -> std::io::Result<i64> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for line in &lines {
        writeln!(file, "{line}")?;
    }
    Ok(lines.len() as i64)
}

fn main() {
    let stats = Extension::new("stats")
        .with::<mean>()
        .with::<zscore>()
        .with::<top_n>()
        .with::<sample>()
        .with::<about>();
    let files = Extension::new("files").with::<exists>().with::<append>();
    qpl::cli::run(vec![stats, files]);
}
