//! Lossless binary codec for [`crate::ast::Value`], shared by the `ipc`
//! feature's wire format and by [`crate::program::Program`]'s `.qplc`
//! serialisation. Ungated (unlike `ipc.rs`)
//! since a `.qplc` file has nothing to do with sockets.
//!
//! [`encode_value`]/[`decode_value`] round-trip every scalar and vector
//! `Value` variant exactly, including nulls inside a vector (a validity flag
//! precedes each element). `Table`, `Lazy`, `Closure`,
//! `Handle` and `Future` still have no encoding here — [`encode_value`]
//! returns `Err` for them; `program.rs`'s operand encoder propagates that
//! error (none of those ever legitimately reaches an `Operand::Value`), while
//! `ipc.rs` catches it and falls back to encoding the same
//! `"<unrepresentable>"` string it always has, preserving its existing wire
//! behaviour for connection/future handles and closures.
//!
//! The one-byte [`ValueTag`] prefixing every encoded value is part of both
//! the IPC wire format and the on-disk `.qplc` format — the tag values below
//! must stay stable, and adding a `Value` variant means adding a tag here
//! (an exhaustive match keeps the compiler honest about it).

use crate::ast::{self, Value};
use crate::errors::QplError;
use polars::prelude::*;

pub(crate) fn rt<E: std::fmt::Display>(e: E) -> QplError {
    QplError::Runtime(e.to_string())
}

// ---------------------------------------------------------------------------
// low-level primitives shared by every tagged encoding in this module
// ---------------------------------------------------------------------------

pub(crate) fn push_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}
pub(crate) fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub(crate) fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub(crate) fn push_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub(crate) fn push_i64(out: &mut Vec<u8>, v: i64) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub(crate) fn push_f64(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&v.to_le_bytes());
}
pub(crate) fn push_str(out: &mut Vec<u8>, s: &str) {
    push_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

/// A cursor over an in-memory byte buffer — every `.qplc` field and every
/// IPC scalar payload is read through this. Every accessor rejects a
/// truncated read with a plain [`QplError::Runtime`] rather than panicking,
/// which is what lets `Program::from_bytes` turn a corrupted/truncated file
/// into a clean error instead of a crash.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// Bytes left unread. Used to cap a `Vec::with_capacity` sized from an
    /// untrusted length prefix — every element takes at least one byte, so a
    /// capacity request larger than this can only come from a corrupted or
    /// truncated file, and pre-allocating it verbatim risks an OOM abort
    /// before the length even gets a chance to fail a real read.
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8], QplError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| rt("corrupt bytecode: length overflow"))?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or_else(|| rt("corrupt bytecode: truncated data"))?;
        self.pos = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, QplError> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, QplError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> Result<u32, QplError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn i32(&mut self) -> Result<i32, QplError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn i64(&mut self) -> Result<i64, QplError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn f64(&mut self) -> Result<f64, QplError> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn bytes(&mut self) -> Result<Vec<u8>, QplError> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }
    pub fn string(&mut self) -> Result<String, QplError> {
        let len = self.u32()? as usize;
        Ok(String::from_utf8_lossy(self.take(len)?).into_owned())
    }
}

/// The one-byte tag identifying which [`Value`] variant follows. Explicit
/// discriminants so the encoding is stable across builds; kept as an enum
/// (rather than loose `u8` constants) so adding a `Value` variant forces a
/// decision here too — [`encode_value`]'s match is exhaustive over `Value`,
/// so the compiler catches a forgotten codec update the moment a new
/// scalar/vector variant is added.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ValueTag {
    Int = 0,
    Float = 1,
    Str = 2,
    Sym = 3,
    Bool = 4,
    Date = 5,
    Month = 6,
    Time = 7,
    Minute = 8,
    Second = 9,
    Timestamp = 10,
    Timespan = 11,
    IntVec = 12,
    FloatVec = 13,
    SymVec = 14,
    StrVec = 15,
    BoolVec = 16,
    DateVec = 17,
    MonthVec = 18,
    TimeVec = 19,
    MinuteVec = 20,
    SecondVec = 21,
    TimestampVec = 22,
    TimespanVec = 23,
}

impl TryFrom<u8> for ValueTag {
    type Error = QplError;
    fn try_from(b: u8) -> Result<Self, QplError> {
        use ValueTag::*;
        Ok(match b {
            0 => Int,
            1 => Float,
            2 => Str,
            3 => Sym,
            4 => Bool,
            5 => Date,
            6 => Month,
            7 => Time,
            8 => Minute,
            9 => Second,
            10 => Timestamp,
            11 => Timespan,
            12 => IntVec,
            13 => FloatVec,
            14 => SymVec,
            15 => StrVec,
            16 => BoolVec,
            17 => DateVec,
            18 => MonthVec,
            19 => TimeVec,
            20 => MinuteVec,
            21 => SecondVec,
            22 => TimestampVec,
            23 => TimespanVec,
            other => {
                return Err(rt(format!("corrupt bytecode: unknown value tag {other}")));
            }
        })
    }
}

/// Encode one `Option<i64>`/`Option<i32>`/... element of a vector: a 1-byte
/// validity flag, followed by the value's raw bytes iff it's `Some`. Used for
/// every fixed-width vector kind (`Int`/`Date`/`Month`/`Time`/`Minute`/
/// `Second`/`Timestamp`/`Timespan`); `Str`/`Sym`/`Bool` have their own
/// analogous inline handling below since their payloads aren't all the same width.
macro_rules! encode_nullable_vec {
    ($out:expr, $chunked:expr, $push:expr) => {{
        let ca = $chunked;
        push_u32($out, ca.len() as u32);
        for opt in ca.iter() {
            match opt {
                Some(v) => {
                    push_u8($out, 1);
                    $push($out, v);
                }
                None => push_u8($out, 0),
            }
        }
    }};
}

macro_rules! decode_nullable_vec {
    ($r:expr, $read:expr) => {{
        let n = $r.u32()? as usize;
        let mut v = Vec::with_capacity(n.min($r.remaining()));
        for _ in 0..n {
            let valid = $r.u8()? != 0;
            v.push(if valid { Some($read($r)?) } else { None });
        }
        v
    }};
}

/// Encode `v`, tagged with its [`ValueTag`] — see [`decode_value`]. Returns
/// `Err` for `Table`/`Lazy`/`Closure`/`Handle`/`Future`, none of which has an
/// on-disk or wire encoding (see the module doc); callers that can tolerate a
/// lossy placeholder (the IPC scalar wire path) catch this and substitute one
/// themselves rather than this function ever writing one silently.
pub fn encode_value(v: &Value, out: &mut Vec<u8>) -> Result<(), QplError> {
    use Value::*;
    match v {
        Int(n) => {
            push_u8(out, ValueTag::Int as u8);
            push_i64(out, *n);
        }
        Float(n) => {
            push_u8(out, ValueTag::Float as u8);
            push_f64(out, *n);
        }
        Str(s) => {
            push_u8(out, ValueTag::Str as u8);
            push_str(out, s);
        }
        Sym(s) => {
            push_u8(out, ValueTag::Sym as u8);
            push_str(out, s);
        }
        Bool(b) => {
            push_u8(out, ValueTag::Bool as u8);
            push_u8(out, *b as u8);
        }
        Date(n) => {
            push_u8(out, ValueTag::Date as u8);
            push_i32(out, *n);
        }
        Month(n) => {
            push_u8(out, ValueTag::Month as u8);
            push_i32(out, *n);
        }
        Time(n) => {
            push_u8(out, ValueTag::Time as u8);
            push_i64(out, *n);
        }
        Minute(n) => {
            push_u8(out, ValueTag::Minute as u8);
            push_i32(out, *n);
        }
        Second(n) => {
            push_u8(out, ValueTag::Second as u8);
            push_i32(out, *n);
        }
        Timestamp(n) => {
            push_u8(out, ValueTag::Timestamp as u8);
            push_i64(out, *n);
        }
        Timespan(n) => {
            push_u8(out, ValueTag::Timespan as u8);
            push_i64(out, *n);
        }
        IntVec(s) => {
            push_u8(out, ValueTag::IntVec as u8);
            encode_nullable_vec!(out, s.i64().map_err(rt)?, push_i64);
        }
        FloatVec(s) => {
            push_u8(out, ValueTag::FloatVec as u8);
            encode_nullable_vec!(out, s.f64().map_err(rt)?, push_f64);
        }
        SymVec(s) => {
            push_u8(out, ValueTag::SymVec as u8);
            let ca = s.str().map_err(rt)?;
            push_u32(out, ca.len() as u32);
            for opt in ca.iter() {
                match opt {
                    Some(v) => {
                        push_u8(out, 1);
                        push_str(out, v);
                    }
                    None => push_u8(out, 0),
                }
            }
        }
        StrVec(s) => {
            push_u8(out, ValueTag::StrVec as u8);
            let ca = s.str().map_err(rt)?;
            push_u32(out, ca.len() as u32);
            for opt in ca.iter() {
                match opt {
                    Some(v) => {
                        push_u8(out, 1);
                        push_str(out, v);
                    }
                    None => push_u8(out, 0),
                }
            }
        }
        BoolVec(s) => {
            push_u8(out, ValueTag::BoolVec as u8);
            let ca = s.bool().map_err(rt)?;
            push_u32(out, ca.len() as u32);
            for opt in ca.iter() {
                match opt {
                    Some(v) => {
                        push_u8(out, 1);
                        push_u8(out, v as u8);
                    }
                    None => push_u8(out, 0),
                }
            }
        }
        DateVec(s) => {
            push_u8(out, ValueTag::DateVec as u8);
            encode_nullable_vec!(out, s.i32().map_err(rt)?, push_i32);
        }
        MonthVec(s) => {
            push_u8(out, ValueTag::MonthVec as u8);
            encode_nullable_vec!(out, s.i32().map_err(rt)?, push_i32);
        }
        TimeVec(s) => {
            push_u8(out, ValueTag::TimeVec as u8);
            encode_nullable_vec!(out, s.i64().map_err(rt)?, push_i64);
        }
        MinuteVec(s) => {
            push_u8(out, ValueTag::MinuteVec as u8);
            encode_nullable_vec!(out, s.i32().map_err(rt)?, push_i32);
        }
        SecondVec(s) => {
            push_u8(out, ValueTag::SecondVec as u8);
            encode_nullable_vec!(out, s.i32().map_err(rt)?, push_i32);
        }
        TimestampVec(s) => {
            push_u8(out, ValueTag::TimestampVec as u8);
            encode_nullable_vec!(out, s.i64().map_err(rt)?, push_i64);
        }
        TimespanVec(s) => {
            push_u8(out, ValueTag::TimespanVec as u8);
            encode_nullable_vec!(out, s.i64().map_err(rt)?, push_i64);
        }
        Handle(_) | Future(_) | Closure(_) | Table(_) | Lazy(_) => {
            return Err(rt(format!(
                "cannot serialise a {} value",
                match v {
                    Handle(_) => "handle",
                    Future(_) => "future",
                    Closure(_) => "closure",
                    Table(_) => "table",
                    Lazy(_) => "lazy",
                    _ => unreachable!(),
                }
            )));
        }
    }
    Ok(())
}

/// Inverse of [`encode_value`].
pub fn decode_value(r: &mut Reader) -> Result<Value, QplError> {
    use ValueTag::*;
    Ok(match ValueTag::try_from(r.u8()?)? {
        Int => Value::Int(r.i64()?),
        Float => Value::Float(r.f64()?),
        Str => Value::Str(r.string()?),
        Sym => Value::Sym(r.string()?),
        Bool => Value::Bool(r.u8()? != 0),
        Date => Value::Date(r.i32()?),
        Month => Value::Month(r.i32()?),
        Time => Value::Time(r.i64()?),
        Minute => Value::Minute(r.i32()?),
        Second => Value::Second(r.i32()?),
        Timestamp => Value::Timestamp(r.i64()?),
        Timespan => Value::Timespan(r.i64()?),
        IntVec => {
            let v: Vec<Option<i64>> = decode_nullable_vec!(r, Reader::i64);
            ast::Value::IntVec(Series::new("".into(), v))
        }
        FloatVec => {
            let v: Vec<Option<f64>> = decode_nullable_vec!(r, Reader::f64);
            ast::Value::FloatVec(Series::new("".into(), v))
        }
        SymVec => {
            let n = r.u32()? as usize;
            let mut v: Vec<Option<String>> = Vec::with_capacity(n.min(r.remaining()));
            for _ in 0..n {
                v.push(if r.u8()? != 0 {
                    Some(r.string()?)
                } else {
                    None
                });
            }
            ast::Value::SymVec(Series::new("".into(), v))
        }
        StrVec => {
            let n = r.u32()? as usize;
            let mut v: Vec<Option<String>> = Vec::with_capacity(n.min(r.remaining()));
            for _ in 0..n {
                v.push(if r.u8()? != 0 {
                    Some(r.string()?)
                } else {
                    None
                });
            }
            ast::Value::StrVec(Series::new("".into(), v))
        }
        BoolVec => {
            let n = r.u32()? as usize;
            let mut v: Vec<Option<bool>> = Vec::with_capacity(n.min(r.remaining()));
            for _ in 0..n {
                v.push(if r.u8()? != 0 {
                    Some(r.u8()? != 0)
                } else {
                    None
                });
            }
            ast::Value::BoolVec(Series::new("".into(), v))
        }
        DateVec => {
            let v: Vec<Option<i32>> = decode_nullable_vec!(r, Reader::i32);
            ast::Value::DateVec(Series::new("".into(), v))
        }
        MonthVec => {
            let v: Vec<Option<i32>> = decode_nullable_vec!(r, Reader::i32);
            ast::Value::MonthVec(Series::new("".into(), v))
        }
        TimeVec => {
            let v: Vec<Option<i64>> = decode_nullable_vec!(r, Reader::i64);
            ast::Value::TimeVec(Series::new("".into(), v))
        }
        MinuteVec => {
            let v: Vec<Option<i32>> = decode_nullable_vec!(r, Reader::i32);
            ast::Value::MinuteVec(Series::new("".into(), v))
        }
        SecondVec => {
            let v: Vec<Option<i32>> = decode_nullable_vec!(r, Reader::i32);
            ast::Value::SecondVec(Series::new("".into(), v))
        }
        TimestampVec => {
            let v: Vec<Option<i64>> = decode_nullable_vec!(r, Reader::i64);
            ast::Value::TimestampVec(Series::new("".into(), v))
        }
        TimespanVec => {
            let v: Vec<Option<i64>> = decode_nullable_vec!(r, Reader::i64);
            ast::Value::TimespanVec(Series::new("".into(), v))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: Value) -> Value {
        let mut buf = Vec::new();
        encode_value(&v, &mut buf).expect("encode");
        let mut r = Reader::new(&buf);
        let out = decode_value(&mut r).expect("decode");
        assert!(r.is_empty(), "trailing bytes after decoding a value");
        out
    }

    #[test]
    fn scalars_round_trip() {
        let cases = vec![
            Value::Int(-42),
            Value::Float(3.5),
            Value::Str("hello".into()),
            Value::Sym("sym".into()),
            Value::Bool(true),
            Value::Bool(false),
            Value::Date(123),
            Value::Month(-7),
            Value::Time(456),
            Value::Minute(90),
            Value::Second(3600),
            Value::Timestamp(789),
            Value::Timespan(-1000),
        ];
        for v in cases {
            assert_eq!(roundtrip(v.clone()), v);
        }
    }

    #[test]
    fn plain_vectors_round_trip() {
        let cases = vec![
            ast::int_vec(vec![1, 2, 3]),
            ast::float_vec(vec![1.5, 2.5]),
            ast::sym_vec(vec!["a".into(), "b".into()]),
            ast::str_vec(vec!["x".into(), "y".into()]),
            ast::bool_vec(vec![true, false, true]),
            ast::date_vec(vec![1, 2]),
            ast::month_vec(vec![1, 2]),
            ast::time_vec(vec![1, 2]),
            ast::minute_vec(vec![1, 2]),
            ast::second_vec(vec![1, 2]),
            ast::timestamp_vec(vec![1, 2]),
            ast::timespan_vec(vec![1, 2]),
        ];
        for v in cases {
            assert_eq!(roundtrip(v.clone()), v);
        }
    }

    #[test]
    fn empty_vectors_and_strings_round_trip() {
        assert_eq!(roundtrip(Value::Str("".into())), Value::Str("".into()));
        assert_eq!(roundtrip(ast::int_vec(vec![])), ast::int_vec(vec![]));
    }

    /// Nulls inside every vector kind
    /// survive a round trip.
    #[test]
    fn nulls_in_every_vector_kind_round_trip() {
        macro_rules! check {
            ($ctor:expr, $ty:ty) => {{
                let s = Series::new(
                    "".into(),
                    Vec::<Option<$ty>>::from([None, Some(Default::default())]),
                );
                let v = $ctor(s);
                let got = roundtrip(v.clone());
                assert_eq!(got, v);
            }};
        }
        check!(Value::IntVec, i64);
        check!(Value::FloatVec, f64);
        check!(Value::BoolVec, bool);
        check!(Value::DateVec, i32);
        check!(Value::MonthVec, i32);
        check!(Value::TimeVec, i64);
        check!(Value::MinuteVec, i32);
        check!(Value::SecondVec, i32);
        check!(Value::TimestampVec, i64);
        check!(Value::TimespanVec, i64);

        let sym = Series::new(
            "".into(),
            Vec::<Option<String>>::from([None, Some("a".to_string())]),
        );
        assert_eq!(roundtrip(Value::SymVec(sym.clone())), Value::SymVec(sym));
        let str_ = Series::new(
            "".into(),
            Vec::<Option<String>>::from([Some("b".to_string()), None]),
        );
        assert_eq!(roundtrip(Value::StrVec(str_.clone())), Value::StrVec(str_));
    }

    #[test]
    fn unrepresentable_variants_error_instead_of_writing_garbage() {
        let mut out = Vec::new();
        assert!(encode_value(&Value::Handle(1), &mut out).is_err());
        assert!(out.is_empty());
        assert!(encode_value(&Value::Future(1), &mut out).is_err());
        assert!(encode_value(&Value::Table(DataFrame::empty()), &mut out).is_err());
    }

    #[test]
    fn decode_rejects_unknown_tag() {
        let mut r = Reader::new(&[250]);
        assert!(decode_value(&mut r).is_err());
    }

    #[test]
    fn decode_rejects_truncated_input() {
        // A `Str` tag with no length/payload following it.
        let mut r = Reader::new(&[ValueTag::Str as u8]);
        assert!(decode_value(&mut r).is_err());
    }
}
