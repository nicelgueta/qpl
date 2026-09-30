//! `.std.fs`: filesystem access (the `os` feature). `parent`/`join`/`name`/
//! `ext` are lexical (`read`) and never touch the filesystem; every other
//! read is `iread`, every mutation `write`. There is no `cd`: it would
//! change process-wide state that `load`/`\l` depend on.

use std::path::Path;

use qpl::ast::Value;
use qpl::ext::Extension;

const NS_2000_TO_1970: i64 = qpl::temporal::NS_2000_TO_1970;

fn to_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[qpl::native(write)]
fn mkdir(p: String) -> Result<(), String> {
    std::fs::create_dir_all(&p).map_err(|e| e.to_string())
}

#[qpl::native(iread)]
fn exists(p: String) -> bool {
    Path::new(&p).exists()
}

#[qpl::native(iread)]
fn isdir(p: String) -> bool {
    Path::new(&p).is_dir()
}

#[qpl::native(iread)]
fn isfile(p: String) -> bool {
    Path::new(&p).is_file()
}

#[qpl::native(iread)]
fn abspath(p: String) -> Result<String, String> {
    std::path::absolute(&p)
        .map(|a| to_str(&a))
        .map_err(|e| e.to_string())
}

#[qpl::native(read)]
fn parent(p: String) -> String {
    let path = Path::new(&p);
    match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => ".".to_string(),
        Some(parent) => to_str(parent),
        None => to_str(path),
    }
}

#[qpl::native(iread)]
fn ls(p: String) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = std::fs::read_dir(&p)
        .map_err(|e| e.to_string())?
        .map(|entry| entry.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<Result<_, _>>()
        .map_err(|e: std::io::Error| e.to_string())?;
    names.sort();
    Ok(names)
}

#[qpl::native(iread)]
fn size(p: String) -> Result<i64, String> {
    std::fs::metadata(&p)
        .map(|m| m.len() as i64)
        .map_err(|e| e.to_string())
}

#[qpl::native(iread)]
fn mtime(p: String) -> Result<Value, String> {
    let modified = std::fs::metadata(&p)
        .and_then(|m| m.modified())
        .map_err(|e| e.to_string())?;
    let ns = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos() as i64;
    Ok(Value::Timestamp(ns - NS_2000_TO_1970))
}

#[qpl::native(read)]
fn join(a: String, b: String) -> String {
    to_str(&Path::new(&a).join(b))
}

#[qpl::native(read)]
fn name(p: String) -> String {
    Path::new(&p)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[qpl::native(read)]
fn ext(p: String) -> String {
    Path::new(&p)
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[qpl::native(iread)]
fn cwd() -> Result<String, String> {
    std::env::current_dir()
        .map(|p| to_str(&p))
        .map_err(|e| e.to_string())
}

#[qpl::native(write)]
fn rm(p: String) -> Result<(), String> {
    let path = Path::new(&p);
    if path.is_dir() {
        std::fs::remove_dir(path).map_err(|e| e.to_string())
    } else {
        std::fs::remove_file(path).map_err(|e| e.to_string())
    }
}

#[qpl::native(write)]
fn mv(a: String, b: String) -> Result<(), String> {
    std::fs::rename(&a, &b).map_err(|e| e.to_string())
}

#[qpl::native(write)]
fn cp(a: String, b: String) -> Result<(), String> {
    std::fs::copy(&a, &b).map(|_| ()).map_err(|e| e.to_string())
}

pub fn extension() -> Extension {
    Extension::new("std.fs")
        .owner("qpl-std")
        .with::<mkdir>()
        .with::<exists>()
        .with::<isdir>()
        .with::<isfile>()
        .with::<abspath>()
        .with::<parent>()
        .with::<ls>()
        .with::<size>()
        .with::<mtime>()
        .with::<join>()
        .with::<name>()
        .with::<ext>()
        .with::<cwd>()
        .with::<rm>()
        .with::<mv>()
        .with::<cp>()
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

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qpl_std_fs_test_{}_{:?}_{}",
            std::process::id(),
            std::thread::current().id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn writable_vm() -> Vm {
        let mut vm = Vm::new_writable();
        vm.register(super::extension()).expect("register .std.fs");
        vm
    }

    fn q(p: &std::path::Path) -> String {
        p.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }

    #[test]
    fn mkdir_exists_isdir_isfile() {
        let dir = temp_dir();
        let sub = dir.join("sub");
        let mut vm = writable_vm();
        let src = format!(r#".std.fs.mkdir "{}""#, q(&sub));
        run_vm(&src, &mut vm).expect("mkdir");
        assert_eq!(
            expect_scalar(run_vm(&format!(r#".std.fs.isdir "{}""#, q(&sub)), &mut vm).unwrap()),
            Value::Bool(true)
        );
        let file = sub.join("f.txt");
        std::fs::write(&file, b"hi").unwrap();
        assert_eq!(
            expect_scalar(run_vm(&format!(r#".std.fs.isfile "{}""#, q(&file)), &mut vm).unwrap()),
            Value::Bool(true)
        );
        assert_eq!(
            expect_scalar(run_vm(&format!(r#".std.fs.exists "{}""#, q(&file)), &mut vm).unwrap()),
            Value::Bool(true)
        );
        assert_eq!(
            expect_scalar(run_vm(r#".std.fs.exists "no/such/path/xyz""#, &mut vm).unwrap()),
            Value::Bool(false)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mkdir_twice_is_harmless() {
        let dir = temp_dir();
        let mut vm = writable_vm();
        let src = format!(r#".std.fs.mkdir "{}""#, q(&dir));
        run_vm(&src, &mut vm).expect("mkdir once");
        run_vm(&src, &mut vm).expect("mkdir again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_and_ls_and_mtime() {
        let dir = temp_dir();
        let file = dir.join("a.txt");
        std::fs::write(&file, b"hello").unwrap();
        let mut vm = writable_vm();
        assert_eq!(
            expect_scalar(run_vm(&format!(r#".std.fs.size "{}""#, q(&file)), &mut vm).unwrap()),
            Value::Int(5)
        );
        let EvalResult::Scalar(Value::StrVec(s)) =
            run_vm(&format!(r#".std.fs.ls "{}""#, q(&dir)), &mut vm).unwrap()
        else {
            panic!("expected a string list");
        };
        let names: Vec<String> = s
            .str()
            .unwrap()
            .iter()
            .flatten()
            .map(str::to_owned)
            .collect();
        assert_eq!(names, vec!["a.txt".to_string()]);
        let EvalResult::Scalar(Value::Timestamp(_)) =
            run_vm(&format!(r#".std.fs.mtime "{}""#, q(&file)), &mut vm).unwrap()
        else {
            panic!("expected a timestamp");
        };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lexical_functions_never_touch_the_filesystem() {
        let mut vm = writable_vm();
        assert_eq!(
            expect_scalar(run_vm(r#".std.fs.parent "a""#, &mut vm).unwrap()),
            Value::Str(".".into())
        );
        assert_eq!(
            expect_scalar(run_vm(r#".std.fs.parent "/""#, &mut vm).unwrap()),
            Value::Str("/".into())
        );
        assert_eq!(
            expect_scalar(run_vm(r#".std.fs.join["a"; "b"]"#, &mut vm).unwrap()),
            Value::Str("a/b".into())
        );
        assert_eq!(
            expect_scalar(run_vm(r#".std.fs.name "a/b/c.txt""#, &mut vm).unwrap()),
            Value::Str("c.txt".into())
        );
        assert_eq!(
            expect_scalar(run_vm(r#".std.fs.ext "a/b/c.txt""#, &mut vm).unwrap()),
            Value::Str("txt".into())
        );
        assert_eq!(
            expect_scalar(run_vm(r#".std.fs.ext "noext""#, &mut vm).unwrap()),
            Value::Str("".into())
        );
    }

    #[test]
    fn rm_mv_cp() {
        let dir = temp_dir();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        let c = dir.join("c.txt");
        std::fs::write(&a, b"hi").unwrap();
        let mut vm = writable_vm();
        run_vm(&format!(r#".std.fs.cp["{}"; "{}"]"#, q(&a), q(&b)), &mut vm).expect("cp");
        assert!(b.exists());
        run_vm(&format!(r#".std.fs.mv["{}"; "{}"]"#, q(&b), q(&c)), &mut vm).expect("mv");
        assert!(!b.exists() && c.exists());
        run_vm(&format!(r#".std.fs.rm "{}""#, q(&a)), &mut vm).expect("rm a");
        run_vm(&format!(r#".std.fs.rm "{}""#, q(&c)), &mut vm).expect("rm c");
        assert!(!a.exists() && !c.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cwd_is_iread() {
        let mut vm = writable_vm();
        let EvalResult::Scalar(Value::Str(_)) = run_vm(".std.fs.cwd", &mut vm).unwrap() else {
            panic!("expected a string");
        };
    }

    #[test]
    fn writes_are_refused_without_write_permission_and_allowed_with_it() {
        let dir = temp_dir();
        let target = dir.join("x");
        let mut ro = Vm::new();
        ro.register(super::extension()).unwrap();
        let src = format!(r#".std.fs.mkdir "{}""#, q(&target));
        assert!(run_vm(&src, &mut ro).is_err());

        let mut rw = writable_vm();
        run_vm(&src, &mut rw).expect("mkdir under -w");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(feature = "ipc")]
    fn ireads_are_refused_over_a_read_only_handle() {
        use qpl::ipc::HandleMode;

        let dir = temp_dir();
        let mut vm = writable_vm();
        let ok = vm.with_request_permission(HandleMode::Read, |vm| {
            run_vm(&format!(r#".std.fs.exists "{}""#, q(&dir)), vm)
        });
        assert!(
            ok.is_err(),
            "iread should be refused over a read-only handle"
        );

        let ok = vm.with_request_permission(HandleMode::Write, |vm| {
            run_vm(&format!(r#".std.fs.exists "{}""#, q(&dir)), vm)
        });
        assert!(ok.is_ok(), "iread is allowed over a write handle");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
