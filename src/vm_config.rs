//! Session settings set with `.qpl.cfg`. A new knob is a field, a default,
//! and a match arm in [`VmConfig::set`].

use crate::errors::QplError;
use polars_ops::prelude::RoundMode;

#[derive(Debug, Clone)]
pub struct VmConfig {
    /// max columns printed when rendering a table
    pub maxcol: usize,
    /// max rows printed when rendering a table
    pub maxrow: usize,
    /// rounding mode for `round`
    pub round_type: RoundMode,
    /// max printed table width in characters; `-1` = unlimited
    pub tblwidth: i64,
    /// max characters shown per cell; `-1` = unlimited
    pub strlen: i64,
    /// Epoch for raw integers crossing the timestamp boundary (`` `timestamp$n ``,
    /// `` `long$ts ``): kdb's 2000.01.01 if `true`, else the Unix epoch
    /// (matching Polars' column casts).
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
    /// Apply one `key=value`. Unknown keys and bad values are errors.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), QplError> {
        match key {
            "maxcol" => self.maxcol = parse_cfg_usize(key, value)?,
            "maxrow" => self.maxrow = parse_cfg_usize(key, value)?,
            "round_type" => self.round_type = parse_round_type(value)?,
            "tblwidth" => self.tblwidth = parse_cfg_i64(key, value)?,
            "strlen" => self.strlen = parse_cfg_i64(key, value)?,
            "useqepoch" => self.useqepoch = parse_cfg_bool(key, value)?,
            _ => {
                return Err(QplError::Runtime(format!(
                    "unknown config '{key}' (known: maxcol, maxrow, round_type, tblwidth, strlen, useqepoch)"
                )));
            }
        }
        self.export_render_limits(key);
        Ok(())
    }

    /// Polars reads the display limits from the environment at render time,
    /// so mirror the knob there. Not on wasm, which has no process environment
    /// (`set_var` panics); the value is still stored.
    #[cfg(not(target_family = "wasm"))]
    fn export_render_limits(&self, key: &str) {
        match key {
            "maxcol" => unsafe {
                std::env::set_var("POLARS_FMT_MAX_COLS", self.maxcol.to_string())
            },
            "maxrow" => unsafe {
                std::env::set_var("POLARS_FMT_MAX_ROWS", self.maxrow.to_string())
            },
            "tblwidth" => unsafe {
                std::env::set_var("POLARS_TABLE_WIDTH", self.tblwidth.to_string())
            },
            // Polars overflows on a negative POLARS_FMT_STR_LEN (unlike
            // POLARS_TABLE_WIDTH), so map `-1` to a large finite value.
            "strlen" => unsafe {
                let v = if self.strlen < 0 {
                    i32::MAX as i64
                } else {
                    self.strlen
                };
                std::env::set_var("POLARS_FMT_STR_LEN", v.to_string())
            },
            _ => {}
        }
    }

    #[cfg(target_family = "wasm")]
    fn export_render_limits(&self, _key: &str) {}

    /// One `key=value` line per knob, for a bare `.qpl.cfg`.
    pub fn describe(&self) -> String {
        format!(
            "maxcol={}\nmaxrow={}\nround_type={}\ntblwidth={}\nstrlen={}\nuseqepoch={}",
            self.maxcol,
            self.maxrow,
            round_type_name(self.round_type),
            self.tblwidth,
            self.strlen,
            self.useqepoch,
        )
    }
}

fn parse_cfg_usize(key: &str, value: &str) -> Result<usize, QplError> {
    value.parse().map_err(|_| {
        QplError::Runtime(format!(
            "config '{key}' expects a non-negative integer, got '{value}'"
        ))
    })
}

fn parse_cfg_i64(key: &str, value: &str) -> Result<i64, QplError> {
    value
        .parse()
        .map_err(|_| QplError::Runtime(format!("config '{key}' expects an integer, got '{value}'")))
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

    /// `set` writes process-wide `POLARS_FMT_*` env vars shared by every test
    /// in this binary, so a test that changes them must restore them (the
    /// golden-output tests depend on the defaults).
    fn saved_env(keys: &[&str]) -> Vec<(String, Option<String>)> {
        keys.iter()
            .map(|k| (k.to_string(), std::env::var(k).ok()))
            .collect()
    }

    fn restore_env(saved: Vec<(String, Option<String>)>) {
        for (k, v) in saved {
            match v {
                Some(v) => unsafe { std::env::set_var(&k, v) },
                None => unsafe { std::env::remove_var(&k) },
            }
        }
    }

    #[test]
    fn config_set_rejects_unknown_key_and_bad_value() {
        let saved = saved_env(&[
            "POLARS_FMT_MAX_COLS",
            "POLARS_FMT_MAX_ROWS",
            "POLARS_TABLE_WIDTH",
            "POLARS_FMT_STR_LEN",
        ]);
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
        restore_env(saved);
    }
}
