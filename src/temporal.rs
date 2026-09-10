//! kdb+/q-style temporal scalars: parsing literals, formatting them back, and
//! the `.qpl.d` / `.qpl.t` / `.qpl.p` / `.qpl.n` "now" functions.
//!
//! Everything calendar-related lives here so the lexer / vm / repl diffs stay
//! small. No `chrono` dependency — dates are proleptic-Gregorian day counts
//! computed with Howard Hinnant's `civil_from_days` / `days_from_civil`.
//!
//! Each [`ast::Value`] temporal variant carries the *kdb* integer offset:
//! `Date` = days since 2000.01.01, `Timestamp` = ns since 2000.01.01,
//! `Time` = ns since midnight, `Timespan` = ns, `Month` = months since 2000.01,
//! `Minute` / `Second` = minutes / seconds since midnight. Conversion to the
//! Polars epoch (1970) happens once, in `vm::ast_val_to_expr`.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::ast::Value;
use crate::errors::QplError;

/// Days from 1970-01-01 to 2000-01-01.
pub const DAYS_2000_TO_1970: i32 = 10_957;
/// Nanoseconds from 1970-01-01 to 2000-01-01.
pub const NS_2000_TO_1970: i64 = 946_684_800_000_000_000;

const NS_PER_DAY: i64 = 86_400_000_000_000;
const NS_PER_SEC: i64 = 1_000_000_000;

// ── calendar math (Hinnant, epoch 1970-01-01) ──────────────────────────────

/// Days since 1970-01-01 for a proleptic-Gregorian date.
pub fn days_from_civil(y: i32, m: u32, d: u32) -> i32 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64; // Mar=0 .. Feb=11
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    (era as i64 * 146_097 + doe - 719_468) as i32
}

/// Proleptic-Gregorian `(year, month, day)` for a day count since 1970-01-01.
pub fn civil_from_days(z: i32) -> (i32, u32, u32) {
    let z = z as i64 + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    ((y + i64::from(m <= 2)) as i32, m, d)
}

// ── parsing ───────────────────────────────────────────────────────────────

/// Parse a temporal literal (`2024.03.15`, `12:30`, `0D12:30:00.5`, …) into the
/// matching [`Value`]. Shared by the lexer and by `"<code>"$"…"` string casts.
/// Returns `None` if `s` is not a recognised temporal shape.
pub fn parse_temporal(s: &str) -> Option<Value> {
    let s = s.trim();

    // date / timespan with a `D` separator: `<date>D<tod>` or `<days>D<tod>`
    if let Some((left, right)) = split_once_upper_d(s) {
        let tod = parse_tod_ns(right)?;
        if let Some(days_1970) = parse_ymd(left) {
            let days = (days_1970 - DAYS_2000_TO_1970) as i64;
            return Some(Value::Timestamp(days * NS_PER_DAY + tod));
        }
        let days: i64 = if left.is_empty() { 0 } else { left.parse().ok()? };
        return Some(Value::Timespan(days * NS_PER_DAY + tod));
    }

    // month: `YYYY.MMm`
    if let Some(head) = s.strip_suffix('m') {
        let (y, m) = head.split_once('.')?;
        let (y, m): (i32, i32) = (y.parse().ok()?, m.parse().ok()?);
        if !(1..=12).contains(&m) {
            return None;
        }
        return Some(Value::Month((y - 2000) * 12 + (m - 1)));
    }

    // date: `YYYY.MM.DD`
    if let Some(days) = parse_ymd(s) {
        return Some(Value::Date(days - DAYS_2000_TO_1970));
    }

    // time of day: `HH:MM` (minute) / `HH:MM:SS` (second) / `HH:MM:SS.fff` (time)
    if s.contains(':') {
        let colons = s.bytes().filter(|&b| b == b':').count();
        let has_frac = s.contains('.');
        let ns = parse_tod_ns(s)?;
        return Some(match (colons, has_frac) {
            (1, _) => Value::Minute((ns / (60 * NS_PER_SEC)) as i32),
            (2, false) => Value::Second((ns / NS_PER_SEC) as i32),
            (2, true) => Value::Time(ns),
            _ => return None,
        });
    }

    None
}

/// `YYYY.MM.DD` → days since 1970-01-01 (`None` if not that exact shape).
fn parse_ymd(s: &str) -> Option<i32> {
    let mut it = s.split('.');
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d): (i32, u32, u32) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    // reject impossible dates (`2024.02.30`) — `days_from_civil` would otherwise
    // silently roll them into the next month
    if civil_from_days(days) != (y, m, d) {
        return None;
    }
    Some(days)
}

/// `HH:MM` / `HH:MM:SS` / `HH:MM:SS.frac` → nanoseconds since midnight.
fn parse_tod_ns(s: &str) -> Option<i64> {
    let (time_part, frac_part) = match s.split_once('.') {
        Some((t, f)) => (t, Some(f)),
        None => (s, None),
    };
    let mut it = time_part.split(':');
    let h: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let sec: i64 = match it.next() {
        Some(v) => v.parse().ok()?,
        None => 0,
    };
    if it.next().is_some() || !(0..24).contains(&h) || !(0..60).contains(&m) || !(0..60).contains(&sec) {
        return None;
    }
    let mut ns = ((h * 60 + m) * 60 + sec) * NS_PER_SEC;
    if let Some(f) = frac_part {
        if f.is_empty() || f.len() > 9 || !f.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut digits = f.to_string();
        while digits.len() < 9 {
            digits.push('0');
        }
        ns += digits.parse::<i64>().ok()?;
    }
    Some(ns)
}

/// Split on the first uppercase `D` that is not the last char. Used to tell a
/// `<date>D<tod>` / `<days>D<tod>` literal apart from a bare date.
fn split_once_upper_d(s: &str) -> Option<(&str, &str)> {
    let idx = s.find('D')?;
    if idx + 1 == s.len() {
        return None;
    }
    Some((&s[..idx], &s[idx + 1..]))
}

// ── formatting ────────────────────────────────────────────────────────────

/// kdb-style rendering of a temporal scalar (`2024.03.15`,
/// `2024.03.15D12:30:00.000000000`, …). `None` for a non-temporal value.
pub fn format_temporal(v: &Value) -> Option<String> {
    Some(match *v {
        Value::Date(d) => {
            let (y, m, day) = civil_from_days(d + DAYS_2000_TO_1970);
            format!("{y:04}.{m:02}.{day:02}")
        }
        Value::Month(mo) => {
            let y = 2000 + mo.div_euclid(12);
            let m = mo.rem_euclid(12) + 1;
            format!("{y:04}.{m:02}m")
        }
        Value::Time(ns) => fmt_tod(ns, 3),
        Value::Minute(m) => {
            let (h, m) = (m.div_euclid(60), m.rem_euclid(60));
            format!("{h:02}:{m:02}")
        }
        Value::Second(s) => {
            let (h, rem) = (s.div_euclid(3600), s.rem_euclid(3600));
            format!("{:02}:{:02}:{:02}", h, rem / 60, rem % 60)
        }
        Value::Timestamp(ns) => {
            let days = ns.div_euclid(NS_PER_DAY);
            let (y, m, day) = civil_from_days(days as i32 + DAYS_2000_TO_1970);
            format!("{y:04}.{m:02}.{day:02}D{}", fmt_tod(ns.rem_euclid(NS_PER_DAY), 9))
        }
        Value::Timespan(ns) => {
            let sign = if ns < 0 { "-" } else { "" };
            let ns = ns.unsigned_abs() as i64;
            format!("{sign}{}D{}", ns / NS_PER_DAY, fmt_tod(ns % NS_PER_DAY, 9))
        }
        _ => return None,
    })
}

/// `HH:MM:SS.frac` for a nanosecond count, `frac_digits` wide. Handles a
/// negative or past-24h `ns` (a `time` can leave `[0, 24h)` through arithmetic):
/// the sign is hoisted out and the hour field is allowed to exceed `99`.
fn fmt_tod(ns: i64, frac_digits: usize) -> String {
    let sign = if ns < 0 { "-" } else { "" };
    let ns = ns.unsigned_abs();
    let ns_per_sec = NS_PER_SEC as u64;
    let secs = ns / ns_per_sec;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    let frac = (ns % ns_per_sec) / 10u64.pow((9 - frac_digits) as u32);
    format!("{sign}{h:02}:{m:02}:{s:02}.{frac:0width$}", width = frac_digits)
}

// ── `.qpl.*` now-functions ────────────────────────────────────────────────

/// Evaluate `.qpl.d` / `.qpl.t` / `.qpl.p` / `.qpl.n`. Times are **UTC** —
/// there is no timezone database without an extra dependency.
pub fn now_value(func: &str) -> Result<Value, QplError> {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| QplError::Runtime(format!("system clock before 1970: {e}")))?;
    let secs = dur.as_secs() as i64;
    let sub_ns = dur.subsec_nanos() as i64;
    let ns_of_day = secs.rem_euclid(86_400) * NS_PER_SEC + sub_ns;
    Ok(match func {
        ".qpl.d" => Value::Date((secs.div_euclid(86_400)) as i32 - DAYS_2000_TO_1970),
        ".qpl.t" => Value::Time(ns_of_day),
        ".qpl.p" => Value::Timestamp(secs * NS_PER_SEC + sub_ns - NS_2000_TO_1970),
        ".qpl.n" => Value::Timespan(ns_of_day),
        other => return Err(QplError::Runtime(format!("unknown function '{other}'"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_round_trips_known_anchors() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 1, 1), 10_957);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
        // leap day
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        // pre-2000
        assert_eq!(civil_from_days(days_from_civil(1999, 12, 31)), (1999, 12, 31));
    }

    fn round_trip(src: &str) -> String {
        format_temporal(&parse_temporal(src).unwrap_or_else(|| panic!("parse {src}"))).unwrap()
    }

    #[test]
    fn literal_round_trips() {
        assert_eq!(round_trip("2024.03.15"), "2024.03.15");
        assert_eq!(round_trip("2000.01.01"), "2000.01.01");
        assert_eq!(round_trip("1999.06.30"), "1999.06.30");
        assert_eq!(round_trip("2024.03m"), "2024.03m");
        assert_eq!(round_trip("12:30"), "12:30");
        assert_eq!(round_trip("12:30:00"), "12:30:00");
        assert_eq!(round_trip("12:30:00.000"), "12:30:00.000");
        assert_eq!(round_trip("09:30:15.250"), "09:30:15.250");
        assert_eq!(
            round_trip("2024.03.15D12:30:00.000000000"),
            "2024.03.15D12:30:00.000000000"
        );
        assert_eq!(
            round_trip("0D12:30:00.000000000"),
            "0D12:30:00.000000000"
        );
    }

    #[test]
    fn parses_to_expected_variant_and_offset() {
        assert_eq!(parse_temporal("2000.01.01"), Some(Value::Date(0)));
        assert_eq!(parse_temporal("2000.01.02"), Some(Value::Date(1)));
        assert_eq!(parse_temporal("2000.01m"), Some(Value::Month(0)));
        assert_eq!(parse_temporal("2001.03m"), Some(Value::Month(14)));
        assert_eq!(parse_temporal("00:00"), Some(Value::Minute(0)));
        assert_eq!(parse_temporal("01:00"), Some(Value::Minute(60)));
        assert_eq!(parse_temporal("00:00:05"), Some(Value::Second(5)));
        assert_eq!(parse_temporal("00:00:00.001"), Some(Value::Time(1_000_000)));
        assert_eq!(parse_temporal("0D00:00:00.000000001"), Some(Value::Timespan(1)));
        assert_eq!(
            parse_temporal("2000.01.01D00:00:00.000000000"),
            Some(Value::Timestamp(0))
        );
    }

    #[test]
    fn rejects_non_temporal() {
        assert_eq!(parse_temporal("2024.03"), None); // that's a float
        assert_eq!(parse_temporal("42"), None);
        assert_eq!(parse_temporal("hello"), None);
        assert_eq!(parse_temporal("2024.13.01"), None); // bad month
        assert_eq!(parse_temporal("25:00"), None); // bad hour
    }

    #[test]
    fn rejects_impossible_dates_instead_of_rolling_over() {
        assert_eq!(parse_temporal("2024.02.30"), None);
        assert_eq!(parse_temporal("2023.02.29"), None); // 2023 is not a leap year
        assert_eq!(parse_temporal("2024.04.31"), None);
        assert_eq!(round_trip("2024.02.29"), "2024.02.29"); // leap day is fine
    }

    #[test]
    fn formats_negative_and_over_a_day_times() {
        // a `time` can leave [0,24h) through arithmetic
        assert_eq!(format_temporal(&Value::Time(-14_400_000_000_000)), Some("-04:00:00.000".into()));
        assert_eq!(format_temporal(&Value::Time(108_000_000_000_000)), Some("30:00:00.000".into()));
    }

    #[test]
    fn timespan_can_be_negative() {
        assert_eq!(format_temporal(&Value::Timespan(-NS_PER_DAY)), Some("-1D00:00:00.000000000".into()));
    }

    #[test]
    fn now_functions_have_the_right_shapes() {
        assert!(matches!(now_value(".qpl.d"), Ok(Value::Date(_))));
        assert!(matches!(now_value(".qpl.t"), Ok(Value::Time(_))));
        assert!(matches!(now_value(".qpl.p"), Ok(Value::Timestamp(_))));
        assert!(matches!(now_value(".qpl.n"), Ok(Value::Timespan(_))));
        assert!(now_value(".qpl.x").is_err());
        if let Ok(Value::Time(ns)) = now_value(".qpl.t") {
            assert!((0..NS_PER_DAY).contains(&ns));
        }
    }
}
