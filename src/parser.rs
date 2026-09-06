use polars::prelude::JoinType;

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

    // main entry point
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
                TokenKind::Select 
                | TokenKind::Update
                | TokenKind::Delete
                | TokenKind::Distinct
                | TokenKind::Cols 
                | TokenKind::Load
                | TokenKind::Show
                | TokenKind::Symbol(_)
                | TokenKind::SymbolVec(_)=> {
                    let stmt = self.parse_body()?;
                    Ok(Stmt::Assign { name, body: Box::new(stmt) })
                }
                TokenKind::Int(_) if matches!(self.peek2(), TokenKind::Limit | TokenKind::Hash) => {
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
        if is_table_expr_start(self.peek())
            || (matches!(self.peek(), TokenKind::Int(_))
                && matches!(self.peek2(), TokenKind::Limit | TokenKind::Hash))
        {
            let tbl_expr = self.parse_table_expr()?;
            Ok(Stmt::RetTable(tbl_expr))
        } else {
            self.parse_scalar_stmt()
        }
    }

    fn parse_scalar_stmt(&mut self) -> Result<Stmt, QplError> {
        match self.peek() {
            TokenKind::Name(_name) => {
                // currently, only sinks are statements that 
                // start with a name variable
                // TODO: need to make more expandable
                if matches!(self.peek2(), TokenKind::Sink) {
                    if let TokenKind::Name(name) = self.next() { // consume name
                        self.next(); //consume sink
                        let res = match self.peek() {
                            TokenKind::Symbol(path) => {
                                Ok(
                                    Stmt::RetTable(
                                        TableExpr::BuiltIn(
                                            BuiltIn::Sink {
                                                name: TableSource::InMem(name),
                                                path: Value::Str(path.clone())
                                            }
                                        )
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

    fn parse_table_expr(&mut self) -> Result<TableExpr, QplError> {
        let peek = self.peek().clone();
        match peek {
            TokenKind::Select => Ok(TableExpr::Select(self.parse_query(false, false)?)),
            TokenKind::Update => Ok(TableExpr::Select(self.parse_query(true, false)?)),
            TokenKind::Delete => Ok(TableExpr::Select(self.parse_query(false, true)?)),
            TokenKind::Distinct => {
                self.next();
                Ok(TableExpr::BuiltIn(BuiltIn::Distinct(Box::new(self.parse_table_expr()?))))
            }
            TokenKind::Int(n) => {
                self.next();
                let operator = self.next();
                if !matches!(operator, TokenKind::Limit | TokenKind::Hash) {
                    return Err(QplError::Parse(format!("expected 'limit' or '#' after row count, got {operator:?}")));
                }
                let limit = usize::try_from(n)
                    .map_err(|_| QplError::Parse(format!("limit must be non-negative, got {n}")))?;
                Ok(TableExpr::BuiltIn(BuiltIn::Limit(Box::new(self.parse_table_expr()?), limit)))
            }
            TokenKind::Load => {
                // standalone: load "path" → select all from the file
                self.next();
                match self.next() {
                    TokenKind::Symbol(path) => Ok(TableExpr::Select(SelectStmt {
                        cols: vec![],
                        from: TableSource::Load(path),
                        by: None,
                        where_: None,
                        order: None,
                        join: None,
                        update: false,
                        delete: false,
                    })),
                    other => Err(QplError::Parse(format!("expected file path symbol after 'load', got {other:?}"))),
                }
            }
            TokenKind::Cols => {
                self.next(); // consume 'cols'
                let tbl_expr = self.parse_table_expr()?;
                Ok(TableExpr::BuiltIn(BuiltIn::Cols(Box::new(tbl_expr))))
            }
            TokenKind::Show => {
                self.next(); // consume 'show'
                let tbl_expr = self.parse_table_expr()?;
                Ok(TableExpr::BuiltIn(BuiltIn::Show(Box::new(tbl_expr))))
            }
            TokenKind::Symbol(s) => {
                self.next(); // consume the symbol
                match self.peek() {
                    TokenKind::Eof => {
                        let tbl_expr = TableExpr::Select(SelectStmt {
                            cols: vec![],
                            from: TableSource::InMem(s),
                            by: None,
                            where_: None,
                            order: None,
                            join: None,
                            update: false,
                            delete: false,
                        });
                        Ok(TableExpr::BuiltIn(BuiltIn::Show(Box::new(tbl_expr))))
                    }
                    TokenKind::Bang => {
                        // single symbol with a bang should actually 
                        // be a symvec with a single element
                        self.next(); // consume '!'
                        // TODO also make the use of a dict generic not just to sort
                        // so a sort in the compiler is pushing the map onto the stack
                        // and calling sort 
                        let peek = self.peek().clone();
                        match peek {
                            TokenKind::BoolVec(b) => {
                                self.next(); // consume the bool vector
                                let tbl_expr = self.parse_table_expr()?;
                                let sort_map = vec![(s, b[0])];
                                Ok(TableExpr::BuiltIn(BuiltIn::Sort(Box::new(tbl_expr), sort_map)))
                            }
                            TokenKind::Bool(b) => {
                                self.next(); // consume the bool
                                let tbl_expr = self.parse_table_expr()?;
                                let sort_map = vec![(s, b)];
                                Ok(TableExpr::BuiltIn(BuiltIn::Sort(Box::new(tbl_expr), sort_map)))
                            }
                            _ => Err(QplError::Parse(format!("expected bool vector after '!', got {:?}", self.peek()))),
                        }
                    }
                    TokenKind::Drop if matches!(self.peek(), TokenKind::Drop) => {
                        self.next();
                        Ok(TableExpr::BuiltIn(BuiltIn::Drop(vec![s], Box::new(self.parse_table_expr()?))))
                    }
                    TokenKind::Name(name) if name == "_" => {
                        self.next();
                        Ok(TableExpr::BuiltIn(BuiltIn::Drop(vec![s], Box::new(self.parse_table_expr()?))))
                    }
                    _ => Err(QplError::Parse(format!("expected 'asc' or 'desc' after symbol, got {:?}", self.peek()))),
                }
            }
            TokenKind::SymbolVec(v) => {
                self.next(); // consume the symbol vector
                let peek = self.peek().clone();
                if matches!(peek, TokenKind::Drop) || matches!(&peek, TokenKind::Name(name) if name == "_") {
                    self.next();
                    Ok(TableExpr::BuiltIn(BuiltIn::Drop(v, Box::new(self.parse_table_expr()?))))
                } else if matches!(peek, TokenKind::Bang) {
                    self.next(); // consume '!'
                    let peek = self.peek().clone();
                    if let TokenKind::BoolVec(b) = peek {
                        self.next(); // consume the bool vector
                        let tbl_expr = self.parse_table_expr()?;
                        let sort_map = v.iter().zip(b.iter()).map(|(s, b)| (s.clone(), *b)).collect();
                        Ok(TableExpr::BuiltIn(BuiltIn::Sort(Box::new(tbl_expr), sort_map)))
                    } else {
                        Err(QplError::Parse(format!("expected bool vector after '!', got {:?}", self.peek())))
                    }
                } else {
                    Err(QplError::Parse(format!("expected '!' after symbol vector, got {:?}", self.peek())))
                }
            }
            _ => Err(QplError::Parse(format!("invalid token for table expression, got {:?}", self.peek()))),
        }
    }

    fn parse_query(&mut self, update: bool, delete: bool) -> Result<SelectStmt, QplError> {
        if update {
            self.eat(&TokenKind::Update)?;
        } else if delete {
            self.eat(&TokenKind::Delete)?;
        } else {
            self.eat(&TokenKind::Select)?;
        }
        let cols = if delete {
            self.parse_delete_columns()?
        } else {
            self.parse_phrase(&[TokenKind::By, TokenKind::From])?
        };
        let mut by = None;
        if self.peek() == &TokenKind::By {
            self.next();
            by = Some(self.parse_phrase(&[TokenKind::From])?);
        }
        self.eat(&TokenKind::From)?;
        let from = self.parse_tbl_src_expr()?;
        let mut join = None;
        if !update && !delete && matches!(self.peek(), TokenKind::Symbol(_)) {
            join = Some(self.parse_join()?);
        }
        let mut where_ = None;
        if self.peek() == &TokenKind::Where {
            self.next();
            where_ = self.parse_where()?;
        }
        let mut order = None;
        if !update && !delete && self.peek() == &TokenKind::Order {
            self.next();
            order = Some(self.parse_order()?);
        }
        Ok(SelectStmt {
            cols,
            from,
            by,
            where_,
            order,
            join,
            update,
            delete,
        })
    }

    fn parse_delete_columns(&mut self) -> Result<Vec<Alias>, QplError> {
        if self.peek() == &TokenKind::From {
            return Ok(Vec::new());
        }
        let mut columns = Vec::new();
        loop {
            match self.next() {
                TokenKind::Name(name) => columns.push(Alias { name: None, expr: Expr::ColRef(name) }),
                TokenKind::Symbol(name) => columns.push(Alias { name: None, expr: Expr::Sym(name) }),
                TokenKind::SymbolVec(names) => columns.extend(names.into_iter().map(|name| Alias {
                    name: None,
                    expr: Expr::Sym(name),
                })),
                other => return Err(QplError::Parse(format!("expected column name after 'delete', got {other:?}"))),
            }
            if self.peek() != &TokenKind::Comma {
                break;
            }
            self.next();
        }
        Ok(columns)
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

    fn parse_case(&mut self) -> Result<Expr, QplError> {
        self.eat(&TokenKind::LBracket)?;
        let mut terms = vec![self.parse_expr()?];
        while self.peek() == &TokenKind::Semicolon {
            self.next();
            terms.push(self.parse_expr()?);
        }
        self.eat(&TokenKind::RBracket)?;
        if terms.len() < 3 || terms.len() % 2 == 0 {
            return Err(QplError::Parse("case expression requires condition/value pairs and a default".into()));
        }
        let default = Box::new(terms.pop().unwrap());
        let branches = terms.chunks_exact(2)
            .map(|pair| (pair[0].clone(), pair[1].clone()))
            .collect();
        Ok(Expr::Case { branches, default })
    }

    fn parse_tbl_src_expr(&mut self) -> Result<TableSource, QplError> {
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

    fn parse_order(&mut self) -> Result<Vec<(String, bool)>, QplError> {
        let mut order = Vec::new();
        loop {
            let column = match self.next() {
                TokenKind::Name(name) => name,
                TokenKind::Symbol(name) => name,
                other => return Err(QplError::Parse(format!("expected symbol column after 'order', got {other:?}"))),
            };
            let descending = match self.next() {
                TokenKind::Asc => false,
                TokenKind::Desc => true,
                other => return Err(QplError::Parse(format!("expected 'asc' or 'desc' after order column, got {other:?}"))),
            };
            order.push((column, descending));
            if self.peek() != &TokenKind::Comma {
                break;
            }
            self.next();
        }
        Ok(order)
    }

    /// format for the join phrase is: select ... from tbl1`id`name lj|ij|rj tbl2`id`f_name
    /// returns (join_src, left_on, right_on, join_type)
    fn parse_join(&mut self) -> Result<(TableSource, Value, Value, JoinType), QplError> {
        let mut left_on = Vec::new();
        let mut right_on = Vec::new();
        while matches!(self.peek(), TokenKind::Symbol(_)) {
            if let TokenKind::Symbol(s) = self.next() {
                left_on.push(s);
            } else {
                unreachable!()
            }
        }
        let left_on = Value::SymVec(left_on);
        let join_type = match self.next() {
            TokenKind::Name(n) => match n.as_str() {
                "lj" => JoinType::Left,
                "ij" => JoinType::Inner,
                "rj" => JoinType::Right,
                other => return Err(QplError::Parse(format!("expected join type (lj|ij|rj), got {other}"))),
            },
            other => return Err(QplError::Parse(format!("expected join type (lj|ij|rj), got {:?}", other))),
        };
        let join_src = self.parse_tbl_src_expr()?;
        while matches!(self.peek(), TokenKind::Symbol(_)) {
            if let TokenKind::Symbol(s) = self.next() {
                right_on.push(s);
            } else {
                unreachable!()
            }
        }
        let right_on = Value::SymVec(right_on);
        Ok((join_src, left_on, right_on, join_type))
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
            TokenKind::Op(op) if op == "$" => self.parse_case(),
            TokenKind::LParen      => {
                let expr = self.parse_expr()?;
                self.eat(&TokenKind::RParen)?;
                Ok(expr)
            },
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

fn is_table_expr_start(token: &TokenKind) -> bool {
    matches!(token,
        TokenKind::Select
        | TokenKind::Update
        | TokenKind::Delete
        | TokenKind::Distinct
        | TokenKind::Int(_)
        | TokenKind::Load
        | TokenKind::Cols
        | TokenKind::Show
        | TokenKind::SymbolVec(_)
        | TokenKind::Symbol(_)
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
            Stmt::RetTable(tbl_expr) => match tbl_expr {
                TableExpr::Select(sel) => sel,
                other => panic!("expected Select, got {other:?}"),
            },
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

    #[test]
    fn order_multiple_columns() {
        let s = sel("select from trades order col1 asc, col2 desc");
        assert_eq!(s.order, Some(vec![("col1".into(), false), ("col2".into(), true)]));
    }

    #[test]
    fn distinct_select() {
        assert!(matches!(
            p("distinct select from trades"),
            Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Distinct(_)))
        ));
    }

    #[test]
    fn limit_keyword_and_hash() {
        for source in ["10 limit select from trades", "10#`trades", "10#select from trades"] {
            assert!(matches!(
                p(source),
                Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Limit(_, 10)))
            ));
        }
    }

    #[test]
    fn drop_single_symbol_keyword_and_shorthand() {
        for source in ["`price drop select from trades", "`price _ `trades"] {
            assert!(matches!(
                p(source),
                Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Drop(columns, _))) if columns == vec!["price"]
            ));
        }
    }

    #[test]
    fn update_uses_select_query_shape() {
        match p("update price: price * 2 by sym from trades where size > 100") {
            Stmt::RetTable(TableExpr::Select(query)) => {
                assert!(query.update);
                assert_eq!(query.cols.len(), 1);
                assert!(query.by.is_some());
                assert!(query.where_.is_some());
            }
            other => panic!("expected update select, got {other:?}"),
        }
    }

    #[test]
    fn delete_uses_select_query_shape() {
        match p("delete `price`size from trades where size > 100") {
            Stmt::RetTable(TableExpr::Select(query)) => {
                assert!(query.delete);
                assert_eq!(query.cols.len(), 2);
                assert!(query.where_.is_some());
            }
            other => panic!("expected delete select, got {other:?}"),
        }
    }

    #[test]
    fn case_expression_parses() {
        let s = sel("select price_bin: $[price>100;`large;price>50;`med;`small] from data");
        assert_eq!(s.cols[0].name, Some("price_bin".into()));
        assert!(matches!(&s.cols[0].expr, Expr::Case { branches, .. } if branches.len() == 2));
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
                assert!(matches!(*body, Stmt::RetTable(_)));
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


