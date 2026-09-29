//! Built-in (native) functions. All natives share the [`NativeFn`] signature;
//! where one is registered decides whether it can be shadowed:
//!
//! - this table (`.qpl.dt` etc.): looked up by name before any user binding,
//!   and can't be rebound;
//! - [`NativeId`] (`enlist`, roll, `\` commands): pushed by id by the
//!   compiler, so no name lookup happens;
//! - `ops::call_by_name` (`til`, `log`, `hopen`, ...): only reached when no
//!   user function or table entry matches, so a user function wins.
//!
//! Adding an entry to [`builtins`] needs no lexer/parser/compiler change.

use std::collections::HashMap;
use std::ops::RangeInclusive;

use crate::errors::QplError;
use crate::temporal::{self, TemporalNowFuncType};
use crate::vm::{Slot, Vm};

/// A primitive the compiler references by id (`Operand::Native`) rather than
/// by name, so it can never be shadowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeId {
    Enlist,
    Roll,
    /// `.qpl.cfg key=value ...`
    Cfg,
    /// `\1 <path>`: point (or detach) the stdout log.
    StdoutLog,
    /// `\d <stmt>`: print a disassembly rendered at compile time.
    PrintText,
    /// `\l <path>`: run an embedded `Operand::Program` in the current scope.
    LoadScript,
    /// `\i "<path>"`: run an embedded `Operand::Program` as a namespaced
    /// import, rolling back on failure.
    ImportScript,
    /// `\port [<n>]`: close any open listener, then open one on `n` if given.
    /// Servicing requests is left to the run loop (`repl::start`).
    Port,
}

/// The signature every native shares: evaluated argument slots in, one slot out.
pub(crate) type NativeFn = fn(&mut Vm, Vec<Slot>) -> Result<Slot, QplError>;

/// One native function: its accepted arity and implementation.
#[derive(Clone)]
pub(crate) struct Builtin {
    pub arity: RangeInclusive<usize>,
    pub call: NativeFn,
}

fn now_fn(typ: TemporalNowFuncType) -> NativeFn {
    match typ {
        TemporalNowFuncType::Date => |_vm, _args| {
            Ok(Slot::Scalar(temporal::now_value(
                TemporalNowFuncType::Date,
            )?))
        },
        TemporalNowFuncType::Time => |_vm, _args| {
            Ok(Slot::Scalar(temporal::now_value(
                TemporalNowFuncType::Time,
            )?))
        },
        TemporalNowFuncType::Timestamp => |_vm, _args| {
            Ok(Slot::Scalar(temporal::now_value(
                TemporalNowFuncType::Timestamp,
            )?))
        },
        TemporalNowFuncType::Timespan => |_vm, _args| {
            Ok(Slot::Scalar(temporal::now_value(
                TemporalNowFuncType::Timespan,
            )?))
        },
    }
}

/// The builtin table, built once in [`Vm::new`](crate::vm::Vm::new).
pub(crate) fn builtins() -> HashMap<String, Builtin> {
    let mut m = HashMap::new();
    m.insert(
        ".qpl.dt".to_string(),
        Builtin {
            arity: 0..=0,
            call: now_fn(TemporalNowFuncType::Date),
        },
    );
    m.insert(
        ".qpl.tm".to_string(),
        Builtin {
            arity: 0..=0,
            call: now_fn(TemporalNowFuncType::Time),
        },
    );
    m.insert(
        ".qpl.ts".to_string(),
        Builtin {
            arity: 0..=0,
            call: now_fn(TemporalNowFuncType::Timestamp),
        },
    );
    m.insert(
        ".qpl.dlta".to_string(),
        Builtin {
            arity: 0..=0,
            call: now_fn(TemporalNowFuncType::Timespan),
        },
    );
    m
}

/// An arity range as text for "takes N argument(s)" errors.
pub(crate) fn arity_desc(arity: &RangeInclusive<usize>) -> String {
    if arity.start() == arity.end() {
        arity.start().to_string()
    } else {
        format!("{}..{}", arity.start(), arity.end())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_is_callable_and_matches_its_arity() {
        let mut vm = Vm::new();
        for (name, b) in builtins() {
            assert_eq!(b.arity, 0..=0, "{name} is niladic");
            assert!(
                (b.call)(&mut vm, vec![]).is_ok(),
                "{name} returned an error"
            );
        }
    }
}
