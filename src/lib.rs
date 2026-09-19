//! The qpl interpreter as a library: `tokenise -> parse -> compile -> run_vm`,
//! plus the [`repl`] driver that feeds it one line at a time.
//!
//! The `qpl` binary (`main.rs`, `cli` feature) and the browser bindings
//! (`wasm.rs`, `wasm` feature) are both thin front-ends over this.

#[cfg(feature = "wasm")]
pub mod arrow_io;
pub mod ast;
pub mod builtins;
pub mod compiler;
pub mod enums;
pub mod errors;
pub mod helpers;
pub mod lexer;
pub mod native;
pub mod opcodes;
pub mod parser;
pub mod repl;
pub mod resolve;
pub mod temporal;
pub mod tokens;
pub mod vm;
pub mod vm_config;
#[cfg(feature = "ipc")]
pub mod ipc;
#[cfg(feature = "wasm")]
mod wasm;
