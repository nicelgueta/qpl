//! The qpl interpreter as a library: lexer, parser, compiler and VM, with no
//! front-end of its own. The `qpl-cli` crate (the `qpl` binary) and
//! `qpl-wasm` (browser bindings) are thin front-ends over it; [`ext`] adds
//! Rust functions to it. The `wasm` feature enables only the plumbing a
//! non-terminal front-end needs from [`vm::Vm`] (captured table results) —
//! the actual browser bindings live in `qpl-wasm`.

// lets `#[qpl::native]`'s generated `::qpl::...` paths resolve inside this crate
extern crate self as qpl;

/// Expose a Rust function to qpl; see [`ext`].
pub use qpl_macros::native;

pub mod ast;
pub mod builtins;
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
