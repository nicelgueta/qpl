
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // keywords
    Select,
    By,
    From,
    Where,
    Over,
    Order,
    Asc,
    Desc,
    Distinct,
    Limit,
    Drop,
    Update,
    Delete,
    // builtin func keywords
    Load,
    Sink,
    Cols,
    Lazy,
    Collect,

    //literals
    Name(String),
    Int(i64),
    // IntVec(Vec<i64>),
    Float(f64),
    // FloatVec(Vec<f64>),
    Symbol(String),
    SymbolVec(Vec<String>),
    Bool(bool),
    BoolVec(Vec<bool>),
    Str(String),
    /// a kdb temporal literal (`2024.03.15`, `12:30:00.000`, `0D12:30:00.0`, …),
    /// already parsed to the matching `ast::Value` variant by the lexer.
    Temporal(crate::ast::Value),
    /// a `.qpl.<name>` now-function reference (`.qpl.d`, `.qpl.p`, …). Carries
    /// the full name. A noun, not an operator — kept distinct so it never binds
    /// like one.
    QplNow(String),
    // StrVec(Vec<String>),

    // punc
    Colon,
    ColonColon,
    Comma,
    Semicolon,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Bang,
    Hash,
    Op(String),
    Eof,
}


#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub pos: usize,
}
