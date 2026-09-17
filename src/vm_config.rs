//! Interpreter configuration set at run time via `.qpl.cfg`. To add a knob:
//! give it a field + default here and a match arm in [`VmConfig::set`] —
//! nothing else in the pipeline needs to change.

use polars_ops::prelude::RoundMode;
use crate::errors::QplError;

#[derive(Debug, Clone)]
pub struct VmConfig {
    /// max columns physically printed when rendering a table (`maxcol`)
    pub maxcol: usize,
    /// max rows physically printed when rendering a table (`maxrow`)
    pub maxrow: usize,
    /// rounding mode used by the `round` column function (`round_type`)
    pub round_type: RoundMode,
    /// max characters wide a printed table may be, `-1` = unlimited (`tblwidth`)
    pub tblwidth: i64,
    /// max characters shown per cell before truncating with an ellipsis,
    /// `-1` = unlimited (`strlen`)
    pub strlen: i64,
    /// when `true`, a raw integer crossing the `Int`/`timestamp` boundary
    /// (`` `timestamp$n ``, `` `long$ts ``) is read/written as ns since kdb's
    /// `2000.01.01` epoch, matching the internal [`crate::ast::Value::Timestamp`]
    /// representation; when `false` (the default) it's ns since the Unix epoch
    /// (`1970.01.01`), matching what a whole-column `` `timestamp$ `` cast
    /// already does under Polars and what non-kdb users expect (`useqepoch`)
    pub useqepoch: bool,
}

impl Default for VmConfig {
    fn default() -> Self {
        // mirror Polars' own display defaults
        Self {
            maxcol: 8,
            maxrow: 10,
            round_type: RoundMode::HalfToEven,
            tblwidth: -1,
            strlen: 30,
            useqepoch: false,
        }
    }
}

impl VmConfig {
    /// Apply one `key=value` assignment. Unknown keys / bad values are errors.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), QplError> {
        match key {
            "maxcol" => self.maxcol = parse_cfg_usize(key, value)?,
            "maxrow" => self.maxrow = parse_cfg_usize(key, value)?,
            "round_type" => self.round_type = parse_round_type(value)?,
            "tblwidth" => self.tblwidth = parse_cfg_i64(key, value)?,
            "strlen" => self.strlen = parse_cfg_i64(key, value)?,
            "useqepoch" => self.useqepoch = parse_cfg_bool(key, value)?,
            _ => return Err(QplError::Runtime(format!(
                "unknown config '{key}' (known: maxcol, maxrow, round_type, tblwidth, strlen, useqepoch)"
            ))),
        }
        // the row/col/width/strlen limits are read by Polars from the environment at render time
        match key {
            "maxcol" => unsafe { std::env::set_var("POLARS_FMT_MAX_COLS", self.maxcol.to_string()) },
            "maxrow" => unsafe { std::env::set_var("POLARS_FMT_MAX_ROWS", self.maxrow.to_string()) },
            "tblwidth" => unsafe { std::env::set_var("POLARS_TABLE_WIDTH", self.tblwidth.to_string()) },
            // Polars' formatter takes a negative POLARS_FMT_STR_LEN literally as
            // usize::MAX and then overflows adding padding to it (fmt.rs), unlike
            // POLARS_TABLE_WIDTH which clamps negatives to u16::MAX itself — so
            // `-1` (unlimited) is translated to a large-but-safe finite value here.
            "strlen" => unsafe {
                let v = if self.strlen < 0 { i32::MAX as i64 } else { self.strlen };
                std::env::set_var("POLARS_FMT_STR_LEN", v.to_string())
            },
            _ => {}
        }
        Ok(())
    }

    /// One `key=value` line per knob — printed by a bare `.qpl.cfg`.
    pub fn describe(&self) -> String {
        format!(
            "maxcol={}\nmaxrow={}\nround_type={}\ntblwidth={}\nstrlen={}\nuseqepoch={}",
            self.maxcol, self.maxrow, round_type_name(self.round_type), self.tblwidth, self.strlen,
            self.useqepoch,
        )
    }
}

fn parse_cfg_usize(key: &str, value: &str) -> Result<usize, QplError> {
    value.parse().map_err(|_| {
        QplError::Runtime(format!("config '{key}' expects a non-negative integer, got '{value}'"))
    })
}

fn parse_cfg_i64(key: &str, value: &str) -> Result<i64, QplError> {
    value.parse().map_err(|_| {
        QplError::Runtime(format!("config '{key}' expects an integer, got '{value}'"))
    })
}

fn parse_cfg_bool(key: &str, value: &str) -> Result<bool, QplError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(QplError::Runtime(format!(
            "config '{key}' expects true/false, got '{value}'"
        ))),
    }
}

fn parse_round_type(value: &str) -> Result<RoundMode, QplError> {
    match value.to_ascii_uppercase().as_str() {
        "HALF_UP" => Ok(RoundMode::HalfAwayFromZero),
        "HALF_TO_EVEN" => Ok(RoundMode::HalfToEven),
        _ => Err(QplError::Runtime(format!(
            "round_type must be HALF_UP or HALF_TO_EVEN, got '{value}'"
        ))),
    }
}

fn round_type_name(mode: RoundMode) -> &'static str {
    match mode {
        RoundMode::HalfAwayFromZero => "HALF_UP",
        _ => "HALF_TO_EVEN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_set_rejects_unknown_key_and_bad_value() {
        let mut cfg = VmConfig::default();
        assert!(cfg.set("nope", "1").is_err());
        assert!(cfg.set("maxrow", "abc").is_err());
        assert!(cfg.set("round_type", "sideways").is_err());
        assert!(cfg.set("tblwidth", "abc").is_err());
        assert!(cfg.set("strlen", "abc").is_err());
        cfg.set("maxrow", "42").unwrap();
        cfg.set("maxcol", "7").unwrap();
        cfg.set("round_type", "half_up").unwrap(); // case-insensitive
        cfg.set("tblwidth", "120").unwrap();
        cfg.set("strlen", "-1").unwrap();
        assert_eq!(cfg.maxrow, 42);
        assert_eq!(cfg.maxcol, 7);
        assert_eq!(cfg.round_type, RoundMode::HalfAwayFromZero);
        assert_eq!(cfg.tblwidth, 120);
        assert_eq!(cfg.strlen, -1);
        assert_eq!(VmConfig::default().tblwidth, -1);
        assert_eq!(VmConfig::default().strlen, 30);
    }
}
