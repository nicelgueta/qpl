//! `.std.str`: string functions. `s` is a string, a symbol, or a list of
//! either ([`StrArg`](qpl::ext::StrArg)); a list is processed element by
//! element and gives a list back. Positions and lengths count characters,
//! not bytes. Every function is `read`.

use qpl::ast::Value;
use qpl::errors::QplError;
use qpl::ext::polars::prelude::*;
use qpl::ext::{Extension, StrArg};

use regex::Regex;

/// Apply a `&str -> bool` predicate, scalar in, scalar out; a null element
/// of a list argument is treated as not matching.
fn map_bool(arg: &StrArg, f: impl Fn(&str) -> bool + Copy) -> Value {
    match arg {
        StrArg::One(s) => Value::Bool(f(s)),
        StrArg::Many(s) => {
            let ca = s.str().expect("StrArg::Many backs a String series");
            let out: Vec<bool> = ca.iter().map(|o| o.map(f).unwrap_or(false)).collect();
            Value::BoolVec(Series::new("".into(), out))
        }
    }
}

/// Apply a `&str -> i64` function; a null element of a list argument comes
/// back as `null_default` (unaffected strings have no natural "unset"
/// int, so callers pick the sentinel: `-1` for a search, `0` for a length).
fn map_int(arg: &StrArg, null_default: i64, f: impl Fn(&str) -> i64 + Copy) -> Value {
    match arg {
        StrArg::One(s) => Value::Int(f(s)),
        StrArg::Many(s) => {
            let ca = s.str().expect("StrArg::Many backs a String series");
            let out: Vec<i64> = ca
                .iter()
                .map(|o| o.map(f).unwrap_or(null_default))
                .collect();
            Value::IntVec(Series::new("".into(), out))
        }
    }
}

/// Character index of `p`'s first literal occurrence in `s`, or `-1`.
fn char_find(s: &str, p: &str) -> i64 {
    match s.find(p) {
        Some(byte_idx) => s[..byte_idx].chars().count() as i64,
        None => -1,
    }
}

/// regex match
fn regex_find(s: &StrArg, pattern: &str) -> Result<Value, QplError> {
    Ok(match s {
        StrArg::One(s2) => {
            let re = Regex::new(pattern)
                .map_err(|e| QplError::Runtime(format!("Invalid regex: {}", e.to_string())))?;
            match re.find(s2) {
                Some(m) => Value::Str(m.as_str().to_string()),
                None => Value::Str(String::new()),
            }
        }
        StrArg::Many(sv) => {
            let pat = StringChunked::from_slice(pattern.into(), &[pattern]);
            let res = sv
                .str()?
                .extract(&pat, 0)
                .map_err(|e| QplError::Runtime(format!("Invalid regex: {}", e.to_string())))?;
            Value::StrVec(res.into())
        }
    })
}

/// `s`'s characters `start..end` (exclusive), negative indices counting
/// from the end, clamped to `s`'s length.
fn char_slice(s: &str, start: i64, end: i64) -> String {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;
    let norm = |i: i64| (if i < 0 { n + i } else { i }).clamp(0, n);
    let (a, b) = (norm(start), norm(end));
    let b = b.max(a);
    chars[a as usize..b as usize].iter().collect()
}

/// take n chars from the front or the back of the string
fn _take(s: &str, n: i64) -> String {
    if n < 0 {
        char_slice(s, n, s.chars().count() as i64)
    } else {
        char_slice(s, 0, n)
    }
}

#[qpl::native(read)]
fn startswith(s: StrArg, p: String) -> Value {
    map_bool(&s, |s| s.starts_with(p.as_str()))
}

#[qpl::native(read)]
fn endswith(s: StrArg, p: String) -> Value {
    map_bool(&s, |s| s.ends_with(p.as_str()))
}

#[qpl::native(read)]
fn contains(s: StrArg, p: String) -> Value {
    map_bool(&s, |s| s.contains(p.as_str()))
}

#[qpl::native(read)]
fn slice(s: StrArg, start: i64, end: i64) -> Value {
    s.map(|s| char_slice(s, start, end))
}

#[qpl::native(read)]
fn take(s: StrArg, n: i64) -> Value {
    s.map(|s| _take(s, n))
}

#[qpl::native(read)]
fn rv(s: StrArg) -> Value {
    s.map(|s| s.chars().rev().collect())
}

#[qpl::native(read)]
fn l(s: StrArg) -> Value {
    s.map(str::to_lowercase)
}

#[qpl::native(read)]
fn u(s: StrArg) -> Value {
    s.map(str::to_uppercase)
}

#[qpl::native(read)]
fn len(s: StrArg) -> Value {
    map_int(&s, 0, |s| s.chars().count() as i64)
}

#[qpl::native(read)]
fn trim(s: StrArg) -> Value {
    s.map(|s: &str| s.trim().to_string())
}

#[qpl::native(read)]
fn split(s: String, sep: String) -> Vec<String> {
    s.split(sep.as_str()).map(str::to_owned).collect()
}

#[qpl::native(read)]
fn join(xs: Vec<String>, sep: String) -> String {
    xs.join(&sep)
}

#[qpl::native(read)]
fn replace(s: StrArg, a: String, b: String) -> Value {
    s.map(|s| s.replace(a.as_str(), b.as_str()))
}

#[qpl::native(read)]
fn find(s: StrArg, p: String) -> Value {
    map_int(&s, -1, |s| char_find(s, p.as_str()))
}

#[qpl::native(read)]
fn rfind(s: StrArg, p: String) -> Result<Value, QplError> {
    regex_find(&s, &p)
}

pub fn extension() -> Extension {
    Extension::new("std.str")
        .owner("qpl-std")
        .with::<startswith>()
        .with::<endswith>()
        .with::<contains>()
        .with::<slice>()
        .with::<take>()
        .with::<rv>()
        .with::<l>()
        .with::<u>()
        .with::<len>()
        .with::<trim>()
        .with::<split>()
        .with::<join>()
        .with::<replace>()
        .with::<find>()
        .with::<rfind>()
}

#[cfg(test)]
mod tests {
    use qpl::ast::Value;
    use qpl::vm::{EvalResult, Vm, run_vm};

    fn vm() -> Vm {
        let mut vm = Vm::new();
        vm.register(super::extension()).expect("register .std.str");
        vm
    }

    fn eval(vm: &mut Vm, src: &str) -> EvalResult {
        run_vm(src, vm).unwrap_or_else(|e| panic!("{src}: {e}"))
    }

    fn scalar(vm: &mut Vm, src: &str) -> Value {
        match eval(vm, src) {
            EvalResult::Scalar(v) => v,
            other => panic!("{src}: expected a scalar, got {other:?}"),
        }
    }

    #[test]
    fn startswith_endswith_contains_scalar() {
        let mut vm = vm();
        assert_eq!(
            scalar(&mut vm, r#".std.str.startswith["hello"; "he"]"#),
            Value::Bool(true)
        );
        assert_eq!(
            scalar(&mut vm, r#".std.str.endswith["hello"; "lo"]"#),
            Value::Bool(true)
        );
        assert_eq!(
            scalar(&mut vm, r#".std.str.contains["hello"; "ell"]"#),
            Value::Bool(true)
        );
        assert_eq!(
            scalar(&mut vm, r#".std.str.contains["hello"; "xyz"]"#),
            Value::Bool(false)
        );
    }

    #[test]
    fn startswith_list() {
        let mut vm = vm();
        let EvalResult::Scalar(Value::BoolVec(s)) =
            eval(&mut vm, r#".std.str.startswith[("ab" "ba");"a"]"#)
        else {
            panic!("expected a bool list");
        };
        let v: Vec<Option<bool>> = s.bool().unwrap().iter().collect();
        assert_eq!(v, vec![Some(true), Some(false)]);
    }

    #[test]
    fn slice_clamps_and_handles_negatives() {
        let mut vm = vm();
        assert_eq!(
            scalar(&mut vm, r#".std.str.slice["hello"; 1; 3]"#),
            Value::Str("el".into())
        );
        assert_eq!(
            scalar(&mut vm, r#".std.str.slice["hello"; -3; -1]"#),
            Value::Str("ll".into())
        );
        assert_eq!(
            scalar(&mut vm, r#".std.str.slice["hi"; 0; 100]"#),
            Value::Str("hi".into())
        );
    }

    #[test]
    fn rv_l_u_trim_are_unicode_aware() {
        let mut vm = vm();
        assert_eq!(
            scalar(&mut vm, r#".std.str.rv "abc""#),
            Value::Str("cba".into())
        );
        assert_eq!(
            scalar(&mut vm, r#".std.str.l "ABC""#),
            Value::Str("abc".into())
        );
        assert_eq!(
            scalar(&mut vm, r#".std.str.u "abc""#),
            Value::Str("ABC".into())
        );
        assert_eq!(
            scalar(&mut vm, r#".std.str.trim "  hi  ""#),
            Value::Str("hi".into())
        );
    }

    #[test]
    fn len_counts_characters() {
        let mut vm = vm();
        assert_eq!(scalar(&mut vm, r#".std.str.len "hello""#), Value::Int(5));
    }

    #[test]
    fn split_rejects_a_list() {
        let mut vm = vm();
        let err = run_vm(r#".std.str.split[("a" "b"); ","]"#, &mut vm).unwrap_err();
        assert!(err.to_string().contains("expected a string"), "{err}");
    }

    #[test]
    fn split_and_join_round_trip() {
        let mut vm = vm();
        assert_eq!(
            scalar(
                &mut vm,
                r#".std.str.join[.std.str.split["a,b,c"; ","]; "-"]"#
            ),
            Value::Str("a-b-c".into())
        );
    }

    #[test]
    fn replace_is_literal() {
        let mut vm = vm();
        assert_eq!(
            scalar(&mut vm, r#".std.str.replace["a.b.c"; "."; "-"]"#),
            Value::Str("a-b-c".into())
        );
    }

    #[test]
    fn find_returns_char_index_or_minus_one() {
        let mut vm = vm();
        assert_eq!(
            scalar(&mut vm, r#".std.str.find["hello"; "ll"]"#),
            Value::Int(2)
        );
        assert_eq!(
            scalar(&mut vm, r#".std.str.find["hello"; "xyz"]"#),
            Value::Int(-1)
        );
    }

    #[test]
    fn take_front_and_back() {
        let mut vm = vm();
        assert_eq!(
            scalar(&mut vm, r#".std.str.take["hello"; 2]"#),
            Value::Str("he".into())
        );
        assert_eq!(
            scalar(&mut vm, r#".std.str.take["hello"; -2]"#),
            Value::Str("lo".into())
        );
        assert_eq!(
            scalar(&mut vm, r#".std.str.take["hi"; 100]"#),
            Value::Str("hi".into())
        );
    }

    #[test]
    fn take_list() {
        let mut vm = vm();
        let EvalResult::Scalar(Value::StrVec(s)) =
            eval(&mut vm, r#".std.str.take[("ab" "cde");1]"#)
        else {
            panic!("expected a string list");
        };
        let v: Vec<Option<&str>> = s.str().unwrap().iter().collect();
        assert_eq!(v, vec![Some("a"), Some("c")]);
    }

    #[test]
    fn rfind_matches_a_regex_scalar() {
        let mut vm = vm();
        assert_eq!(
            scalar(&mut vm, r#".std.str.rfind["room 12b"; "\\d+"]"#),
            Value::Str("12".into())
        );
        assert_eq!(
            scalar(&mut vm, r#".std.str.rfind["no digits here"; "\\d+"]"#),
            Value::Str("".into())
        );
    }

    #[test]
    fn rfind_matches_a_regex_list() {
        let mut vm = vm();
        let EvalResult::Scalar(Value::StrVec(s)) = eval(
            &mut vm,
            r#".std.str.rfind[("room 12b" "no digits here");"\\d+"]"#,
        ) else {
            panic!("expected a string list");
        };
        let v: Vec<Option<&str>> = s.str().unwrap().iter().collect();
        assert_eq!(v, vec![Some("12"), None]);
    }

    #[test]
    fn rfind_rejects_an_invalid_regex() {
        let mut vm = vm();
        let err = run_vm(r#".std.str.rfind["abc"; "("]"#, &mut vm).unwrap_err();
        assert!(err.to_string().contains("Invalid regex"), "{err}");
    }

    #[test]
    fn a_symbol_is_accepted_like_a_string() {
        let mut vm = vm();
        assert_eq!(
            scalar(&mut vm, r#".std.str.u[`abc]"#),
            Value::Str("ABC".into())
        );
    }
}
