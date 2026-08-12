use crate::errors::QplError;
use crate::vm::Value;
use std::collections::HashMap;

pub type BuiltinFn = fn(Vec<Value>) -> Result<Value, QplError>;

// pub fn get_builtins() -> HashMap<String, BuiltinFn> {

// }
