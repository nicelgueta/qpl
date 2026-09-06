use std::fmt;
use crate::ast::{self, TableSource, Value};
use crate::enums::{PolarsFrameExpr, PolarsStackArg};

#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    FromSrc(TableSource),
    PushConst(Value),
    PushColRef(String),
    PushPolarsArg(PolarsStackArg),
    PushIColRef,
    PushScalar(Value),
    BinOp(String),
    Assign(String),
    Call { func: String, args_count: usize },
    Case { branches: usize },
    Alias { name: Option<String> },
    Eval(ast::Expr),
    Sink,

    // structural
    FrameExpr(PolarsFrameExpr), // pop n predicates and push filtered df
    BuildKeys(usize), // pop n expr into a key list
    BuildProj { count: usize, exclude: Vec<String>, predicates: usize }, // pop expressions into a projection list
    Select,
    SelectBy,
    Cast(String),
    Result,

}

impl fmt::Display for Instruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Instruction::FromSrc(tbl_src)             => write!(f, "FROM_SRC {tbl_src:?}"),
            Instruction::PushConst(val)                     => write!(f, "PUSH_CONST {val:?}"),
            Instruction::PushColRef(name)                  => write!(f, "PUSH_COL_REF {name}"),
            Instruction::PushScalar(name)                   => write!(f, "PUSH_SCALAR {name:?}"),
            Instruction::PushIColRef                                => write!(f, "PUSH_I_COL_REF"),
            Instruction::PushPolarsArg(arg)        => write!(f, "PUSH_POLARS_ARG {arg:?}"),
            Instruction::BinOp(op)                         => write!(f, "BIN_OP {op}"),
            Instruction::Call { func, args_count } => write!(f, "CALL {func} {args_count}"),
            Instruction::Case { branches } => write!(f, "CASE {branches}"),
            Instruction::Alias { name }            => write!(f, "ALIAS {:?}", name),
            Instruction::FrameExpr(expr)          => write!(f, "FRAME_EXPR {expr:?}"),
            Instruction::BuildKeys(n)                       => write!(f, "BUILD_KEYS {n}"),
            Instruction::BuildProj { count, .. }            => write!(f, "BUILD_PROJ {count}"),
            Instruction::Select                                     => write!(f, "SELECT"),
            Instruction::SelectBy                                   => write!(f, "SELECT_BY"),
            Instruction::Cast(dtype)                       => write!(f, "CAST {dtype}"),
            Instruction::Result                                     => write!(f, "RESULT"),
            Instruction::Assign(name )                     => write!(f, "ASSIGN {name}"),
            Instruction::Eval(expr)                          => write!(f, "EVAL {expr:?}"),
            Instruction::Sink                                       => write!(f, "SINK"),
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