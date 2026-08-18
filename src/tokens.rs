
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // keywords
    Select,
    By,
    From,
    Where,
    Scan,
    Sink,
    // builtin keywords
    Cols,
    Show,

    //literals
    Name(String),
    Int(i64),
    Float(f64),
    Symbol(String),
    Bool(bool),
    BoolVec(Vec<bool>),
    Str(String),

    // punc
    Colon,
    Comma,
    LParen,
    RParen,
    Op(String),
    Eof,
}


#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub pos: usize,
}
