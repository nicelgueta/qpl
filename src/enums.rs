use polars::{self, prelude::JoinType};

#[derive(Debug, Clone, PartialEq)]
pub enum PolarsStackArg {
    Join(JoinType)
}


#[derive(Debug, Clone, PartialEq)]
pub enum PolarsFrameExpr {
    Filter(usize),
    Join{ l: usize, r: usize }
}
