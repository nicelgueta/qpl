//! `.std`: qpl's standard library, built entirely on the public extension
//! API (`qpl::ext`) — no access to VM internals, as a reference example of
//! that API.
//!
//! - [`str`]: string functions, working on a single string, a symbol, or a
//!   list of either.
//! - [`arr`]: list functions that work on any list kind.
//! - [`fs`] / [`env`] (the `os` feature, on by default): filesystem and
//!   environment variable access.
//!
//! A path, string or list is always the first argument; a string argument
//! also accepts a symbol. Errors come back as `.std.<ns>.<fn>: <message>`.

pub mod arr;
#[cfg(feature = "os")]
pub mod env;
#[cfg(feature = "os")]
pub mod fs;
pub mod str;

use qpl::errors::QplError;
use qpl::ext::Extension;
use qpl::vm::Vm;

/// Every `.std` extension.
pub fn extensions() -> Vec<Extension> {
    #[allow(unused_mut)]
    let mut exts = vec![str::extension(), arr::extension()];
    #[cfg(feature = "os")]
    {
        exts.push(fs::extension());
        exts.push(env::extension());
    }
    exts
}

/// Register every `.std` extension on `vm`.
pub fn register(vm: &mut Vm) -> Result<(), QplError> {
    for ext in extensions() {
        vm.register(ext)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use qpl::vm::{EvalResult, run_vm};

    fn expect_scalar(r: EvalResult) -> qpl::ast::Value {
        match r {
            EvalResult::Scalar(v) => v,
            other => panic!("expected a scalar, got {other:?}"),
        }
    }

    fn eval(src: &str) -> EvalResult {
        let mut vm = Vm::new_writable();
        register(&mut vm).expect("register .std");
        run_vm(src, &mut vm).expect("eval")
    }

    #[test]
    fn registers_without_conflicts() {
        let mut vm = Vm::new();
        assert!(register(&mut vm).is_ok());
    }

    #[test]
    fn a_namespace_is_reachable() {
        assert_eq!(
            expect_scalar(eval(r#".std.str.u "abc""#)),
            qpl::ast::Value::Str("ABC".into())
        );
    }
}
