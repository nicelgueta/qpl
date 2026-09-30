//! The qpl interpreter as a library. The `qpl` binary (`main.rs` over
//! [`cli`], `cli` feature) and the browser bindings (`wasm.rs`, `wasm`
//! feature) are thin front-ends over it; [`ext`] adds Rust functions to it.

// lets `#[qpl::native]`'s generated `::qpl::...` paths resolve inside this crate
extern crate self as qpl;

/// Expose a Rust function to qpl; see [`ext`].
pub use qpl_macros::native;

#[cfg(feature = "wasm")]
pub mod arrow_io;
pub mod ast;
pub mod builtins;
#[cfg(feature = "cli")]
pub mod cli;
pub mod codec;
pub mod compiler;
pub mod errors;
pub mod ext;
pub mod helpers;
pub mod interrupt;
#[cfg(feature = "ipc")]
pub mod ipc;
pub mod lexer;
pub mod native;
pub mod ops;
pub mod parser;
pub mod permission;
pub mod program;
pub mod repl;
pub mod temporal;
pub mod tokens;
pub mod vm;
pub mod vm_config;
#[cfg(feature = "wasm")]
mod wasm;
