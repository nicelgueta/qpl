//! `.std.arr`: list functions. `xs` is any list; results keep its kind
//! ([`Value::map_vec`](qpl::ast::Value::map_vec)). Every function is `read`.

use qpl::ast::{Value, VecKind};
use qpl::ext::Extension;
use qpl::ext::polars::prelude::*;

/// `x`, as the `AnyValue` it must equal for a `has`/`find`/`findall` element
/// comparison against a list of `kind`. An error if `x`'s type doesn't
/// match the list's element type.
fn scalar_anyvalue(kind: VecKind, x: &Value) -> Result<AnyValue<'_>, String> {
    use Value::*;
    Ok(match (kind, x) {
        (VecKind::Int, Int(n)) => AnyValue::Int64(*n),
        (VecKind::Float, Float(f)) => AnyValue::Float64(*f),
        (VecKind::Float, Int(n)) => AnyValue::Float64(*n as f64),
        (VecKind::Bool, Bool(b)) => AnyValue::Boolean(*b),
        (VecKind::Str, Str(s)) | (VecKind::Str, Sym(s)) => AnyValue::String(s),
        (VecKind::Sym, Sym(s)) | (VecKind::Sym, Str(s)) => AnyValue::String(s),
        (VecKind::Date, Date(d)) => AnyValue::Int32(*d),
        (VecKind::Month, Month(m)) => AnyValue::Int32(*m),
        (VecKind::Time, Time(t)) => AnyValue::Int64(*t),
        (VecKind::Minute, Minute(m)) => AnyValue::Int32(*m),
        (VecKind::Second, Second(s)) => AnyValue::Int32(*s),
        (VecKind::Timestamp, Timestamp(t)) => AnyValue::Int64(*t),
        (VecKind::Timespan, Timespan(t)) => AnyValue::Int64(*t),
        _ => return Err(format!("{x:?} doesn't match the list's element type")),
    })
}

/// `true` for each element of `xs` equal to `x`.
fn element_mask(xs: &Value, x: &Value) -> Result<Vec<bool>, String> {
    let (kind, s) = xs
        .as_vec()
        .ok_or_else(|| format!("expected a list, got {xs:?}"))?;
    let target = scalar_anyvalue(kind, x)?;
    Ok(s.iter().map(|av| av == target).collect())
}

#[qpl::native(read)]
fn rv(xs: Value) -> Result<Value, String> {
    xs.map_vec(|s| Ok(s.reverse()))
}

#[qpl::native(read)]
fn iasc(xs: Value) -> Result<Vec<i64>, String> {
    let (_, s) = xs
        .as_vec()
        .ok_or_else(|| format!("expected a list, got {xs:?}"))?;
    let idx = s.arg_sort(SortOptions {
        descending: false,
        ..Default::default()
    });
    Ok(idx.iter().map(|o| o.unwrap_or(0) as i64).collect())
}

#[qpl::native(read)]
fn idesc(xs: Value) -> Result<Vec<i64>, String> {
    let (_, s) = xs
        .as_vec()
        .ok_or_else(|| format!("expected a list, got {xs:?}"))?;
    let idx = s.arg_sort(SortOptions {
        descending: true,
        ..Default::default()
    });
    Ok(idx.iter().map(|o| o.unwrap_or(0) as i64).collect())
}

#[qpl::native(read)]
fn has(xs: Value, x: Value) -> Result<bool, String> {
    Ok(element_mask(&xs, &x)?.into_iter().any(|b| b))
}

#[qpl::native(read)]
fn find(xs: Value, x: Value) -> Result<i64, String> {
    Ok(element_mask(&xs, &x)?
        .into_iter()
        .position(|b| b)
        .map(|i| i as i64)
        .unwrap_or(-1))
}

#[qpl::native(read)]
fn findall(xs: Value, x: Value) -> Result<Vec<i64>, String> {
    Ok(element_mask(&xs, &x)?
        .into_iter()
        .enumerate()
        .filter_map(|(i, b)| b.then_some(i as i64))
        .collect())
}

#[qpl::native(read)]
fn dedup(xs: Value) -> Result<Value, String> {
    let (kind, s) = xs
        .as_vec()
        .ok_or_else(|| format!("expected a list, got {xs:?}"))?;
    let mut keep: Vec<u32> = Vec::new();
    let mut prev: Option<AnyValue> = None;
    for (i, av) in s.iter().enumerate() {
        if prev.as_ref() != Some(&av) {
            keep.push(i as u32);
        }
        prev = Some(av);
    }
    let idx = IdxCa::from_vec("".into(), keep);
    let out = s.take(&idx).map_err(|e| e.to_string())?;
    Ok(Value::from_vec(kind, out))
}

#[qpl::native(read)]
fn rotate(xs: Value, n: i64) -> Result<Value, String> {
    let (kind, s) = xs
        .as_vec()
        .ok_or_else(|| format!("expected a list, got {xs:?}"))?;
    let len = s.len() as i64;
    if len == 0 {
        return Ok(Value::from_vec(kind, s.clone()));
    }
    let shift = n.rem_euclid(len);
    let idx: Vec<u32> = (0..len).map(|i| ((i + shift) % len) as u32).collect();
    let idx = IdxCa::from_vec("".into(), idx);
    let out = s.take(&idx).map_err(|e| e.to_string())?;
    Ok(Value::from_vec(kind, out))
}

#[qpl::native(read)]
fn cat(xs: Value, ys: Value) -> Result<Value, String> {
    let (kx, sx) = xs
        .as_vec()
        .ok_or_else(|| format!("expected a list, got {xs:?}"))?;
    let (ky, sy) = ys
        .as_vec()
        .ok_or_else(|| format!("expected a list, got {ys:?}"))?;
    if kx != ky {
        return Err(format!("cannot cat a {kx:?} list with a {ky:?} list"));
    }
    let mut out = sx.clone();
    out.append(sy).map_err(|e| e.to_string())?;
    Ok(Value::from_vec(kx, out))
}

pub fn extension() -> Extension {
    Extension::new("std.arr")
        .owner("qpl-std")
        .with::<rv>()
        .with::<iasc>()
        .with::<idesc>()
        .with::<has>()
        .with::<find>()
        .with::<findall>()
        .with::<dedup>()
        .with::<rotate>()
        .with::<cat>()
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

    fn vm() -> Vm {
        let mut vm = Vm::new();
        vm.register(super::extension()).expect("register .std.arr");
        vm
    }

    fn eval(vm: &mut Vm, src: &str) -> EvalResult {
        run_vm(src, vm).unwrap_or_else(|e| panic!("{src}: {e}"))
    }

    fn int_vec(v: &Value) -> Vec<i64> {
        let Value::IntVec(s) = v else {
            panic!("expected an int list, got {v:?}")
        };
        s.i64().unwrap().iter().map(|o| o.unwrap()).collect()
    }

    #[test]
    fn rv_reverses_and_keeps_kind() {
        let mut vm = vm();
        let EvalResult::Scalar(v) = eval(&mut vm, ".std.arr.rv 1 2 3") else {
            panic!("expected a scalar")
        };
        assert_eq!(int_vec(&v), vec![3, 2, 1]);
    }

    #[test]
    fn rv_keeps_a_date_vec_a_date_vec() {
        let mut vm = vm();
        let EvalResult::Scalar(v) = eval(&mut vm, ".std.arr.rv date$(1 2)") else {
            panic!("expected a scalar")
        };
        assert!(matches!(v, Value::DateVec(_)), "{v:?}");
    }

    #[test]
    fn iasc_idesc_are_sort_permutations() {
        let mut vm = vm();
        let EvalResult::Scalar(v) = eval(&mut vm, ".std.arr.iasc 3 1 2") else {
            panic!("scalar")
        };
        assert_eq!(int_vec(&v), vec![1, 2, 0]);
        let EvalResult::Scalar(v) = eval(&mut vm, ".std.arr.idesc 3 1 2") else {
            panic!("scalar")
        };
        assert_eq!(int_vec(&v), vec![0, 2, 1]);
    }

    #[test]
    fn has_find_findall() {
        let mut vm = vm();
        assert_eq!(
            expect_scalar(eval(&mut vm, ".std.arr.has[1 2 3; 2]")),
            Value::Bool(true)
        );
        assert_eq!(
            expect_scalar(eval(&mut vm, ".std.arr.has[1 2 3; 9]")),
            Value::Bool(false)
        );
        assert_eq!(
            expect_scalar(eval(&mut vm, ".std.arr.find[1 2 3 2; 2]")),
            Value::Int(1)
        );
        assert_eq!(
            expect_scalar(eval(&mut vm, ".std.arr.find[1 2 3; 9]")),
            Value::Int(-1)
        );
        let EvalResult::Scalar(v) = eval(&mut vm, ".std.arr.findall[1 2 3 2; 2]") else {
            panic!("scalar")
        };
        assert_eq!(int_vec(&v), vec![1, 3]);
    }

    #[test]
    fn has_rejects_a_mismatched_type() {
        let mut vm = vm();
        let err = run_vm(r#".std.arr.has[1 2 3; "x"]"#, &mut vm).unwrap_err();
        assert!(err.to_string().contains("doesn't match"), "{err}");
    }

    #[test]
    fn dedup_removes_only_consecutive_repeats() {
        let mut vm = vm();
        let EvalResult::Scalar(v) = eval(&mut vm, ".std.arr.dedup 1 1 2 2 1 3") else {
            panic!("scalar")
        };
        assert_eq!(int_vec(&v), vec![1, 2, 1, 3]);
    }

    #[test]
    fn rotate_left_and_right() {
        let mut vm = vm();
        let EvalResult::Scalar(v) = eval(&mut vm, ".std.arr.rotate[1 2 3 4; 1]") else {
            panic!("scalar")
        };
        assert_eq!(int_vec(&v), vec![2, 3, 4, 1]);
        let EvalResult::Scalar(v) = eval(&mut vm, ".std.arr.rotate[1 2 3 4; -1]") else {
            panic!("scalar")
        };
        assert_eq!(int_vec(&v), vec![4, 1, 2, 3]);
    }

    #[test]
    fn cat_appends_same_kind_lists() {
        let mut vm = vm();
        let EvalResult::Scalar(v) = eval(&mut vm, ".std.arr.cat[1 2; 3 4]") else {
            panic!("scalar")
        };
        assert_eq!(int_vec(&v), vec![1, 2, 3, 4]);
    }

    #[test]
    fn cat_rejects_mismatched_kinds() {
        let mut vm = vm();
        let err = run_vm(r#".std.arr.cat[1 2; "a" "b"]"#, &mut vm).unwrap_err();
        assert!(err.to_string().contains("cannot cat"), "{err}");
    }
}
