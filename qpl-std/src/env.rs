//! `.std.env`: environment variable access (the `os` feature). `s` writes
//! only to an in-process overlay, never to the real process environment —
//! env vars are set from outside the process, so a read-only session must
//! not be able to override them, and `std::env::set_var` is `unsafe` in
//! edition 2024 because it races with Polars' and IPC's own threads reading
//! the environment. `r`/`has`/`all` check the overlay first.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use qpl::ext::Extension;
use qpl::ext::polars::prelude::*;

static OVERLAY: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[qpl::native(iread)]
fn r(k: String) -> String {
    OVERLAY
        .lock()
        .unwrap()
        .get(&k)
        .cloned()
        .unwrap_or_else(|| std::env::var(&k).unwrap_or_default())
}

#[qpl::native(write)]
fn s(k: String, v: String) {
    OVERLAY.lock().unwrap().insert(k, v);
}

#[qpl::native(iread)]
fn has(k: String) -> bool {
    OVERLAY.lock().unwrap().contains_key(&k) || std::env::var(&k).is_ok()
}

#[qpl::native(iread, name = "all")]
fn all_vars() -> Result<DataFrame, String> {
    let mut vars: HashMap<String, String> = std::env::vars().collect();
    for (k, v) in OVERLAY.lock().unwrap().iter() {
        vars.insert(k.clone(), v.clone());
    }
    let mut names: Vec<String> = vars.keys().cloned().collect();
    names.sort();
    let values: Vec<String> = names.iter().map(|n| vars[n].clone()).collect();
    df!("name" => names, "value" => values).map_err(|e| e.to_string())
}

pub fn extension() -> Extension {
    Extension::new("std.env")
        .owner("qpl-std")
        .with::<r>()
        .with::<s>()
        .with::<has>()
        .with::<all_vars>()
}

#[cfg(test)]
mod tests {
    use qpl::ast::Value;
    use qpl::vm::{EvalResult, Vm, run_vm};

    fn expect_scalar(r: EvalResult) -> Value {
        match r {
            EvalResult::Scalar(v) => v,
            other => panic!("expected a scalar, got {other:?}"),
        }
    }

    fn writable_vm() -> Vm {
        let mut vm = Vm::new_writable();
        vm.register(super::extension()).expect("register .std.env");
        vm
    }

    /// A key unlikely to collide with a real environment variable or another
    /// test running in parallel (the overlay is process-global).
    fn unique_key(tag: &str) -> String {
        format!(
            "QPL_STD_ENV_TEST_{tag}_{:?}_{}",
            std::thread::current().id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn unset_key_reads_as_empty_and_has_is_false() {
        let key = unique_key("unset");
        let mut vm = writable_vm();
        assert_eq!(
            expect_scalar(run_vm(&format!(r#".std.env.r "{key}""#), &mut vm).unwrap()),
            Value::Str("".into())
        );
        assert_eq!(
            expect_scalar(run_vm(&format!(r#".std.env.has "{key}""#), &mut vm).unwrap()),
            Value::Bool(false)
        );
    }

    #[test]
    fn s_writes_only_to_the_overlay() {
        let key = unique_key("overlay");
        let mut vm = writable_vm();
        run_vm(&format!(r#".std.env.s["{key}"; "hi"]"#), &mut vm).expect("set");
        assert_eq!(
            expect_scalar(run_vm(&format!(r#".std.env.r "{key}""#), &mut vm).unwrap()),
            Value::Str("hi".into())
        );
        assert_eq!(
            expect_scalar(run_vm(&format!(r#".std.env.has "{key}""#), &mut vm).unwrap()),
            Value::Bool(true)
        );
        // never touches the real process environment
        assert!(std::env::var(&key).is_err());
    }

    #[test]
    fn s_is_refused_without_write_permission() {
        let key = unique_key("ro");
        let mut ro = Vm::new();
        ro.register(super::extension()).unwrap();
        let err = run_vm(&format!(r#".std.env.s["{key}"; "x"]"#), &mut ro).unwrap_err();
        assert!(err.to_string().contains("read-only") || err.to_string().contains("write"));
    }

    #[test]
    fn s_is_allowed_with_write_permission() {
        let key = unique_key("rw");
        let mut vm = writable_vm();
        run_vm(&format!(r#".std.env.s["{key}"; "x"]"#), &mut vm).expect("set with -w");
    }

    #[test]
    fn all_lists_overlay_entries_sorted_with_overlay_winning() {
        let key = unique_key("all");
        let mut vm = writable_vm();
        run_vm(&format!(r#".std.env.s["{key}"; "overlay-value"]"#), &mut vm).expect("set");
        let EvalResult::Table(df) = run_vm(".std.env.all", &mut vm).unwrap() else {
            panic!("expected a table");
        };
        assert_eq!(df.get_column_names(), vec!["name", "value"]);
        let names = df.column("name").unwrap().str().unwrap();
        let values = df.column("value").unwrap().str().unwrap();
        let mut found = false;
        let mut prev: Option<&str> = None;
        for i in 0..df.height() {
            let n = names.get(i).unwrap();
            if let Some(p) = prev {
                assert!(p <= n, "name column isn't sorted: {p} then {n}");
            }
            prev = Some(n);
            if n == key {
                found = true;
                assert_eq!(values.get(i).unwrap(), "overlay-value");
            }
        }
        assert!(found, "overlay entry missing from .std.env.all");
    }

    #[test]
    #[cfg(feature = "ipc")]
    fn ireads_are_refused_over_a_read_only_handle() {
        use qpl::ipc::HandleMode;

        let mut vm = writable_vm();
        let ok =
            vm.with_request_permission(HandleMode::Read, |vm| run_vm(r#".std.env.r "PATH""#, vm));
        assert!(
            ok.is_err(),
            "iread should be refused over a read-only handle"
        );

        let ok =
            vm.with_request_permission(HandleMode::Write, |vm| run_vm(r#".std.env.r "PATH""#, vm));
        assert!(ok.is_ok(), "iread is allowed over a write handle");
    }
}
