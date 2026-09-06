use polars::{self, prelude::JoinType};


#[derive(Debug, Clone, PartialEq)]
pub enum PolarsStackArg {
    Join(JoinType)
}

#[derive(Debug, Clone, PartialEq)]
pub enum SortDirection {
    Asc,
    Desc
}


#[derive(Debug, Clone, PartialEq)]
pub enum PolarsFrameExpr {
    Filter(usize),
    Join{ l: usize, r: usize },
    Sort(Vec<(String, bool)>), // col name -> descending
    Distinct,
    Limit(usize),
    Drop(Vec<String>),
    Cols
}
