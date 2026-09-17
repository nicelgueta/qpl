//! Built-in (native) functions — resolved through [`crate::vm::Vm::builtins`]
//! exactly like a user function (`Lookup::Builtin` alongside `Lookup::Function`
//! in [`crate::vm::Lookup`]), the one difference being that a name in this map
//! can never be bound over (see the `bind_*` guards in `vm.rs`).
//!
//! To add a builtin: write a `fn(&Vm, &[Value]) -> Result<Value, QplError>`
//! below and register it in [`builtins`] — no parser/lexer/compiler changes
//! needed, since a builtin's name is lexed and parsed as a plain identifier
//! like any other.

use std::collections::HashMap;

use crate::ast::Value;
use crate::errors::QplError;
use crate::temporal;
use crate::vm::Vm;

/// One native function: its arity (checked the same way a user function's
/// `params.len()` is) and the Rust implementation. Copy since it's just a
/// `usize` plus a function pointer.
#[derive(Clone, Copy)]
pub struct Builtin {
    pub arity: usize,
    pub call: fn(&Vm, &[Value]) -> Result<Value, QplError>,
}

/// The session-wide builtin table, populated once in [`Vm::new`](crate::vm::Vm::new).
pub fn builtins() -> HashMap<String, Builtin> {
    let mut m = HashMap::new();
    m.insert(".qpl.d".to_string(), Builtin { arity: 0, call: now_d });
    m.insert(".qpl.t".to_string(), Builtin { arity: 0, call: now_t });
    m.insert(".qpl.p".to_string(), Builtin { arity: 0, call: now_p });
    m.insert(".qpl.n".to_string(), Builtin { arity: 0, call: now_n });
    m
}

// `.qpl.d` / `.qpl.t` / `.qpl.p` / `.qpl.n` — nullary now-functions (date /
// time / timestamp / timespan, UTC). One trampoline per name since a
// `Builtin::call` is a plain `fn` pointer with no way to close over which
// key it was registered under.
fn now_d(_vm: &Vm, _args: &[Value]) -> Result<Value, QplError> { temporal::now_value(".qpl.d") }
fn now_t(_vm: &Vm, _args: &[Value]) -> Result<Value, QplError> { temporal::now_value(".qpl.t") }
fn now_p(_vm: &Vm, _args: &[Value]) -> Result<Value, QplError> { temporal::now_value(".qpl.p") }
fn now_n(_vm: &Vm, _args: &[Value]) -> Result<Value, QplError> { temporal::now_value(".qpl.n") }
