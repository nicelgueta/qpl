use crate::errors::QplError;
use crate::tokens::{Token, TokenKind};

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

pub fn tokenise(src: &str) -> Result<Vec<Token>, QplError> {
    let chars: Vec<char> = src.chars().collect();
    let n: usize = chars.len();
    let mut tokens = Vec::new();
    let mut i = 0usize;

    while i < n {
        let c = chars[i];
        let start = i;
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                i += 1;
            }
            '/' => {
                // q-style comment - single slash
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            '"' => {
                i += 1;
                let mut buf = String::new();
                while i < n && chars[i] != '"' {
                    if chars[i] == '\\' && i + 1 < n {
                        i += 1;
                        buf.push(match chars[i] {
                            'n'  => '\n',
                            't'  => '\t',
                            'r'  => '\r',
                            '"'  => '"',
                            '\\' => '\\',
                            other => other,
                        });
                    } else {
                        buf.push(chars[i]);
                    }
                    i += 1;
                }
                if i < n && chars[i] == '"' {
                    i += 1;
                    tokens.push(Token {
                        kind: TokenKind::Str(buf),
                        pos: start,
                    });
                } else {
                    return Err(QplError::Lex("Unterminated string literal".to_string()));
                }
            }
            '`' => {
                i += 1;
                let s = i;
                while i < n && is_name_char(chars[i]) {
                    i += 1;
                }
                let name: String = chars[s..i].iter().collect();
                tokens.push(Token {
                    kind: TokenKind::Symbol(name),
                    pos: start,
                });
            }
            '0'..='9' => {
                let mut j = i;
                // consume all leading digits first
                while j < n && chars[j].is_ascii_digit() {
                    j += 1;
                }
                // bool or bool-vec: all digits 0/1 followed by 'b'
                if j < n && chars[j] == 'b' && chars[i..j].iter().all(|&c| c == '0' || c == '1') {
                    let bits: Vec<bool> = chars[i..j].iter().map(|&c| c == '1').collect();
                    let kind = if bits.len() == 1 {
                        TokenKind::Bool(bits[0])
                    } else {
                        TokenKind::BoolVec(bits)
                    };
                    i = j + 1;
                    tokens.push(Token { kind, pos: start });
                    continue;
                }
                // float: decimal point after integer digits
                if j < n && chars[j] == '.' {
                    j += 1;
                    while j < n && chars[j].is_ascii_digit() {
                        j += 1;
                    }
                    let float_str: String = chars[i..j].iter().collect();
                    let float_val: f64 = float_str.parse().map_err(|_| {
                        QplError::Lex(format!("Invalid float literal: {}", float_str))
                    })?;
                    tokens.push(Token {
                        kind: TokenKind::Float(float_val),
                        pos: start,
                    });
                    i = j;
                    continue;
                }
                // integer
                let int_str: String = chars[i..j].iter().collect();
                let int_val: i64 = int_str.parse().map_err(|_| {
                    QplError::Lex(format!("Invalid integer literal: {}", int_str))
                })?;
                tokens.push(Token {
                    kind: TokenKind::Int(int_val),
                    pos: start,
                });
                i = j;
                continue;
            }
            ':' => {
                tokens.push(Token {
                    kind: TokenKind::Colon,
                    pos: start,
                });
                i += 1;
            }
            ',' => {
                tokens.push(Token {
                    kind: TokenKind::Comma,
                    pos: start,
                });
                i += 1;
            }
            '(' => {
                tokens.push(Token {
                    kind: TokenKind::LParen,
                    pos: start,
                });
                i += 1;
            }
            ')' => {
                tokens.push(Token {
                    kind: TokenKind::RParen,
                    pos: start,
                });
                i += 1;
            }
            '!' => {
                i += 1;
                if chars.get(i) == Some(&'=') {
                    tokens.push(Token {
                        kind: TokenKind::Op("!=".to_string()),
                        pos: start,
                    });
                    i += 1;
                } else {
                    tokens.push(Token {
                        kind: TokenKind::Op("!".to_string()),
                        pos: start,
                    });
                }
            }
            '<' | '>' | '=' | '+' | '-' | '*' | '/' | '%' | '$' => {
                let mut j = i + 1;
                while j < n && "+-*/=<>!".contains(chars[j]) {
                    j += 1;
                }
                let op: String = chars[i..j].iter().collect();
                tokens.push(Token {
                    kind: TokenKind::Op(op),
                    pos: start,
                });
                i = j;
            }
            _ => {
                if is_name_start(c) {
                    let mut j = i + 1;
                    while j < n && is_name_char(chars[j]) {
                        j += 1;
                    }
                    let name: String = chars[i..j].iter().collect();
                    let kind = match name.as_str() {
                        // key words
                        "select" => TokenKind::Select,
                        "by"     => TokenKind::By,
                        "from"   => TokenKind::From,
                        "where"  => TokenKind::Where,
                        "cols"   => TokenKind::Cols,
                        "scan"   => TokenKind::Scan,
                        _ => TokenKind::Name(name),
                    };
                    tokens.push(Token { kind, pos: start });
                    i = j;
                } else {
                    return Err(QplError::Lex(format!("Unexpected character: {}", c)));
                }
            }
        }

    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::TokenKind;

    fn kinds(src: &str) -> Vec<TokenKind> {
        tokenise(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    // --- integers ---

    #[test]
    fn integer_simple() {
        assert_eq!(kinds("42"), vec![TokenKind::Int(42)]);
    }

    #[test]
    fn integer_zero() {
        assert_eq!(kinds("0"), vec![TokenKind::Int(0)]);
    }

    #[test]
    fn integer_multiple() {
        assert_eq!(kinds("1 2 3"), vec![
            TokenKind::Int(1),
            TokenKind::Int(2),
            TokenKind::Int(3),
        ]);
    }

    // --- floats ---

    #[test]
    fn float_basic() {
        assert_eq!(kinds("3.14"), vec![TokenKind::Float(3.14)]);
    }

    #[test]
    fn float_zero() {
        assert_eq!(kinds("0.0"), vec![TokenKind::Float(0.0)]);
    }

    // --- booleans ---

    #[test]
    fn bool_true() {
        assert_eq!(kinds("1b"), vec![TokenKind::Bool(true)]);
    }

    #[test]
    fn bool_false() {
        assert_eq!(kinds("0b"), vec![TokenKind::Bool(false)]);
    }

    // --- bool vectors ---

    #[test]
    fn bool_vec_basic() {
        assert_eq!(kinds("1010b"), vec![TokenKind::BoolVec(vec![true, false, true, false])]);
    }

    #[test]
    fn bool_vec_all_false() {
        assert_eq!(kinds("000b"), vec![TokenKind::BoolVec(vec![false, false, false])]);
    }

    // --- strings ---

    #[test]
    fn string_simple() {
        assert_eq!(kinds(r#""hello""#), vec![TokenKind::Str("hello".into())]);
    }

    #[test]
    fn string_escaped_quote() {
        assert_eq!(kinds(r#""say \"hi\"""#), vec![TokenKind::Str(r#"say "hi""#.into())]);
    }

    #[test]
    fn string_escape_sequences() {
        assert_eq!(kinds(r#""\n\t""#), vec![TokenKind::Str("\n\t".into())]);
    }

    #[test]
    fn string_empty() {
        assert_eq!(kinds(r#""""#), vec![TokenKind::Str("".into())]);
    }

    // --- symbols ---

    #[test]
    fn symbol_simple() {
        assert_eq!(kinds("`AAPL"), vec![TokenKind::Symbol("AAPL".into())]);
    }

    #[test]
    fn symbol_null() {
        // bare backtick is the null symbol in q
        assert_eq!(kinds("`"), vec![TokenKind::Symbol("".into())]);
    }

    #[test]
    fn symbol_list() {
        assert_eq!(kinds("`a`b`c"), vec![
            TokenKind::Symbol("a".into()),
            TokenKind::Symbol("b".into()),
            TokenKind::Symbol("c".into()),
        ]);
    }

    // --- keywords ---

    #[test]
    fn keyword_select() {
        assert_eq!(kinds("select"), vec![TokenKind::Select]);
    }

    #[test]
    fn keyword_by() {
        assert_eq!(kinds("by"), vec![TokenKind::By]);
    }

    #[test]
    fn keyword_from() {
        assert_eq!(kinds("from"), vec![TokenKind::From]);
    }

    #[test]
    fn keyword_where() {
        assert_eq!(kinds("where"), vec![TokenKind::Where]);
    }

    // --- identifiers ---

    #[test]
    fn name_simple() {
        assert_eq!(kinds("foo"), vec![TokenKind::Name("foo".into())]);
    }

    #[test]
    fn name_underscore_prefix() {
        assert_eq!(kinds("_x"), vec![TokenKind::Name("_x".into())]);
    }

    #[test]
    fn name_mixed_case() {
        assert_eq!(kinds("camelCase"), vec![TokenKind::Name("camelCase".into())]);
    }

    #[test]
    fn name_with_digits() {
        assert_eq!(kinds("x1"), vec![TokenKind::Name("x1".into())]);
    }

    // --- operators ---

    #[test]
    fn op_arith() {
        for op in ["+", "-", "*"] {
            assert_eq!(kinds(op), vec![TokenKind::Op(op.into())], "op: {op}");
        }
    }

    #[test]
    fn op_eq() {
        assert_eq!(kinds("="), vec![TokenKind::Op("=".into())]);
    }

    #[test]
    fn op_not_eq() {
        assert_eq!(kinds("!="), vec![TokenKind::Op("!=".into())]);
    }

    #[test]
    fn op_comparison() {
        for (src, expected) in [("<", "<"), (">", ">"), ("<=", "<="), (">=", ">=")] {
            assert_eq!(kinds(src), vec![TokenKind::Op(expected.into())], "op: {src}");
        }
    }

    // --- punctuation ---

    #[test]
    fn punct_colon() {
        assert_eq!(kinds(":"), vec![TokenKind::Colon]);
    }

    #[test]
    fn punct_comma() {
        assert_eq!(kinds(","), vec![TokenKind::Comma]);
    }

    #[test]
    fn punct_parens() {
        assert_eq!(kinds("()"), vec![TokenKind::LParen, TokenKind::RParen]);
    }

    // --- whitespace & comments ---

    #[test]
    fn whitespace_skipped() {
        assert_eq!(kinds("  42  "), vec![TokenKind::Int(42)]);
    }

    #[test]
    fn comment_rest_of_line() {
        // '/' comments out to end of line
        assert_eq!(kinds("42 / ignore this\n99"), vec![TokenKind::Int(42), TokenKind::Int(99)]);
    }

    #[test]
    fn comment_full_line() {
        assert_eq!(kinds("/ whole line\n42"), vec![TokenKind::Int(42)]);
    }

    // --- assignment ---

    #[test]
    fn assignment() {
        assert_eq!(kinds("x: 42"), vec![
            TokenKind::Name("x".into()),
            TokenKind::Colon,
            TokenKind::Int(42),
        ]);
    }

    // --- full select query ---

    #[test]
    fn select_query() {
        let src = "select px: price, qty from trades where sym = `AAPL";
        assert_eq!(kinds(src), vec![
            TokenKind::Select,
            TokenKind::Name("px".into()),
            TokenKind::Colon,
            TokenKind::Name("price".into()),
            TokenKind::Comma,
            TokenKind::Name("qty".into()),
            TokenKind::From,
            TokenKind::Name("trades".into()),
            TokenKind::Where,
            TokenKind::Name("sym".into()),
            TokenKind::Op("=".into()),
            TokenKind::Symbol("AAPL".into()),
        ]);
    }

    // --- positions ---

    #[test]
    fn token_positions() {
        let tokens = tokenise("x: 42").unwrap();
        assert_eq!(tokens[0].pos, 0); // x
        assert_eq!(tokens[1].pos, 1); // :
        assert_eq!(tokens[2].pos, 3); // 42
    }

    // --- error cases ---

    #[test]
    fn unterminated_string_is_error() {
        assert!(tokenise(r#""hello"#).is_err());
    }

}

