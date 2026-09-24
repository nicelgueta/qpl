use crate::errors::QplError;
use crate::tokens::{Token, TokenKind};

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_name_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_valid_symbol_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || '/' == c
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
                while i < n && is_valid_symbol_char(chars[i]) {
                    i += 1;
                }
                if ! (i < n && chars[i] == '`'){
                    let name: String = chars[s..i].iter().collect();
                    tokens.push(Token {
                        kind: TokenKind::Symbol(name),
                        pos: start,
                    });
                    continue
                }
                
                // this looks like a symbol vector
                // this should be a list of symbols separated by backticks, e.g. `a`b`c
                let mut symbols = Vec::new();
                let mut j = s;
                while j < n {
                    let k = j;
                    while j < n && is_valid_symbol_char(chars[j]){
                        j += 1;
                    }
                    let sym: String = chars[k..j].iter().collect();
                    symbols.push(sym);

                    // check if next is a backtick, if so, continue, else break
                    if j < n && chars[j] == '`' {
                        j += 1; // skip the backtick
                    } else {
                        break;
                    }
                }
                tokens.push(Token {
                    kind: TokenKind::SymbolVec(symbols),
                    pos: start,
                });
                i = j;    
            }
            '0'..='9' => {
                let mut j = i;
                // consume all leading digits first
                while j < n && chars[j].is_ascii_digit() {
                    j += 1;
                }
                let digits_end = j;
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
                // temporal literal: a run of digits / `.` / `:` / `D` that
                // `parse_temporal` accepts (`2024.03.15`, `12:30`, `0D12:30:00.0`,
                // `2024.03.15D09:30:00.000`, `2024.03m`). Falls through to the
                // float / int paths below when the shape doesn't match.
                if j < n && matches!(chars[j], '.' | ':' | 'D') {
                    let mut k = j;
                    while k < n && (chars[k].is_ascii_digit() || matches!(chars[k], '.' | ':' | 'D')) {
                        k += 1;
                    }
                    if k < n && chars[k] == 'm' {
                        k += 1; // `2024.03m` month suffix
                    }
                    let slice: String = chars[i..k].iter().collect();
                    if let Some(val) = crate::temporal::parse_temporal(&slice) {
                        tokens.push(Token { kind: TokenKind::Temporal(val), pos: start });
                        i = k;
                        continue;
                    }
                    // it looked temporal (`:` / `D` / a second `.`) but didn't
                    // parse — a clearer error than letting the float path choke
                    let dots = slice.bytes().filter(|&b| b == b'.').count();
                    if slice.contains([':', 'D']) || dots >= 2 {
                        return Err(QplError::Lex(format!("invalid temporal literal '{slice}'")));
                    }
                }
                // float: decimal point after integer digits
                let mut is_float = false;
                if j < n && chars[j] == '.' {
                    is_float = true;
                    j += 1;
                    while j < n && chars[j].is_ascii_digit() {
                        j += 1;
                    }
                }
                // scientific-notation suffix: `e5`, `E-2`, `e+3`. A negative
                // exponent (or a `.` mantissa) makes it a float; a bare
                // non-negative exponent folds to an `Int` (`10e5` == `1000000`)
                // so it composes with everything that expects a plain int
                // literal (e.g. `n limit`) without also having to accept a float.
                let mut int_exp: Option<u32> = None;
                if j < n && matches!(chars[j], 'e' | 'E') {
                    let mut k = j + 1;
                    let mut exp_negative = false;
                    if k < n && matches!(chars[k], '+' | '-') {
                        exp_negative = chars[k] == '-';
                        k += 1;
                    }
                    let exp_digits_start = k;
                    while k < n && chars[k].is_ascii_digit() {
                        k += 1;
                    }
                    if k > exp_digits_start {
                        if exp_negative {
                            is_float = true;
                        } else {
                            let exp_str: String = chars[exp_digits_start..k].iter().collect();
                            int_exp = Some(exp_str.parse().map_err(|_| {
                                QplError::Lex(format!("invalid exponent in numeric literal '{exp_str}'"))
                            })?);
                        }
                        j = k;
                    }
                }
                if is_float {
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
                if let Some(exponent) = int_exp {
                    let literal: String = chars[i..j].iter().collect();
                    let mantissa: i64 = chars[i..digits_end].iter().collect::<String>().parse().map_err(|_| {
                        QplError::Lex(format!("Invalid integer literal: {}", literal))
                    })?;
                    let scale = 10i64.checked_pow(exponent).ok_or_else(|| {
                        QplError::Lex(format!("integer literal '{literal}' overflows i64"))
                    })?;
                    let int_val = mantissa.checked_mul(scale).ok_or_else(|| {
                        QplError::Lex(format!("integer literal '{literal}' overflows i64"))
                    })?;
                    tokens.push(Token {
                        kind: TokenKind::Int(int_val),
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
                if chars.get(i + 1) == Some(&':') {
                    tokens.push(Token { kind: TokenKind::ColonColon, pos: start });
                    i += 2;
                } else {
                    tokens.push(Token { kind: TokenKind::Colon, pos: start });
                    i += 1;
                }
            }
            ',' => {
                tokens.push(Token {
                    kind: TokenKind::Comma,
                    pos: start,
                });
                i += 1;
            }
            '.' => {
                // a namespaced identifier: `.ns.name` (`.qpl.dt`, `.qpl.cfg`,
                // `.utils.helper` for a `\i`-imported script's bindings, …),
                // any number of `.segment`s. No new token/AST node — it's a
                // plain `Name`, so it flows through every existing
                // identifier path (variable/table lookup, bareword calls,
                // assignment targets) unchanged. (`.qpl.cfg` as a bare REPL
                // directive is still handled at the string level in repl.rs
                // and never reaches here; this only matters when `.qpl.cfg`
                // appears inside an expression, which it doesn't today.)
                let mut k = i + 1;
                if k < n && is_name_start(chars[k]) {
                    k += 1;
                    while k < n && is_name_char(chars[k]) {
                        k += 1;
                    }
                    while k < n && chars[k] == '.'
                        && k + 1 < n && is_name_start(chars[k + 1])
                    {
                        k += 1;
                        while k < n && is_name_char(chars[k]) {
                            k += 1;
                        }
                    }
                    let name: String = chars[i..k].iter().collect();
                    tokens.push(Token { kind: TokenKind::Name(name), pos: start });
                    i = k;
                    continue;
                }
                return Err(QplError::Lex(format!("Unexpected character: {c}")));
            }
            ';' => {
                tokens.push(Token { kind: TokenKind::Semicolon, pos: start });
                i += 1;
            }
            '#' => {
                tokens.push(Token {
                    kind: TokenKind::Hash,
                    pos: start,
                });
                i += 1;
            }
            '?' => {
                // vectorised conditional: `?[cond;then;else]`
                tokens.push(Token { kind: TokenKind::Op("?".to_string()), pos: start });
                i += 1;
            }
            '&' | '|' => {
                // logical and / or; single-char, no run-glomming
                tokens.push(Token { kind: TokenKind::Op(chars[i].to_string()), pos: start });
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
            '[' => {
                tokens.push(Token { kind: TokenKind::LBracket, pos: start });
                i += 1;
            }
            ']' => {
                tokens.push(Token { kind: TokenKind::RBracket, pos: start });
                i += 1;
            }
            '{' => {
                tokens.push(Token { kind: TokenKind::LBrace, pos: start });
                i += 1;
            }
            '}' => {
                tokens.push(Token { kind: TokenKind::RBrace, pos: start });
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
                        kind: TokenKind::Bang,
                        pos: start,
                    });
                }
            }
            '<' | '>' | '=' | '+' | '-' | '*' | '%' | '$' => {
                let mut j = i + 1;
                if j >= n {
                    tokens.push(Token {
                        kind: TokenKind::Op(chars[i].to_string()),
                        pos: start,
                    });
                    i = j;
                    continue;
                }
                // only `<= >= <>` are real multi-char operators; never glom a
                // following `+ - * $` etc. onto an operator (`int$-1`, `a%-b`)
                while j < n && "=<>".contains(chars[j]) {
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
                        "over"   => TokenKind::Over,
                        "order"  => TokenKind::Order,
                        "asc"    => TokenKind::Asc,
                        "desc"   => TokenKind::Desc,
                        "distinct" => TokenKind::Distinct,
                        "dropnull" => TokenKind::DropNull,
                        "limit" => TokenKind::Limit,
                        "drop" => TokenKind::Drop,
                        "update" => TokenKind::Update,
                        "delete" => TokenKind::Delete,
                        "cols"   => TokenKind::Cols,
                        "load"   => TokenKind::Load,
                        "sink"   => TokenKind::Sink,
                        "lazy"    => TokenKind::Lazy,
                        "collect" => TokenKind::Collect,
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

    #[test]
    fn integer_scientific_notation() {
        assert_eq!(kinds("1e6"), vec![TokenKind::Int(1_000_000)]);
        assert_eq!(kinds("10e5"), vec![TokenKind::Int(1_000_000)]);
        assert_eq!(kinds("1E3"), vec![TokenKind::Int(1_000)]);
        assert_eq!(kinds("3e0"), vec![TokenKind::Int(3)]);
        assert_eq!(kinds("2e+3"), vec![TokenKind::Int(2_000)]);
    }

    #[test]
    fn integer_scientific_notation_overflows() {
        assert!(tokenise("1e30").is_err());
    }

    #[test]
    fn scientific_notation_with_decimal_mantissa_or_negative_exponent_is_a_float() {
        assert_eq!(kinds("1.5e3"), vec![TokenKind::Float(1500.0)]);
        assert_eq!(kinds("5e-2"), vec![TokenKind::Float(0.05)]);
        assert_eq!(kinds("1e-6"), vec![TokenKind::Float(0.000001)]);
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

    #[test]
    fn float_not_confused_with_a_date() {
        // two dotted groups only — still a float, not a temporal literal
        assert_eq!(kinds("2024.03"), vec![TokenKind::Float(2024.03)]);
    }

    // --- temporal literals ---

    #[test]
    fn temporal_literals_lex_to_values() {
        use crate::ast::Value;
        assert_eq!(kinds("2024.03.15"), vec![TokenKind::Temporal(Value::Date(8840))]);
        assert_eq!(kinds("2000.01.01"), vec![TokenKind::Temporal(Value::Date(0))]);
        assert_eq!(kinds("2024.03m"), vec![TokenKind::Temporal(Value::Month(290))]);
        assert_eq!(kinds("09:30"), vec![TokenKind::Temporal(Value::Minute(570))]);
        assert_eq!(kinds("12:30:00"), vec![TokenKind::Temporal(Value::Second(45000))]);
        assert_eq!(kinds("12:30:00.000"), vec![TokenKind::Temporal(Value::Time(45_000_000_000_000))]);
        assert_eq!(kinds("0D00:00:00.000000001"), vec![TokenKind::Temporal(Value::Timespan(1))]);
        assert_eq!(
            kinds("2000.01.01D00:00:00.000000000"),
            vec![TokenKind::Temporal(Value::Timestamp(0))],
        );
    }

    #[test]
    fn temporal_literal_composes_with_an_operator() {
        use crate::ast::Value;
        assert_eq!(kinds("2024.03.15 + 10"), vec![
            TokenKind::Temporal(Value::Date(8840)),
            TokenKind::Op("+".into()),
            TokenKind::Int(10),
        ]);
    }

    #[test]
    fn temporal_shaped_but_invalid_literal_is_a_clear_error() {
        for bad in ["2024.13.01", "2024.02.30", "12:99"] {
            let err = tokenise(bad).unwrap_err();
            assert!(
                matches!(&err, QplError::Lex(m) if m.contains("temporal")),
                "{bad}: {err:?}"
            );
        }
    }

    #[test]
    fn qpl_now_functions_lex_as_their_own_token() {
        assert_eq!(kinds(".qpl.dt"), vec![TokenKind::Name(".qpl.dt".into())]);
        assert_eq!(kinds("log .qpl.ts"), vec![
            TokenKind::Name("log".into()),
            TokenKind::Name(".qpl.ts".into()),
        ]);
    }

    #[test]
    fn namespaced_identifier_lexes_as_a_dotted_name() {
        assert_eq!(kinds(".utils.helper"), vec![TokenKind::Name(".utils.helper".into())]);
        assert_eq!(kinds(".utils.sub.thing"), vec![TokenKind::Name(".utils.sub.thing".into())]);
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
    fn string_escaped_quote_mid_sentence() {
        assert_eq!(
            kinds(r#""my name is \"John\"""#),
            vec![TokenKind::Str(r#"my name is "John""#.into())],
        );
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
            TokenKind::SymbolVec(vec!["a".into(), "b".into(), "c".into()]),
        ]);
    }

    #[test]
    fn dict_basic() {
        assert_eq!(kinds("`a`b!1 2"), vec![
            TokenKind::SymbolVec(vec!["a".into(), "b".into()]),
            TokenKind::Bang,
            TokenKind::Int(1),
            TokenKind::Int(2),
        ]);
    }

    #[test]
    fn dict_bool_vec() {
        assert_eq!(kinds("`a`b`c!101b"), vec![
            TokenKind::SymbolVec(vec!["a".into(), "b".into(), "c".into()]),
            TokenKind::Bang,
            TokenKind::BoolVec(vec![true, false, true]),
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
    fn keyword_over() {
        assert_eq!(kinds("over"), vec![TokenKind::Over]);
    }

    #[test]
    fn keyword_where() {
        assert_eq!(kinds("where"), vec![TokenKind::Where]);
    }

    #[test]
    fn keyword_order_directions() {
        assert_eq!(kinds("order asc desc"), vec![TokenKind::Order, TokenKind::Asc, TokenKind::Desc]);
    }

    #[test]
    fn keyword_table_operators() {
        assert_eq!(kinds("distinct limit #"), vec![TokenKind::Distinct, TokenKind::Limit, TokenKind::Hash]);
    }

    #[test]
    fn keyword_delete() {
        assert_eq!(kinds("delete"), vec![TokenKind::Delete]);
    }

    #[test]
    fn show_is_no_longer_a_keyword() {
        assert_eq!(kinds("show"), vec![TokenKind::Name("show".into())]);
    }

    #[test]
    fn load_and_sink_are_keywords() {
        assert_eq!(kinds("load sink"), vec![TokenKind::Load, TokenKind::Sink]);
    }

    #[test]
    fn angle_bracket_pairs_are_not_load_or_sink() {
        assert_eq!(kinds("<<"), vec![TokenKind::Op("<<".into())]);
        assert_eq!(kinds(">>"), vec![TokenKind::Op(">>".into())]);
    }

    #[test]
    fn keyword_lazy_and_collect() {
        assert_eq!(kinds("lazy collect"), vec![TokenKind::Lazy, TokenKind::Collect]);
    }

    #[test]
    fn case_punctuation() {
        assert_eq!(kinds("?[a>1;`large;a>0;`small;`none]"), vec![
            TokenKind::Op("?".into()),
            TokenKind::LBracket,
            TokenKind::Name("a".into()),
            TokenKind::Op(">".into()),
            TokenKind::Int(1),
            TokenKind::Semicolon,
            TokenKind::Symbol("large".into()),
            TokenKind::Semicolon,
            TokenKind::Name("a".into()),
            TokenKind::Op(">".into()),
            TokenKind::Int(0),
            TokenKind::Semicolon,
            TokenKind::Symbol("small".into()),
            TokenKind::Semicolon,
            TokenKind::Symbol("none".into()),
            TokenKind::RBracket,
        ]);
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

    #[test]
    fn dollar_never_gloms_a_following_operator() {
        // `int$-45.3` must tokenise as a cast of a negative literal
        assert_eq!(kinds("int$-45.3"), vec![
            TokenKind::Name("int".into()),
            TokenKind::Op("$".into()),
            TokenKind::Op("-".into()),
            TokenKind::Float(45.3),
        ]);
    }

    #[test]
    fn op_logical_and_or() {
        for op in ["&", "|"] {
            assert_eq!(kinds(op), vec![TokenKind::Op(op.into())], "op: {op}");
        }
        // no run-glomming with adjacent operators
        assert_eq!(kinds("a|b"), vec![
            TokenKind::Name("a".into()),
            TokenKind::Op("|".into()),
            TokenKind::Name("b".into()),
        ]);
    }

    // --- punctuation ---

    #[test]
    fn punct_colon() {
        assert_eq!(kinds(":"), vec![TokenKind::Colon]);
    }

    #[test]
    fn punct_colon_colon() {
        assert_eq!(kinds("::"), vec![TokenKind::ColonColon]);
        assert_eq!(kinds("a: b"), vec![
            TokenKind::Name("a".into()), TokenKind::Colon, TokenKind::Name("b".into()),
        ]);
        assert_eq!(kinds("lvl::`$b"), vec![
            TokenKind::Name("lvl".into()),
            TokenKind::ColonColon,
            TokenKind::Symbol("".into()),
            TokenKind::Op("$".into()),
            TokenKind::Name("b".into()),
        ]);
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

    #[test]
    fn select_order_query() {
        let src = "select from trades order `col1 asc, `col2 desc";
        assert_eq!(kinds(src), vec![
            TokenKind::Select,
            TokenKind::From,
            TokenKind::Name("trades".into()),
            TokenKind::Order,
            TokenKind::Symbol("col1".into()),
            TokenKind::Asc,
            TokenKind::Comma,
            TokenKind::Symbol("col2".into()),
            TokenKind::Desc,
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

    #[test]
    fn braces_lex_to_brace_tokens() {
        assert_eq!(kinds("{[x,y] x+y }"), vec![
            TokenKind::LBrace,
            TokenKind::LBracket,
            TokenKind::Name("x".into()),
            TokenKind::Comma,
            TokenKind::Name("y".into()),
            TokenKind::RBracket,
            TokenKind::Name("x".into()),
            TokenKind::Op("+".into()),
            TokenKind::Name("y".into()),
            TokenKind::RBrace,
        ]);
    }
}

