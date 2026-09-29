//! The qpl interpreter as a library. The `qpl` binary (`main.rs`, `cli`
//! feature) and the browser bindings (`wasm.rs`, `wasm` feature) are thin
//! front-ends over it.

#[cfg(feature = "wasm")]
pub mod arrow_io;
pub mod ast;
pub mod builtins;
pub mod codec;
pub mod compiler;
pub mod errors;
pub mod helpers;
pub mod interrupt;
#[cfg(feature = "ipc")]
pub mod ipc;
pub mod lexer;
pub mod native;
pub mod ops;
pub mod parser;
pub mod program;
pub mod repl;
pub mod temporal;
pub mod tokens;
pub mod vm;
pub mod vm_config;
#[cfg(feature = "wasm")]
mod wasm;
