//! Built-in (native) functions — resolved through [`crate::vm::Vm::builtins`]
//! exactly like a user function value (`Lookup::Builtin` alongside a `Value::Closure`
//! in [`crate::vm::Lookup`]), the one difference being that a name in this map
//! can never be bound over (see the `bind_*` guards in `vm.rs`).
//!
//! To add a builtin: register the name in [`builtins`] with a `NativeFn` that
//! implements it — no parser/lexer/compiler changes needed, since a builtin's
//! name is lexed and parsed as a plain identifier like any other.
//!
//! One call signature, `NativeFn`, is shared by every native in the
//! interpreter: this table's `.qpl.dt/tm/ts/dlta`
//! (name-keyed, unshadowable — checked by [`crate::vm::Vm::lookup`] ahead of
//! any user binding), [`NativeId`]'s `enlist`/`?` roll (id-keyed, unshadowable
//! — the compiler pushes the id directly, so no name lookup ever happens), and
//! `ops::call_by_name`'s `til`/`log`/`hopen`/`whopen`/`await` (name-matched,
//! shadowable — only reached once [`crate::vm::Vm::lookup`] has already found
//! neither a user closure nor an entry in this table). Which table a native
//! lives in is what decides its shadowability, not the signature.

use std::collections::HashMap;
use std::ops::RangeInclusive;

use crate::errors::QplError;
use crate::temporal::{self, TemporalNowFuncType};
use crate::vm::{Slot, Vm};

/// A value-context primitive referenced by id, not by name: `enlist` and
/// `?` (roll) are unconditional keywords (checked before a same-named user
/// function could ever shadow them), so the compiler pushes one of these
/// directly (`Operand::Native`) instead of the name, and `Op::Call` never has
/// to look either up. Every *other* value-context primitive (`til`, `hopen`,
/// `log`, …) is shadowable by a user function and so is resolved by name at
/// run time — see `ops::call_by_name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeId {
    Enlist,
    Roll,
    /// `.qpl.cfg key=value ...`: compiled from
    /// `ast::Stmt::Cfg`, never reachable as an identifier, so it's pushed by
    /// id like `Enlist`/`Roll` rather than resolved by name.
    Cfg,
    /// `\1 <path>` — point (or detach) the stdout log.
    StdoutLog,
    /// `\d <stmt>` — print a pre-rendered disassembly listing (the listing
    /// itself is computed at compile time; this just emits the text).
    PrintText,
    /// `\l <path>` — run an embedded [`crate::program::Operand::Program`]
    /// flat, in the current session scope.
    LoadScript,
    /// `\i "<path>"` — run an embedded `Operand::Program` as a namespaced
    /// import: snapshot/rollback semantics on failure.
    ImportScript,
}

/// The one call signature every native function in the interpreter shares:
/// it takes the already-evaluated
/// argument [`Slot`]s (so it can be reused unchanged whether the caller looked
/// it up by name or by [`NativeId`]) and `&mut Vm` (for the handful — `log`,
/// `hopen`, `await` — that need session state), and returns a [`Slot`] rather
/// than a bare [`crate::ast::Value`] so a native could in principle build a
/// `Frame` too, even though none of today's do.
pub(crate) type NativeFn = fn(&mut Vm, Vec<Slot>) -> Result<Slot, QplError>;

/// One native function: its accepted arity range (checked by the caller, the
/// same way a user function's `params.len()` is — but a range rather than a
/// single `usize` so an eventual variadic native needs no shape change) and
/// its implementation. `Clone`, not `Copy`: `RangeInclusive` isn't `Copy`
/// (deliberately, upstream — a `Copy` range invites double-iteration bugs),
/// even though the function pointer alone would be.
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

/// The session-wide builtin table, populated once in [`Vm::new`](crate::vm::Vm::new).
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

/// A human-readable rendering of an arity range for the "'{name}' takes …
/// argument(s)" error text: every builtin today has a fixed (single-value)
/// arity, which renders as a plain number.
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
