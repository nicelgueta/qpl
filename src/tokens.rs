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
    DropNull,
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

    // literals
    Name(String),
    Int(i64),
    Float(f64),
    Symbol(String),
    SymbolVec(Vec<String>),
    Bool(bool),
    BoolVec(Vec<bool>),
    Str(String),
    /// A temporal literal (`2024.03.15`, `12:30:00.000`, ...), already parsed by the lexer.
    Temporal(crate::ast::Value),

    // punc
    Colon,
    ColonColon,
    Comma,
    Semicolon,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
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
