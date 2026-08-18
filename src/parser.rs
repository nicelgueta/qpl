use crate::ast::*;
use crate::builtins::BuiltIn;
use crate::errors::QplError;
use crate::tokens::{Token, TokenKind};

pub struct Parser {
    tokens: Vec<Token>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> &TokenKind {
        if self.i < self.tokens.len() {
            &self.tokens[self.i].kind
        } else {
            &TokenKind::Eof
        }
    }
    fn peek2(&self) -> &TokenKind {
        if self.i + 1 < self.tokens.len() {
            &self.tokens[self.i + 1].kind
        } else {
            &TokenKind::Eof
        }
    }
    fn next(&mut self) -> TokenKind {
        if self.i < self.tokens.len() {
            let kind = self.tokens[self.i].kind.clone();
            self.i += 1;
            kind
        } else {
            TokenKind::Eof
        }
    }
    fn eat(&mut self, kind: &TokenKind) -> Result<(), QplError> {
        if &self.peek() == &kind {
            self.i += 1;
            Ok(())
        } else {
            Err(QplError::Parse(format!("expected {kind:?}, got {:?}", self.peek())))
        }
    }

    fn parse_stmt(&mut self) -> Result<Stmt, QplError> {
        if matches!(self.peek(), TokenKind::Name(_)) && self.peek2() == &TokenKind::Colon {
            let name = if let TokenKind::Name(name) = self.next() {
                name
            } else {
                unreachable!()
            };
            self.eat(&TokenKind::Colon)?;
            // query keywords produce a table result; anything else is a scalar expression
            match self.peek() {
                TokenKind::Select | TokenKind::Cols | TokenKind::Load => {
                    let stmt = self.parse_body()?;
                    Ok(Stmt::Assign { name, body: Box::new(stmt) })
                }
                _ => {
                    let expr = self.parse_expr()?;
                    Ok(Stmt::ScalarAssign { name, expr })
                }
            }
        } else {
            Ok(self.parse_body()?)
        }
    }

    fn parse_body(&mut self) -> Result<Stmt, QplError> {
        match self.peek() {
            TokenKind::Select => Ok(Stmt::Select(self.parse_query()?)),
            TokenKind::Load => {
                // standalone: load "path" → select all from the file
                self.next();
                match self.next() {
                    TokenKind::Symbol(path) => Ok(Stmt::Select(SelectStmt {
                        cols: vec![],
                        from: TableSource::Load(path),
                        by: None,
                        where_: None,
                    })),
                    other => Err(QplError::Parse(format!("expected file path after 'load', got {other:?}"))),
                }
            }
            TokenKind::Cols => {
                self.next(); // consume 'cols'
                match self.next() {
                    TokenKind::Name(n) => Ok(Stmt::BuiltIn(BuiltIn::Cols(n))),
                    other => Err(QplError::Parse(format!("expected table name after 'cols', got {other:?}"))),
                }
            }
            TokenKind::Show => {
                self.next(); // consume 'show'
                match self.next() {
                    TokenKind::Name(tbl_name) => {
                        Ok(Stmt::BuiltIn(BuiltIn::Show(SelectStmt {
                            cols: vec![],
                            from: TableSource::InMem(tbl_name),
                            by: None,
                            where_: None
                        })))
                    },
                    other => Err(QplError::Parse(format!("expected table name after 'show', got {other:?}")))
                }
            }
            TokenKind::Name(_name) => {
                if matches!(self.peek2(), TokenKind::Sink) {
                    if let TokenKind::Name(name) = self.next() { // consume name
                        self.next(); //consume sink
                        let res = match self.peek() {
                            TokenKind::Symbol(path) => {
                                Ok(
                                    Stmt::BuiltIn(
                                        BuiltIn::Sink {
                                            name: TableSource::InMem(name),
                                            path: Value::Str(path.clone())
                                        }
                                    )
                                )
                            }
                            _ => Err(QplError::Parse(format!("Unexpected token: {:?}", self.peek())))
                        };
                        self.next(); // consume the path
                        res
                    } else {
                        unreachable!()
                    }
                } else {
                    Ok(Stmt::SingleVar(self.parse_expr()?))
                }
            }
            _ => Err(QplError::Parse(format!("Unexpected token: {:?}", self.peek()))),
        }
    }

    fn parse_query(&mut self) -> Result<SelectStmt, QplError> {
        self.eat(&TokenKind::Select)?;
        let select = self.parse_phrase(&[TokenKind::By, TokenKind::From])?;
        let mut by = None;
        if self.peek() == &TokenKind::By {
            self.next();
            by = Some(self.parse_phrase(&[TokenKind::From])?);
        }
        self.eat(&TokenKind::From)?;
        let from = self.parse_tbl_expr()?;
        let mut where_ = None;
        if self.peek() == &TokenKind::Where {
            self.next();
            where_ = self.parse_where()?;
        }
        Ok(SelectStmt {
            cols: select,
            from,
            by,
            where_,
        })
    }

    fn parse_phrase(&mut self, stop_tokens: &[TokenKind]) -> Result<Vec<Alias>, QplError> {
        if stop_tokens.contains(self.peek()) {
            return Ok(Vec::new());
        }
        let mut out_phrase = vec![self.parse_subphrase()?];
        while self.peek() == &TokenKind::Comma {
            self.next();
            let alias = self.parse_subphrase()?;
            out_phrase.push(alias);
        }
        Ok(out_phrase)
    }

    fn parse_subphrase(&mut self) -> Result<Alias, QplError> {
        let mut name = None;
        if matches!(self.peek(), TokenKind::Name(_)) && self.peek2() == &TokenKind::Colon {
            if let TokenKind::Name(n) = self.next() {
                name = Some(n);
            } else {
                unreachable!()
            }
            self.eat(&TokenKind::Colon)?;
        };
        Ok(Alias { name, expr: self.parse_expr()? })

    }

    fn parse_expr(&mut self) -> Result<Expr, QplError> {
        let left = self.parse_primary()?;

        // bin op: left op right where right is the entire expr cos q is right to left eval
        if let TokenKind::Op(op) = self.peek().clone() {
            // cast: type$expr  e.g. f64$qty
            if op == "$" {
                let dtype = match &left {
                    Expr::ColRef(name) => name.clone(),
                    _ => return Err(QplError::Parse(format!("expected type name before '$', got {left:?}"))),
                };
                self.next();
                let expr = self.parse_expr()?;
                return Ok(Expr::Cast { dtype, expr: Box::new(expr) });
            }
            self.next();
            let right = self.parse_expr()?;
            return Ok(Expr::BinOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            });
        }
        // call: left(args)
        if let Expr::ColRef(name) = &left {
            if is_noun_start(self.peek()) {
                let name = name.clone();
                let arg = self.parse_expr()?;
                return Ok(Expr::Call {
                    func: name,
                    args: vec![arg],
                });
            }
        }
        Ok(left)
    }

    fn parse_tbl_expr(&mut self) -> Result<TableSource, QplError> {
        match self.next() {
            TokenKind::Name(name) => Ok(TableSource::InMem(name)),
            TokenKind::Load => match self.next() {
                TokenKind::Symbol(path) => Ok(TableSource::Load(path)),
                other => Err(QplError::Parse(format!("expected file path after 'load', got {other:?}"))),
            },
            other => Err(QplError::Parse(format!("expected table name or load expression, got {other:?}"))),
        }
    }

    fn parse_where(&mut self) -> Result<Option<Vec<Expr>>, QplError> {
        let mut where_clause = vec![self.parse_expr()?];
        while self.peek() == &TokenKind::Comma {
            self.next();
            let expr = self.parse_expr()?;
            where_clause.push(expr);
        }
        Ok(Some(where_clause))
    }

    fn parse_primary(&mut self) -> Result<Expr, QplError> {
        match self.next() {
            TokenKind::Int(n)      => Ok(Expr::Lit(Value::Int(n))),
            TokenKind::Float(n)    => Ok(Expr::Lit(Value::Float(n))),
            TokenKind::Str(s)      => Ok(Expr::Lit(Value::Str(s))),
            TokenKind::Bool(b)     => Ok(Expr::Lit(Value::Bool(b))),
            TokenKind::BoolVec(v)  => Ok(Expr::Lit(Value::BoolVec(v))),
            TokenKind::Symbol(s)   => Ok(Expr::Sym(s)),
            TokenKind::Name(n) if n == "i" => Ok(Expr::IColRef),
            TokenKind::Name(n)     => Ok(Expr::ColRef(n)),
            TokenKind::LParen      => {
                let expr = self.parse_expr()?;
                self.eat(&TokenKind::RParen)?;
                Ok(expr)
            }
            other => Err(QplError::Parse(format!("Unexpected token in primary: {:?}", other))),
        }
    }

}

pub fn parse(tokens: Vec<Token>) -> Result<Stmt, QplError> {
    let mut parser = Parser { tokens, i: 0 };
    let stmt = parser.parse_stmt()?;
    parser.eat(&TokenKind::Eof)?;
    Ok(stmt)
}

fn is_noun_start(token: &TokenKind) -> bool {
    matches!(token,
        TokenKind::Name(_)
        | TokenKind::Int(_)
        | TokenKind::Float(_)
        | TokenKind::Str(_)
        | TokenKind::Bool(_)
        | TokenKind::Symbol(_)
        | TokenKind::BoolVec(_)
        | TokenKind::LParen
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenise;

    fn p(src: &str) -> Stmt {
        let tokens = tokenise(src).expect("lex error");
        parse(tokens).expect("parse error")
    }

    fn sel(src: &str) -> SelectStmt {
        match p(src) {
            Stmt::Select(s) => s,
            other => panic!("expected Select, got {other:?}"),
        }
    }

    fn col(expr: Expr) -> Alias {
        Alias { name: None, expr }
    }

    fn named(name: &str, expr: Expr) -> Alias {
        Alias { name: Some(name.into()), expr }
    }

    fn cref(s: &str) -> Expr {
        Expr::ColRef(s.into())
    }

    fn binop(left: Expr, op: &str, right: Expr) -> Expr {
        Expr::BinOp { left: Box::new(left), op: op.into(), right: Box::new(right) }
    }

    // --- basic selects ---

    #[test]
    fn select_single_col() {
        let s = sel("select px from trades");
        assert_eq!(s.cols, vec![col(cref("px"))]);
        assert_eq!(s.from, TableSource::InMem("trades".into()));
        assert_eq!(s.by, None);
        assert_eq!(s.where_, None);
    }

    #[test]
    fn select_multi_col() {
        let s = sel("select px, qty from trades");
        assert_eq!(s.cols, vec![col(cref("px")), col(cref("qty"))]);
    }

    #[test]
    fn select_empty_cols() {
        // select from t returns all columns
        let s = sel("select from t");
        assert_eq!(s.cols, vec![]);
        assert_eq!(s.from, TableSource::InMem("t".into()));
    }

    // --- aliases ---

    #[test]
    fn select_aliased_col() {
        let s = sel("select px: price from trades");
        assert_eq!(s.cols, vec![named("px", cref("price"))]);
    }

    #[test]
    fn select_mixed_alias() {
        let s = sel("select px: price, qty from trades");
        assert_eq!(s.cols, vec![named("px", cref("price")), col(cref("qty"))]);
    }

    // --- literals ---

    #[test]
    fn select_int_literal() {
        let s = sel("select 42 from t");
        assert_eq!(s.cols, vec![col(Expr::Lit(Value::Int(42)))]);
    }

    #[test]
    fn select_float_literal() {
        let s = sel("select 3.14 from t");
        assert_eq!(s.cols, vec![col(Expr::Lit(Value::Float(3.14)))]);
    }

    #[test]
    fn select_bool_literal() {
        let s = sel("select 1b from t");
        assert_eq!(s.cols, vec![col(Expr::Lit(Value::Bool(true)))]);
    }

    #[test]
    fn select_str_literal() {
        let s = sel(r#"select "hello" from t"#);
        assert_eq!(s.cols, vec![col(Expr::Lit(Value::Str("hello".into())))]);
    }

    // --- expressions ---

    #[test]
    fn select_binop() {
        let s = sel("select px + qty from t");
        assert_eq!(s.cols, vec![col(binop(cref("px"), "+", cref("qty")))]);
    }

    #[test]
    fn select_aliased_binop() {
        let s = sel("select dbl: c3*2 from t");
        assert_eq!(s.cols, vec![named("dbl", binop(cref("c3"), "*", Expr::Lit(Value::Int(2))))]);
    }

    #[test]
    fn select_fn_call() {
        let s = sel("select sum price from trades");
        assert_eq!(s.cols, vec![col(Expr::Call {
            func: "sum".into(),
            args: vec![cref("price")],
        })]);
    }

    #[test]
    fn select_aliased_fn_call() {
        let s = sel("select total: sum price from trades");
        assert_eq!(s.cols, vec![named("total", Expr::Call {
            func: "sum".into(),
            args: vec![cref("price")],
        })]);
    }

    #[test]
    fn select_icol() {
        let s = sel("select i from t");
        assert_eq!(s.cols, vec![col(Expr::IColRef)]);
    }

    // --- symbol literal ---

    #[test]
    fn select_sym_literal() {
        let s = sel("select `AAPL from t");
        assert_eq!(s.cols, vec![col(Expr::Sym("AAPL".into()))]);
    }

    // --- where clause ---

    #[test]
    fn where_simple() {
        let s = sel("select px from trades where qty > 0");
        assert_eq!(s.where_, Some(vec![binop(cref("qty"), ">", Expr::Lit(Value::Int(0)))]));
    }

    #[test]
    fn where_symbol_eq() {
        let s = sel("select px from trades where sym = `AAPL");
        assert_eq!(s.where_, Some(vec![binop(cref("sym"), "=", Expr::Sym("AAPL".into()))]));
    }

    #[test]
    fn where_multiple_conditions() {
        let s = sel("select px from trades where sym = `AAPL, qty > 0");
        assert_eq!(s.where_, Some(vec![
            binop(cref("sym"), "=", Expr::Sym("AAPL".into())),
            binop(cref("qty"), ">", Expr::Lit(Value::Int(0))),
        ]));
    }

    // --- by clause ---

    #[test]
    fn by_single() {
        let s = sel("select sum px by sym from trades");
        assert_eq!(s.by, Some(vec![col(cref("sym"))]));
    }

    #[test]
    fn by_aliased() {
        let s = sel("select sum px by s: sym from trades");
        assert_eq!(s.by, Some(vec![named("s", cref("sym"))]));
    }

    // --- assignment ---

    #[test]
    fn assign_select() {
        match p("t: select px from trades") {
            Stmt::Assign { name, body } => {
                assert_eq!(name, "t");
                assert!(matches!(*body, Stmt::Select(_)));
            }
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    // --- full query (the key example) ---

    #[test]
    fn full_query() {
        // select dbl: c3*2 by c1 from t where c2>15
        let s = sel("select dbl: c3*2 by c1 from t where c2>15");
        assert_eq!(s.cols, vec![named("dbl", binop(cref("c3"), "*", Expr::Lit(Value::Int(2))))]);
        assert_eq!(s.by, Some(vec![col(cref("c1"))]));
        assert_eq!(s.from, TableSource::InMem("t".into()));
        assert_eq!(s.where_, Some(vec![binop(cref("c2"), ">", Expr::Lit(Value::Int(15)))]));
    }
}


