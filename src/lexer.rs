use crate::errors::QplError;
use crate::tokens::Token;

pub struct Lexer {
    source: Vec<char>,
    pos: usize,
    line: usize,
}

impl Lexer {
    pub fn new(source: &str) -> Self {
        Self {
            source: source.chars().collect(),
            pos: 0,
            line: 1,
        }
    }

    pub fn tokenize(&mut self) -> Result<Vec<Token>, QplError> {
        let mut tokens = Vec::new();
        // TODO: implement tokenization
        tokens.push(Token::Eof);
        Ok(tokens)
    }
}
