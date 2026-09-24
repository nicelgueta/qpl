use std::fmt;

use polars::error::PolarsError;

#[derive(Debug)]
pub enum QplError {
    Lex(String),
    Parse(String),
    Compile(String),
    Runtime(String),
    /// Ctrl-C stopped the running statement. Its own variant (not a `Runtime`
    /// string) so the script runner / REPL can tell it apart.
    Interrupted,
}

impl fmt::Display for QplError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QplError::Lex(msg) => write!(f, "LexError: {msg}"),
            QplError::Parse(msg) => write!(f, "ParseError: {msg}"),
            QplError::Compile(msg) => write!(f, "CompileError: {msg}"),
            QplError::Runtime(msg) => write!(f, "'{msg}"),
            QplError::Interrupted => write!(f, "'interrupted"),
        }
    }
}

impl std::error::Error for QplError {}

impl From<PolarsError> for QplError {
    fn from(err: PolarsError) -> Self {
        match &err {
            PolarsError::ShapeMismatch(_) => QplError::Runtime(format!("Shape mismatch: {}", err)),
            _ => QplError::Runtime(err.to_string())
        }
    }
}