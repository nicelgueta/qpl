use polars::{self, prelude::JoinType};


#[derive(Debug, Clone, PartialEq)]
pub enum PolarsStackArg {
    Join(JoinType)
}


/// Which window computation `Instruction::Window` performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFn {
    /// apply the popped aggregate/column expression per partition (`.over`)
    Over,
    /// `rn`    — SQL `row_number()`: strict 1..n ordinal in the window order
    RowNumber,
    /// `rank`  — SQL `rank()`: ties share the lowest rank, then a gap
    Rank,
    /// `drank` — SQL `dense_rank()`: ties share a rank, no gaps
    DenseRank,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PolarsFrameExpr {
    Filter(usize),
    Join{ l: usize, r: usize },
    Sort(Vec<(String, bool)>), // col name -> descending
    Distinct,
    /// first `n` rows for `n >= 0`, last `|n|` rows (tail) for `n < 0`; `n` is
    /// popped from the value stack (a preceding `Instruction::Eval` pushes it)
    Limit,
    Drop(Vec<String>),
    Cols
}