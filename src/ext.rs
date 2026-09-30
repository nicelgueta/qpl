//! Rust extensions: native functions written in Rust and called from qpl.
//!
//! An extension function is an ordinary Rust function marked with
//! [`#[qpl::native(read)]`](crate::native) or `#[qpl::native(write)]`. The
//! permission is required: the VM checks it before every call, so a read-only
//! session refuses a `write` extension exactly as it refuses `sink`.
//!
//! ```ignore
//! #[qpl::native(read)]
//! fn haversine(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 { ... }
//!
//! fn main() {
//!     qpl::cli::run(vec![qpl::ext::Extension::new("geo").with::<haversine>()]);
//! }
//! ```
//!
//! That binary is `qpl` with `.geo.haversine[a;b;c;d]` added. Extensions are
//! linked at compile time (Rust has no stable ABI to load them at run time),
//! always live under their own namespace, and, like every builtin, can never
//! be rebound by a script.
//!
//! Arguments and results convert through [`FromValue`] and [`IntoValue`]. A
//! function may return `()` (nothing to print) or a `Result` whose error is
//! reported as a qpl runtime error.

use std::fmt::Display;

use polars::prelude::*;

use crate::ast::{self, Value};
use crate::errors::QplError;
use crate::native::{Builtin, NativeCall};
use crate::vm::Vm;

pub use crate::permission::Effect;

/// The Polars qpl is built against. An extension that takes or returns
/// tables must use these types, so it should import them from here rather
/// than depend on its own (possibly different) Polars version.
pub use polars;

/// An extension function's entry point: converted arguments in, `None` for a
/// function that returns `()`. The VM prefixes an error with the qpl name.
pub type ExtFn = fn(Vec<Value>) -> Result<Option<Value>, String>;

/// A Rust function exposed to qpl. Implemented by `#[qpl::native]`, which
/// generates a type of the same name as the function to carry it.
pub trait Native {
    /// The name after the extension's namespace: `NAME` in `.<ns>.<NAME>`.
    const NAME: &'static str;
    /// What the function may change; a read-only session refuses `Write`.
    const EFFECT: Effect;
    /// The exact number of arguments it takes.
    const ARITY: usize;
    /// Convert `args` (exactly `ARITY` of them), call, convert the result.
    fn call(args: Vec<Value>) -> Result<Option<Value>, String>;
}

/// A named group of extension functions, registered with
/// [`Vm::register`] (or passed to [`crate::cli::run`]).
pub struct Extension {
    namespace: String,
    functions: Vec<(&'static str, Effect, usize, ExtFn)>,
}

impl Extension {
    /// An empty extension whose functions are called as `.<namespace>.<name>`.
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            functions: Vec::new(),
        }
    }

    /// Add the function marked `#[qpl::native]` as `N`.
    pub fn with<N: Native>(mut self) -> Self {
        self.functions.push((N::NAME, N::EFFECT, N::ARITY, N::call));
        self
    }
}

impl Vm {
    /// Add `ext`'s functions to this session's builtins. Fails, adding
    /// nothing, on a bad or reserved namespace or a name already taken.
    pub fn register(&mut self, ext: Extension) -> Result<(), QplError> {
        let ns = &ext.namespace;
        if !is_ident(ns) {
            return Err(QplError::Runtime(format!(
                "extension namespace '{ns}' must be a plain identifier (letters, digits, '_')"
            )));
        }
        if ns == "qpl" {
            return Err(QplError::Runtime(
                "extension namespace 'qpl' is reserved for qpl's own builtins".into(),
            ));
        }
        let mut entries = Vec::with_capacity(ext.functions.len());
        for &(name, effect, arity, call) in &ext.functions {
            if !is_ident(name) {
                return Err(QplError::Runtime(format!(
                    "extension function name '{name}' must be a plain identifier"
                )));
            }
            let full = format!(".{ns}.{name}");
            if self.builtins.contains_key(&full) || entries.iter().any(|(n, _)| *n == full) {
                return Err(QplError::Runtime(format!(
                    "extension function '{full}' is already defined"
                )));
            }
            let builtin = Builtin {
                arity: arity..=arity,
                effect,
                call: NativeCall::Extension(call),
            };
            entries.push((full, builtin));
        }
        self.builtins.extend(entries);
        Ok(())
    }
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A qpl value's type, for conversion errors.
fn kind(v: &Value) -> String {
    match v {
        Value::Int(_) => "an int".into(),
        Value::Float(_) => "a float".into(),
        Value::Str(_) => "a string".into(),
        Value::Sym(_) => "a symbol".into(),
        Value::Bool(_) => "a bool".into(),
        Value::Table(_) => "a table".into(),
        Value::Lazy(_) => "a lazy table".into(),
        Value::Closure(_) => "a function".into(),
        Value::Handle(_) => "a connection handle".into(),
        Value::Future(_) => "a pending response".into(),
        other => match other.as_vec() {
            Some((k, _)) => format!("a {} list", format!("{k:?}").to_lowercase()),
            None => format!(
                "a {}",
                format!("{other:?}")
                    .split('(')
                    .next()
                    .unwrap_or("value")
                    .to_lowercase()
            ),
        },
    }
}

fn expected(what: &str, v: &Value) -> String {
    format!("expected {what}, got {}", kind(v))
}

/// Conversion from a qpl argument to a Rust parameter type.
pub trait FromValue: Sized {
    fn from_value(v: Value) -> Result<Self, String>;
}

/// Conversion from a Rust result to a qpl value.
pub trait IntoValue {
    fn into_value(self) -> Value;
}

impl FromValue for Value {
    fn from_value(v: Value) -> Result<Self, String> {
        Ok(v)
    }
}

impl FromValue for i64 {
    fn from_value(v: Value) -> Result<Self, String> {
        match v {
            Value::Int(n) => Ok(n),
            other => Err(expected("an int", &other)),
        }
    }
}

impl FromValue for f64 {
    fn from_value(v: Value) -> Result<Self, String> {
        match v {
            Value::Float(f) => Ok(f),
            Value::Int(n) => Ok(n as f64),
            other => Err(expected("a float", &other)),
        }
    }
}

impl FromValue for bool {
    fn from_value(v: Value) -> Result<Self, String> {
        match v {
            Value::Bool(b) => Ok(b),
            other => Err(expected("a bool", &other)),
        }
    }
}

/// A string or a symbol.
impl FromValue for String {
    fn from_value(v: Value) -> Result<Self, String> {
        match v {
            Value::Str(s) | Value::Sym(s) => Ok(s),
            other => Err(expected("a string", &other)),
        }
    }
}

/// A table; a lazy one is collected first.
impl FromValue for DataFrame {
    fn from_value(v: Value) -> Result<Self, String> {
        match v {
            Value::Table(df) => Ok(df),
            Value::Lazy(lf) => lf.collect().map_err(|e| e.to_string()),
            other => Err(expected("a table", &other)),
        }
    }
}

/// A table as a plan, so a lazy argument stays lazy.
impl FromValue for LazyFrame {
    fn from_value(v: Value) -> Result<Self, String> {
        match v {
            Value::Lazy(lf) => Ok(*lf),
            Value::Table(df) => Ok(df.lazy()),
            other => Err(expected("a table", &other)),
        }
    }
}

/// Any list, as its backing `Series`.
impl FromValue for Series {
    fn from_value(v: Value) -> Result<Self, String> {
        match v.as_vec() {
            Some((_, s)) => Ok(s.clone()),
            None => Err(expected("a list", &v)),
        }
    }
}

/// A list without nulls, converted element by element.
fn list<T>(
    v: Value,
    what: &str,
    accept: fn(&Value) -> bool,
    dtype: DataType,
    get: fn(&Series) -> PolarsResult<Vec<Option<T>>>,
) -> Result<Vec<T>, String> {
    if !accept(&v) {
        return Err(expected(what, &v));
    }
    let (_, s) = v.as_vec().expect("accept only passes lists");
    let s = s.cast(&dtype).map_err(|e| e.to_string())?;
    get(&s)
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect::<Option<Vec<T>>>()
        .ok_or_else(|| format!("expected {what} without nulls"))
}

impl FromValue for Vec<i64> {
    fn from_value(v: Value) -> Result<Self, String> {
        list(
            v,
            "an int list",
            |v| matches!(v, Value::IntVec(_)),
            DataType::Int64,
            |s| Ok(s.i64()?.iter().collect()),
        )
    }
}

impl FromValue for Vec<f64> {
    fn from_value(v: Value) -> Result<Self, String> {
        list(
            v,
            "a float list",
            |v| matches!(v, Value::FloatVec(_) | Value::IntVec(_)),
            DataType::Float64,
            |s| Ok(s.f64()?.iter().collect()),
        )
    }
}

impl FromValue for Vec<bool> {
    fn from_value(v: Value) -> Result<Self, String> {
        list(
            v,
            "a bool list",
            |v| matches!(v, Value::BoolVec(_)),
            DataType::Boolean,
            |s| Ok(s.bool()?.iter().collect()),
        )
    }
}

/// A string or symbol list.
impl FromValue for Vec<String> {
    fn from_value(v: Value) -> Result<Self, String> {
        list(
            v,
            "a string list",
            |v| matches!(v, Value::StrVec(_) | Value::SymVec(_)),
            DataType::String,
            |s| Ok(s.str()?.iter().map(|o| o.map(str::to_owned)).collect()),
        )
    }
}

impl IntoValue for Value {
    fn into_value(self) -> Value {
        self
    }
}

impl IntoValue for i64 {
    fn into_value(self) -> Value {
        Value::Int(self)
    }
}

impl IntoValue for f64 {
    fn into_value(self) -> Value {
        Value::Float(self)
    }
}

impl IntoValue for bool {
    fn into_value(self) -> Value {
        Value::Bool(self)
    }
}

impl IntoValue for String {
    fn into_value(self) -> Value {
        Value::Str(self)
    }
}

impl IntoValue for &str {
    fn into_value(self) -> Value {
        Value::Str(self.to_owned())
    }
}

impl IntoValue for DataFrame {
    fn into_value(self) -> Value {
        Value::Table(self)
    }
}

/// Returned as a lazy table, so the caller decides when it runs.
impl IntoValue for LazyFrame {
    fn into_value(self) -> Value {
        Value::Lazy(Box::new(self))
    }
}

impl IntoValue for Vec<i64> {
    fn into_value(self) -> Value {
        ast::int_vec(self)
    }
}

impl IntoValue for Vec<f64> {
    fn into_value(self) -> Value {
        ast::float_vec(self)
    }
}

impl IntoValue for Vec<bool> {
    fn into_value(self) -> Value {
        ast::bool_vec(self)
    }
}

impl IntoValue for Vec<String> {
    fn into_value(self) -> Value {
        ast::str_vec(self)
    }
}

/// What a `#[qpl::native]` function may return: any [`IntoValue`], `()`, or
/// a `Result` of either whose error is `Display`.
pub trait IntoReturn {
    fn into_return(self) -> Result<Option<Value>, String>;
}

impl<T: IntoValue> IntoReturn for T {
    fn into_return(self) -> Result<Option<Value>, String> {
        Ok(Some(self.into_value()))
    }
}

impl IntoReturn for () {
    fn into_return(self) -> Result<Option<Value>, String> {
        Ok(None)
    }
}

impl<T: IntoReturn, E: Display> IntoReturn for Result<T, E> {
    fn into_return(self) -> Result<Option<Value>, String> {
        self.map_err(|e| e.to_string())?.into_return()
    }
}

/// Convert argument `index` (0-based) for the generated [`Native::call`].
#[doc(hidden)]
pub fn arg<T: FromValue>(args: &mut std::vec::IntoIter<Value>, index: usize) -> Result<T, String> {
    let v = args.next().expect("the VM checks arity before calling");
    T::from_value(v).map_err(|e| format!("argument {}: {e}", index + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::{EvalResult, run_vm};

    #[crate::native(read)]
    fn add(a: i64, b: i64) -> i64 {
        a + b
    }

    #[crate::native(read, name = "mean")]
    fn average(xs: Vec<f64>) -> Result<f64, String> {
        if xs.is_empty() {
            return Err("mean of an empty list".into());
        }
        Ok(xs.iter().sum::<f64>() / xs.len() as f64)
    }

    #[crate::native(read)]
    fn answer() -> i64 {
        42
    }

    #[crate::native(read)]
    fn shout(s: String) -> String {
        s.to_uppercase()
    }

    #[crate::native(read)]
    fn head2(t: DataFrame) -> DataFrame {
        t.head(Some(2))
    }

    #[crate::native(read)]
    fn passthrough(t: LazyFrame) -> LazyFrame {
        t
    }

    #[crate::native(write)]
    fn touch(path: String) -> std::io::Result<()> {
        std::fs::write(path, "")
    }

    fn test_ext() -> Extension {
        Extension::new("t")
            .with::<add>()
            .with::<average>()
            .with::<answer>()
            .with::<shout>()
            .with::<head2>()
            .with::<passthrough>()
            .with::<touch>()
    }

    fn vm_with(mut vm: Vm) -> Vm {
        vm.register(test_ext()).expect("register");
        let df = df!["a" => [1i64, 2, 3]].unwrap();
        vm.globals.insert("t".into(), Value::Table(df));
        vm
    }

    fn scalar(vm: &mut Vm, src: &str) -> Value {
        match run_vm(src, vm) {
            Ok(EvalResult::Scalar(v)) => v,
            other => panic!("expected a scalar from '{src}', got {other:?}"),
        }
    }

    fn runtime_err(vm: &mut Vm, src: &str) -> String {
        match run_vm(src, vm) {
            Err(QplError::Runtime(msg)) => msg,
            other => panic!("expected '{src}' to fail, got {other:?}"),
        }
    }

    #[test]
    fn the_function_itself_is_still_callable_from_rust() {
        assert_eq!(add(2, 3), 5);
        assert_eq!(<add as Native>::ARITY, 2);
        assert_eq!(<add as Native>::EFFECT, Effect::Read);
        assert_eq!(<touch as Native>::EFFECT, Effect::Write);
    }

    #[test]
    fn calls_convert_arguments_and_results() {
        let mut vm = vm_with(Vm::new());
        assert!(matches!(scalar(&mut vm, ".t.add[2;3]"), Value::Int(5)));
        assert!(matches!(scalar(&mut vm, ".t.mean[1 2 3]"), Value::Float(f) if f == 2.0));
        assert!(matches!(scalar(&mut vm, r#".t.shout["hi"]"#), Value::Str(s) if s == "HI"));
        assert!(matches!(scalar(&mut vm, ".t.shout[`hi]"), Value::Str(s) if s == "HI"));
    }

    #[test]
    fn a_niladic_function_is_called_by_naming_it() {
        let mut vm = vm_with(Vm::new());
        assert!(matches!(scalar(&mut vm, ".t.answer"), Value::Int(42)));
        run_vm("x: .t.answer + 1", &mut vm).expect("assign");
        assert!(matches!(scalar(&mut vm, "x"), Value::Int(43)));
    }

    #[test]
    fn tables_go_in_and_out() {
        let mut vm = vm_with(Vm::new());
        match run_vm(".t.head2[t]", &mut vm) {
            Ok(EvalResult::Table(df)) => assert_eq!(df.height(), 2),
            other => panic!("expected a table, got {other:?}"),
        }
        run_vm("l: lazy select from t", &mut vm).expect("lazy binding");
        assert!(matches!(
            run_vm(".t.passthrough[l]", &mut vm),
            Ok(EvalResult::Lazy(_))
        ));
    }

    #[test]
    fn a_rename_replaces_the_rust_name() {
        let mut vm = vm_with(Vm::new());
        let msg = runtime_err(&mut vm, ".t.average[1 2 3]");
        assert!(!msg.contains("argument"), "{msg}");
    }

    #[test]
    fn errors_name_the_function() {
        let mut vm = vm_with(Vm::new());
        assert_eq!(
            runtime_err(&mut vm, r#".t.mean["x"]"#),
            ".t.mean: argument 1: expected a float list, got a string"
        );
        assert_eq!(
            runtime_err(&mut vm, ".t.add[1]"),
            "'.t.add' takes 2 argument(s), got 1"
        );
        assert_eq!(
            runtime_err(&mut vm, r#".t.add[1;"x"]"#),
            ".t.add: argument 2: expected an int, got a string"
        );
    }

    #[test]
    fn a_returned_err_is_a_runtime_error() {
        let mut vm = vm_with(Vm::new());
        run_vm("xs: 1 2", &mut vm).expect("assign");
        assert_eq!(
            runtime_err(&mut vm, ".t.mean[0#xs]"),
            ".t.mean: mean of an empty list"
        );
    }

    #[test]
    fn a_write_function_is_refused_in_a_read_only_session() {
        let path = std::env::temp_dir().join("qpl_ext_test_read_only_touch");
        let _ = std::fs::remove_file(&path);
        let src = format!(r#".t.touch["{}"]"#, path.display());

        let mut vm = vm_with(Vm::new());
        assert_eq!(
            runtime_err(&mut vm, &src),
            "Cannot perform write action in read-only session: .t.touch (start qpl with -w to allow writes)"
        );
        assert!(!path.exists());

        let mut vm = vm_with(Vm::new_writable());
        run_vm(&src, &mut vm).expect("a write session may write");
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "ipc")]
    #[test]
    fn a_read_handle_refuses_a_write_function() {
        let mut vm = vm_with(Vm::new_writable());
        let path = std::env::temp_dir().join("qpl_ext_test_read_handle_touch");
        let src = format!(r#".t.touch["{}"]"#, path.display());
        let result =
            vm.with_request_permission(crate::ipc::HandleMode::Read, |vm| run_vm(&src, vm));
        assert!(matches!(result, Err(QplError::Runtime(m)) if m.contains("read-only connection")));
        assert!(!path.exists());
    }

    #[test]
    fn extension_names_cannot_be_rebound() {
        let mut vm = vm_with(Vm::new());
        assert!(run_vm(".t.add: 1", &mut vm).is_err());
        assert!(matches!(scalar(&mut vm, ".t.add[1;1]"), Value::Int(2)));
    }

    #[test]
    fn register_rejects_bad_namespaces_and_duplicates_atomically() {
        let mut vm = Vm::new();
        for ns in ["qpl", "", "a.b", "1x", "has space"] {
            assert!(
                vm.register(Extension::new(ns).with::<add>()).is_err(),
                "namespace '{ns}' should be rejected"
            );
        }
        assert!(
            vm.register(Extension::new("t").with::<add>().with::<add>())
                .is_err()
        );
        assert!(
            !vm.builtins.contains_key(".t.add"),
            "a failed register adds nothing"
        );

        vm.register(Extension::new("t").with::<add>())
            .expect("first");
        assert!(
            vm.register(Extension::new("t").with::<answer>().with::<add>())
                .is_err()
        );
        assert!(
            !vm.builtins.contains_key(".t.answer"),
            "a failed register adds nothing"
        );
    }
}
