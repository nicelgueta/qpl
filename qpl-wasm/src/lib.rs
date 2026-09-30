//! Browser bindings for qpl: a `Repl` object (one [`qpl::vm::Vm`] per
//! session) and Monaco editor configuration, built with `wasm-pack` (see
//! `scripts/build-wasm.sh` and `tools/wasm/README.md`).

pub mod arrow_io;
mod wasm;

pub use wasm::*;
