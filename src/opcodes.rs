use std::fmt;
use crate::ast::{self, CastTarget, TableSource, Value};
use crate::enums::{PolarsFrameExpr, PolarsStackArg, WindowFn};

#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    FromSrc(TableSource),
    PushConst(Value),
    PushColRef(String),
    PushPolarsArg(PolarsStackArg),
    PushIColRef,
    BinOp(String),
    Assign(String),
    Call { func: String, args_count: usize },
    /// `<precision> round <col>` — round a float column to `decimals` places.
    /// The rounding mode is read from `VmConfig::round_type` at execution time.
    Round { decimals: u32 },
    /// `<func> over <partition> [order <keys>]` — a window function. For
    /// `WindowFn::Over` the target expression is popped from the stack; the
    /// ranking verbs synthesise their own expression. `rolling` is
    /// `Some((agg, window))` for `<agg> <col> <n>!rolling over ...`, where the
    /// popped stack value is the raw column and `agg` names the rolling
    /// reduction instead of it being pre-applied.
    Window {
        func: WindowFn,
        partition: Vec<String>,
        order: Vec<(String, bool)>,
        rolling: Option<(String, usize)>,
    },
    Case { branches: usize },
    Alias { name: Option<String> },
    Eval(ast::Expr),
    Sink,
    /// mark the current statement as lazy: its result is stored/returned as a
    /// LazyFrame plan rather than collected into a DataFrame.
    Lazy,
    /// force the current lazy plan to materialise into a DataFrame.
    Collect,

    // structural
    FrameExpr(PolarsFrameExpr), // pop n predicates and push filtered df
    BuildKeys(usize), // pop n expr into a key list
    BuildProj { count: usize, exclude: Vec<String>, predicates: usize }, // pop expressions into a projection list
    Select,
    SelectBy,
    Cast(CastTarget),
    Result,

}

impl fmt::Display for Instruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Instruction::FromSrc(tbl_src)             => write!(f, "FROM_SRC {tbl_src:?}"),
            Instruction::PushConst(val)                     => write!(f, "PUSH_CONST {val:?}"),
            Instruction::PushColRef(name)                  => write!(f, "PUSH_COL_REF {name}"),
            Instruction::PushIColRef                                => write!(f, "PUSH_I_COL_REF"),
            Instruction::PushPolarsArg(arg)        => write!(f, "PUSH_POLARS_ARG {arg:?}"),
            Instruction::BinOp(op)                         => write!(f, "BIN_OP {op}"),
            Instruction::Call { func, args_count } => write!(f, "CALL {func} {args_count}"),
            Instruction::Round { decimals } => write!(f, "ROUND {decimals}"),
            Instruction::Window { func, partition, order, rolling } => {
                write!(f, "WINDOW {func:?} [{}]", partition.join(","))?;
                if !order.is_empty() {
                    let keys: Vec<String> = order.iter()
                        .map(|(c, d)| format!("{c} {}", if *d { "desc" } else { "asc" }))
                        .collect();
                    write!(f, " order [{}]", keys.join(","))?;
                }
                if let Some((agg, window)) = rolling {
                    write!(f, " rolling {agg}/{window}")?;
                }
                Ok(())
            }
            Instruction::Case { branches } => write!(f, "CASE {branches}"),
            Instruction::Alias { name }            => write!(f, "ALIAS {:?}", name),
            Instruction::FrameExpr(expr)          => write!(f, "FRAME_EXPR {expr:?}"),
            Instruction::BuildKeys(n)                       => write!(f, "BUILD_KEYS {n}"),
            Instruction::BuildProj { count, .. }            => write!(f, "BUILD_PROJ {count}"),
            Instruction::Select                                     => write!(f, "SELECT"),
            Instruction::SelectBy                                   => write!(f, "SELECT_BY"),
            Instruction::Cast(target)                      => match target {
                CastTarget::Prim(d)         => write!(f, "CAST {d}"),
                CastTarget::Sym             => write!(f, "CAST sym"),
                CastTarget::SymPhysical(w)  => write!(f, "CAST sym!{w}"),
                CastTarget::Enum(name)      => write!(f, "CAST enum({name})"),
            },
            Instruction::Result                                     => write!(f, "RESULT"),
            Instruction::Assign(name )                     => write!(f, "ASSIGN {name}"),
            Instruction::Eval(expr)                          => write!(f, "EVAL {expr:?}"),
            Instruction::Sink                                       => write!(f, "SINK"),
            Instruction::Lazy                                       => write!(f, "LAZY"),
            Instruction::Collect                                    => write!(f, "COLLECT"),
        }
    }
}

pub fn disassemble_instructions(instructions: &[Instruction]) -> Vec<String> {
    let mut result = Vec::new();
    for (i, instr) in instructions.iter().enumerate() {
        result.push(format!("{:04}: {}", i, instr));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Value;

    fn disp(instr: Instruction) -> String {
        instr.to_string()
    }

    // --- Display for each variant ---

    #[test]
    fn display_push_const_int() {
        assert_eq!(disp(Instruction::PushConst(Value::Int(42))), "PUSH_CONST Int(42)");
    }

    #[test]
    fn display_push_const_str() {
        assert_eq!(disp(Instruction::PushConst(Value::Str("hi".into()))), r#"PUSH_CONST Str("hi")"#);
    }

    #[test]
    fn display_push_const_bool() {
        assert_eq!(disp(Instruction::PushConst(Value::Bool(true))),  "PUSH_CONST Bool(true)");
        assert_eq!(disp(Instruction::PushConst(Value::Bool(false))), "PUSH_CONST Bool(false)");
    }

    #[test]
    fn display_push_col_ref() {
        assert_eq!(disp(Instruction::PushColRef("price".into())), "PUSH_COL_REF price");
    }

    #[test]
    fn display_push_i_col_ref() {
        assert_eq!(disp(Instruction::PushIColRef), "PUSH_I_COL_REF");
    }

    #[test]
    fn display_bin_op() {
        for op in ["+", "-", "*", "%", "=", ">", "<", ">=", "<=", "<>"] {
            assert_eq!(disp(Instruction::BinOp(op.into())), format!("BIN_OP {op}"));
        }
    }

    #[test]
    fn display_call() {
        assert_eq!(
            disp(Instruction::Call { func: "sum".into(), args_count: 1 }),
            "CALL sum 1"
        );
        assert_eq!(
            disp(Instruction::Call { func: "avg".into(), args_count: 2 }),
            "CALL avg 2"
        );
    }

    #[test]
    fn display_round() {
        assert_eq!(disp(Instruction::Round { decimals: 2 }), "ROUND 2");
    }

    #[test]
    fn display_window() {
        use crate::enums::WindowFn;
        assert_eq!(
            disp(Instruction::Window {
                func: WindowFn::Over,
                partition: vec!["country".into()],
                order: vec![],
                rolling: None,
            }),
            "WINDOW Over [country]",
        );
        assert_eq!(
            disp(Instruction::Window {
                func: WindowFn::Rank,
                partition: vec!["country".into(), "role".into()],
                order: vec![("desk".into(), false), ("date".into(), true)],
                rolling: None,
            }),
            "WINDOW Rank [country,role] order [desk asc,date desc]",
        );
        assert_eq!(
            disp(Instruction::Window {
                func: WindowFn::Over,
                partition: vec!["sym".into()],
                order: vec![("date".into(), false)],
                rolling: Some(("sum".into(), 3)),
            }),
            "WINDOW Over [sym] order [date asc] rolling sum/3",
        );
    }

    #[test]
    fn display_alias_some() {
        assert_eq!(disp(Instruction::Alias { name: Some("px".into()) }), r#"ALIAS Some("px")"#);
    }

    #[test]
    fn display_alias_none() {
        assert_eq!(disp(Instruction::Alias { name: None }), "ALIAS None");
    }

    #[test]
    fn display_filter() {
        assert_eq!(disp(Instruction::FrameExpr(PolarsFrameExpr::Filter(3))), "FRAME_EXPR Filter(3)");
    }

    #[test]
    fn display_build_keys() {
        assert_eq!(disp(Instruction::BuildKeys(2)), "BUILD_KEYS 2");
    }

    #[test]
    fn display_build_proj() {
        assert_eq!(disp(Instruction::BuildProj { count: 4, exclude: vec![], predicates: 0 }), "BUILD_PROJ 4");
    }

    #[test]
    fn display_select() {
        assert_eq!(disp(Instruction::Select), "SELECT");
    }

    #[test]
    fn display_select_by() {
        assert_eq!(disp(Instruction::SelectBy), "SELECT_BY");
    }

    #[test]
    fn display_result() {
        assert_eq!(disp(Instruction::Result), "RESULT");
    }


    // --- disassemble ---

    #[test]
    fn disassemble_empty() {
        assert_eq!(disassemble_instructions(&[]), Vec::<String>::new());
    }

    #[test]
    fn disassemble_sequential_indices() {
        let instrs = vec![
            Instruction::PushColRef("px".into()),
            Instruction::Alias { name: None },
            Instruction::Select,
            Instruction::Result,
        ];
        let lines = disassemble_instructions(&instrs);
        assert_eq!(lines[0], "0000: PUSH_COL_REF px");
        assert_eq!(lines[1], "0001: ALIAS None");
        assert_eq!(lines[2], "0002: SELECT");
        assert_eq!(lines[3], "0003: RESULT");
    }

    #[test]
    fn disassemble_pads_to_four_digits() {
        let instrs = vec![Instruction::Result];
        assert_eq!(disassemble_instructions(&instrs)[0], "0000: RESULT");
    }

    // --- equality ---

    #[test]
    fn eq_same_variant() {
        assert_eq!(Instruction::PushColRef("a".into()), Instruction::PushColRef("a".into()));
        assert_eq!(Instruction::Select, Instruction::Select);
    }

    #[test]
    fn ne_different_variant() {
        assert_ne!(Instruction::Select, Instruction::SelectBy);
        assert_ne!(Instruction::BinOp("+".into()), Instruction::BinOp("-".into()));
    }

    #[test]
    fn clone_round_trip() {
        let instr = Instruction::Call { func: "sum".into(), args_count: 1 };
        assert_eq!(instr.clone(), instr);
    }
}