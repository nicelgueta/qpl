//! Built-in (native) functions — resolved through [`crate::vm::Vm::builtins`]
//! exactly like a user function (`Lookup::Builtin` alongside `Lookup::Function`
//! in [`crate::vm::Lookup`]), the one difference being that a name in this map
//! can never be bound over (see the `bind_*` guards in `vm.rs`).
//!
//! To add a builtin: give it a variant of whatever kind [`Builtin`] carries,
//! register the name in [`builtins`] and dispatch it in [`Builtin::call`] —
//! no parser/lexer/compiler changes needed, since a builtin's name is lexed
//! and parsed as a plain identifier like any other.

use std::collections::HashMap;

use crate::ast::Value;
use crate::errors::QplError;
use crate::temporal;

/// One native function: its arity (checked by the caller, the same way a user
/// function's `params.len()` is) and which implementation it names. Copy since
/// it's just a `usize` plus a tag.
#[derive(Clone, Copy)]
pub struct Builtin {
    pub arity: usize,
    pub typ: temporal::TemporalNowFuncType,
}

/// The session-wide builtin table, populated once in [`Vm::new`](crate::vm::Vm::new).
pub fn builtins() -> HashMap<String, Builtin> {
    let mut m = HashMap::new();
    m.insert(".qpl.dt".to_string(), Builtin { arity: 0, typ: temporal::TemporalNowFuncType::Date });
    m.insert(".qpl.tm".to_string(), Builtin { arity: 0, typ: temporal::TemporalNowFuncType::Time });
    m.insert(".qpl.ts".to_string(), Builtin { arity: 0, typ: temporal::TemporalNowFuncType::Timestamp });
    m.insert(".qpl.dlta".to_string(), Builtin { arity: 0, typ: temporal::TemporalNowFuncType::Timespan });
    m
}

impl Builtin {
    /// Run the builtin. `args` has already been arity-checked against
    /// [`Builtin::arity`] by the caller (`resolve::apply_function`), so a
    /// niladic builtin can ignore it.
    pub fn call(&self, _args: &[Value]) -> Result<Value, QplError> {
        // every builtin so far is one of the niladic now-functions
        temporal::now_value(self.typ)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_is_callable_and_matches_its_arity() {
        for (name, b) in builtins() {
            assert_eq!(b.arity, 0, "{name} is niladic");
            assert!(b.call(&[]).is_ok(), "{name} returned an error");
        }
    }
}
