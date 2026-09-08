
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // keywords
    Select,
    By,
    From,
    Where,
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
