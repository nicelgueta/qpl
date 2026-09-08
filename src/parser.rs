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
                | TokenKind::Lazy
                | TokenKind::Collect
                | TokenKind::Symbol(_) => {
                    let stmt = self.parse_body()?;
                    Ok(Stmt::Assign { name, body: Box::new(stmt) })
                }
                // `\`a\`b!01b …` (sort) / `\`a\`b drop …` (drop) are table ops;
                // a bare `\`a\`b\`c` is a symbol-vector value (e.g. an enum definition)
                TokenKind::SymbolVec(_)
                    if matches!(self.peek2(), TokenKind::Bang | TokenKind::Drop)
                        || matches!(self.peek2(), TokenKind::Name(n) if n == "_") =>
                {
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
            // postfix sink: `<table-expr> >> <path>` / `<table-expr> sink <path>`
            if matches!(self.peek(), TokenKind::Sink) {
                self.next(); // consume `>>` / `sink`
                let path = self.parse_expr()?;
                return Ok(Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Sink {
                    src: Box::new(tbl_expr),
                    path,
                })));
            }
            Ok(Stmt::RetTable(tbl_expr))
        } else {
            self.parse_scalar_stmt()
        }
    }

    fn parse_scalar_stmt(&mut self) -> Result<Stmt, QplError> {
        match self.peek() {
            TokenKind::Name(_name) => {
                if matches!(self.peek2(), TokenKind::Sink) {
                    return Err(QplError::Parse(
                        "sink expects a table expression on the left (e.g. `\
                         `tbl >> `path` or `select … from tbl >> `path`), not a bare name".into(),
                    ));
                }
                Ok(Stmt::SingleVar(self.parse_expr()?))
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
            TokenKind::Lazy => {
                self.next(); // consume 'lazy'
                Ok(TableExpr::BuiltIn(BuiltIn::Lazy(Box::new(self.parse_lazy_operand()?))))
            }
            TokenKind::Collect => {
                self.next(); // consume 'collect'
                Ok(TableExpr::BuiltIn(BuiltIn::Collect(Box::new(self.parse_lazy_operand()?))))
            }
            TokenKind::Symbol(s) => {
                self.next(); // consume the symbol
                match self.peek() {
                    // bare `\`tbl` selects the whole table; `\`tbl >> path` → the
                    // postfix sink in `parse_body` handles the rest
                    TokenKind::Eof | TokenKind::Sink => {
                        Ok(TableExpr::Select(SelectStmt {
                            cols: vec![],
                            from: TableSource::InMem(s),
                            by: None,
                            where_: None,
                            order: None,
                            join: None,
                            update: false,
                            delete: false,
                        }))
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
        self.parse_expr_inner(true)
    }

    /// Like [`Parser::parse_expr`] but stops at a trailing `over`: the window
    /// binds to the whole aggregate (`(sum x) over p`), and an aggregate's own
    /// argument (`sum price * size`) must not swallow a following `over`.
    fn parse_value(&mut self) -> Result<Expr, QplError> {
        self.parse_expr_inner(false)
    }

    /// The core expression parser. `windows` enables the trailing `over` postfix.
    fn parse_expr_inner(&mut self, windows: bool) -> Result<Expr, QplError> {
        let left = self.parse_primary()?;

        // `u8!`$expr` (physical-width categorical) / `name::`$expr` (enum) —
        // a modifier token between the type and the `` `$ `` cast operator
        if let Some(cast) = self.parse_modified_cast(&left, false)? {
            return self.finish_window(cast, windows);
        }

        // infix `round`: `<precision> round <expr>` (q-style dyadic verb). The
        // precision is `left`; the value is the rest of the expression. `round`
        // binds tighter than `over`, so its value never swallows a window.
        if matches!(self.peek(), TokenKind::Name(n) if n == "round") {
            self.next();
            let value = self.parse_value()?;
            let call = Expr::Call { func: "round".into(), args: vec![value, left] };
            return self.finish_window(call, windows);
        }

        // bin op: left op right where right is the entire expr cos q is right to left eval
        if let TokenKind::Op(op) = self.peek().clone() {
            // cast: type$expr  e.g. f64$qty ;  `$expr  casts to a symbol / categorical
            if op == "$" {
                let target = cast_target(&left)?;
                self.next();
                let expr = self.parse_expr_inner(windows)?;
                return Ok(Expr::Cast { target, expr: Box::new(expr) });
            }
            self.next();
            let right = self.parse_expr_inner(windows)?;
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
                let arg = self.parse_value()?;
                let call = Expr::Call { func: name, args: vec![arg] };
                return self.finish_window(call, windows);
            }
        }
        self.finish_window(left, windows)
    }

    /// If `windows` and the next token is `over`, wrap `left` in an
    /// `Expr::Window` and let any arithmetic *after* the window continue
    /// (`... over `p - salary`). Otherwise return `left` unchanged.
    fn finish_window(&mut self, left: Expr, windows: bool) -> Result<Expr, QplError> {
        if !windows || !matches!(self.peek(), TokenKind::Over) {
            return Ok(left);
        }
        self.next(); // consume `over`
        let partition = self.parse_partition_syms()?;
        let order = if matches!(self.peek(), TokenKind::Order) {
            self.next();
            self.parse_window_order()?
        } else {
            Vec::new()
        };
        let win = Expr::Window { func: Box::new(left), partition, order };
        // `over` binds tighter than arithmetic: fold trailing binary operators.
        if let TokenKind::Op(op) = self.peek().clone()
            && op != "$"
        {
            self.next();
            let right = self.parse_expr_inner(true)?;
            return Ok(Expr::BinOp { left: Box::new(win), op, right: Box::new(right) });
        }
        Ok(win)
    }

    /// Partition keys after `over`: one `` `sym `` or a `` `a`b `` vector.
    fn parse_partition_syms(&mut self) -> Result<Vec<String>, QplError> {
        match self.next() {
            TokenKind::Symbol(s) => Ok(vec![s]),
            TokenKind::SymbolVec(v) => Ok(v),
            other => Err(QplError::Parse(format!(
                "expected a `partition symbol after 'over', got {other:?}"
            ))),
        }
    }

    /// Window `order` sub-clause: space-separated `` `col asc|desc `` pairs
    /// (no commas — a comma ends the clause and returns to the projection list).
    fn parse_window_order(&mut self) -> Result<Vec<(String, bool)>, QplError> {
        let mut order = Vec::new();
        loop {
            let column = match self.next() {
                TokenKind::Symbol(name) => name,
                other => return Err(QplError::Parse(format!(
                    "expected a `column after window 'order', got {other:?}"
                ))),
            };
            let descending = match self.next() {
                TokenKind::Asc => false,
                TokenKind::Desc => true,
                other => return Err(QplError::Parse(format!(
                    "expected 'asc' or 'desc' after window order column, got {other:?}"
                ))),
            };
            order.push((column, descending));
            if !matches!(self.peek(), TokenKind::Symbol(_)) {
                break;
            }
        }
        Ok(order)
    }

    /// Like [`Parser::parse_expr`] but without trailing juxtaposition-as-call:
    /// in a `log` argument list `a b` is two items, not `a(b)`. Binary ops and
    /// casts still compose; wrap an actual function call in parens.
    fn parse_expr_no_call(&mut self) -> Result<Expr, QplError> {
        let left = self.parse_primary()?;
        if let Some(cast) = self.parse_modified_cast(&left, true)? {
            return Ok(cast);
        }
        if let TokenKind::Op(op) = self.peek().clone() {
            if op == "$" {
                let target = cast_target(&left)?;
                self.next();
                return Ok(Expr::Cast { target, expr: Box::new(self.parse_expr_no_call()?) });
            }
            self.next();
            return Ok(Expr::BinOp {
                left: Box::new(left),
                op,
                right: Box::new(self.parse_expr_no_call()?),
            });
        }
        Ok(left)
    }

    /// `u8!`$expr`  → `CastTarget::SymPhysical("u8")`
    /// `name::`$expr` → `CastTarget::Enum("name")`
    /// `left` is whatever `parse_primary` produced before the modifier token.
    /// Returns `Ok(None)` when there is no `!` / `::` modifier to consume.
    fn parse_modified_cast(&mut self, left: &Expr, no_call: bool) -> Result<Option<Expr>, QplError> {
        let target = match (self.peek(), left) {
            (TokenKind::Bang, Expr::ColRef(w)) => {
                self.next();
                CastTarget::SymPhysical(w.clone())
            }
            (TokenKind::ColonColon, Expr::ColRef(name)) => {
                self.next();
                CastTarget::Enum(name.clone())
            }
            _ => return Ok(None),
        };
        // the modifier must be followed by the `` `$ `` cast operator
        match self.next() {
            TokenKind::Symbol(s) if s.is_empty() => {}
            other => return Err(QplError::Parse(format!("expected `$ after a cast modifier, got {other:?}"))),
        }
        match self.next() {
            TokenKind::Op(op) if op == "$" => {}
            other => return Err(QplError::Parse(format!("expected `$ after a cast modifier, got {other:?}"))),
        }
        let expr = if no_call { self.parse_expr_no_call()? } else { self.parse_expr()? };
        Ok(Some(Expr::Cast { target, expr: Box::new(expr) }))
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

    /// operand of `lazy` / `collect`: either a bare table variable name
    /// (`collect t`) or a full table expression (`lazy load \`x.parquet`).
    fn parse_lazy_operand(&mut self) -> Result<TableExpr, QplError> {
        if let TokenKind::Name(name) = self.peek().clone() {
            self.next();
            return Ok(TableExpr::Select(SelectStmt {
                cols: vec![],
                from: TableSource::InMem(name),
                by: None,
                where_: None,
                order: None,
                join: None,
                update: false,
                delete: false,
            }));
        }
        self.parse_table_expr()
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
            TokenKind::SymbolVec(v)=> Ok(Expr::Lit(Value::SymVec(v))),
            TokenKind::Symbol(s)   => Ok(Expr::Sym(s)),
            TokenKind::Name(n) if n == "i" => Ok(Expr::IColRef),
            TokenKind::Name(n)     => Ok(Expr::ColRef(n)),
            TokenKind::Op(op) if op == "?" => self.parse_case(),
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

/// Parse one or more juxtaposed expressions (space-separated), consuming every
/// token. Used by the `log` / `1` stdout-write, which evaluates each as a scalar
/// and concatenates the rendered values. Top-level juxtaposition separates
/// items rather than forming a call — see [`Parser::parse_expr_no_call`].
pub fn parse_expr_seq(tokens: Vec<Token>) -> Result<Vec<Expr>, QplError> {
    let mut parser = Parser { tokens, i: 0 };
    let mut exprs = Vec::new();
    while !matches!(parser.peek(), TokenKind::Eof) {
        exprs.push(parser.parse_expr_no_call()?);
    }
    Ok(exprs)
}

/// The token(s) just before a `$` in a cast expression, already parsed into
/// `left`, decide the cast target: `f64$x` → `Prim("f64")`, `` `$x `` → `Sym`.
/// `u8!`$x` and `name::`$x` are handled in `parse_expr` before this is reached.
fn cast_target(left: &Expr) -> Result<CastTarget, QplError> {
    match left {
        Expr::ColRef(name) => Ok(CastTarget::Prim(name.clone())),
        // bare backtick before `$` — `` `$expr `` casts to a symbol / categorical
        Expr::Sym(s) if s.is_empty() => Ok(CastTarget::Sym),
        _ => Err(QplError::Parse(format!("expected type name before '$', got {left:?}"))),
    }
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
        | TokenKind::Lazy
        | TokenKind::Collect
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

    fn seq(src: &str) -> Vec<Expr> {
        parse_expr_seq(tokenise(src).expect("lex error")).expect("parse error")
    }

    #[test]
    fn expr_seq_single() {
        assert_eq!(seq("\"hi\""), vec![Expr::Lit(Value::Str("hi".into()))]);
    }

    #[test]
    fn expr_seq_two_string_literals() {
        assert_eq!(seq("\"test\" \"me\""), vec![
            Expr::Lit(Value::Str("test".into())),
            Expr::Lit(Value::Str("me".into())),
        ]);
    }

    #[test]
    fn expr_seq_mixes_cast_and_binop_between_literals() {
        // "test" str$2*3 " that"  ->  three items, middle one a cast of 2*3
        let got = seq("\"test\" str$2*3 \" that\"");
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], Expr::Lit(Value::Str("test".into())));
        assert!(matches!(&got[1], Expr::Cast { target, .. } if *target == CastTarget::Prim("str".into())));
        assert_eq!(got[2], Expr::Lit(Value::Str(" that".into())));
    }

    #[test]
    fn expr_seq_juxtaposed_names_are_separate_items_not_a_call() {
        let got = seq("a b");
        assert_eq!(got, vec![Expr::ColRef("a".into()), Expr::ColRef("b".into())]);
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
    fn lazy_wraps_a_table_expr() {
        match p("t: lazy load `x.parquet") {
            Stmt::Assign { name, body } => {
                assert_eq!(name, "t");
                assert!(matches!(
                    *body,
                    Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Lazy(_)))
                ));
            }
            other => panic!("expected lazy assign, got {other:?}"),
        }
    }

    #[test]
    fn collect_takes_a_bare_var_name() {
        match p("tm: collect t") {
            Stmt::Assign { name, body } => {
                assert_eq!(name, "tm");
                match *body {
                    Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Collect(inner))) => {
                        assert!(matches!(
                            *inner,
                            TableExpr::Select(SelectStmt { from: TableSource::InMem(ref n), .. }) if n == "t"
                        ));
                    }
                    other => panic!("expected collect body, got {other:?}"),
                }
            }
            other => panic!("expected collect assign, got {other:?}"),
        }
    }

    #[test]
    fn sink_takes_a_table_ref_on_the_left_and_a_symbol_path() {
        match p("`t >> `out.parquet") {
            Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Sink { src, path })) => {
                assert!(matches!(
                    src.as_ref(),
                    TableExpr::Select(SelectStmt { from: TableSource::InMem(n), .. }) if n == "t"
                ));
                assert_eq!(path, Expr::Sym("out.parquet".into()));
            }
            other => panic!("expected sink, got {other:?}"),
        }
    }

    #[test]
    fn sink_accepts_a_full_select_on_the_left() {
        assert!(matches!(
            p("select price from trades >> `out.parquet"),
            Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Sink { src, .. }))
                if matches!(src.as_ref(), TableExpr::Select(_))
        ));
    }

    #[test]
    fn sink_rejects_a_bare_identifier_on_the_left() {
        assert!(parse(tokenise("t >> `out.parquet").unwrap()).is_err());
    }

    #[test]
    fn sink_path_can_cast_a_string_var_to_a_symbol() {
        // `\`$o` — intern the string held in `o` into a symbol path
        match p("`t >> `$o") {
            Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Sink { path, .. })) => {
                assert!(matches!(
                    &path,
                    Expr::Cast { target: CastTarget::Sym, expr }
                        if **expr == Expr::ColRef("o".into())
                ));
            }
            other => panic!("expected sink, got {other:?}"),
        }
    }

    #[test]
    fn sym_cast_parses_to_cast_target_sym() {
        let s = sel("select c: `$name from t");
        assert!(matches!(
            &s.cols[0].expr,
            Expr::Cast { target: CastTarget::Sym, expr } if **expr == Expr::ColRef("name".into())
        ));
    }

    #[test]
    fn physical_width_sym_cast_parses() {
        let s = sel("select c: u8!`$name from t");
        assert!(matches!(
            &s.cols[0].expr,
            Expr::Cast { target: CastTarget::SymPhysical(w), expr }
                if w == "u8" && **expr == Expr::ColRef("name".into())
        ));
    }

    #[test]
    fn enum_cast_parses() {
        let s = sel("select c: lvl::`$band from t");
        assert!(matches!(
            &s.cols[0].expr,
            Expr::Cast { target: CastTarget::Enum(n), expr }
                if n == "lvl" && **expr == Expr::ColRef("band".into())
        ));
    }

    #[test]
    fn bare_symbol_vector_is_a_scalar_value_assignment() {
        match p("lvl: `low`mid`high") {
            Stmt::ScalarAssign { name, expr } => {
                assert_eq!(name, "lvl");
                assert_eq!(expr, Expr::Lit(Value::SymVec(vec![
                    "low".into(), "mid".into(), "high".into(),
                ])));
            }
            other => panic!("expected scalar assign, got {other:?}"),
        }
    }

    #[test]
    fn bare_symbol_dict_sort_is_unaffected_by_cast_modifiers() {
        // `\`a\`b!01b \`t` must still parse as a sort, not a cast
        assert!(matches!(
            p("`c1`c2!01b `t"),
            Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Sort(..)))
        ));
    }

    #[test]
    fn case_expression_parses() {
        let s = sel("select price_bin: ?[price>100;`large;price>50;`med;`small] from data");
        assert_eq!(s.cols[0].name, Some("price_bin".into()));
        assert!(matches!(&s.cols[0].expr, Expr::Case { branches, .. } if branches.len() == 2));
    }

    #[test]
    fn infix_round_parses_to_a_call_value_then_precision() {
        let s = sel("select mv: 2 round market_value from t");
        assert_eq!(s.cols[0].expr, Expr::Call {
            func: "round".into(),
            args: vec![cref("market_value"), Expr::Lit(Value::Int(2))],
        });
    }

    // --- window functions ---

    fn window(func: Expr, partition: &[&str], order: &[(&str, bool)]) -> Expr {
        Expr::Window {
            func: Box::new(func),
            partition: partition.iter().map(|s| s.to_string()).collect(),
            order: order.iter().map(|(c, d)| (c.to_string(), *d)).collect(),
        }
    }

    #[test]
    fn window_over_binds_to_the_whole_aggregate() {
        let s = sel("select m: max salary over `country from t");
        assert_eq!(
            s.cols[0].expr,
            window(Expr::Call { func: "max".into(), args: vec![cref("salary")] }, &["country"], &[]),
        );
    }

    #[test]
    fn window_aggregate_argument_does_not_swallow_over() {
        // `sum price * size over `c` == `(sum(price*size)) over c`
        let s = sel("select v: sum price * size over `c from t");
        assert_eq!(
            s.cols[0].expr,
            window(
                Expr::Call { func: "sum".into(), args: vec![binop(cref("price"), "*", cref("size"))] },
                &["c"], &[],
            ),
        );
    }

    #[test]
    fn window_ranking_verb_with_partition_vector_and_order() {
        let s = sel("select r: rank over `country`role order `desk asc `date desc from t");
        assert_eq!(
            s.cols[0].expr,
            window(cref("rank"), &["country", "role"], &[("desk", false), ("date", true)]),
        );
    }

    #[test]
    fn window_binds_tighter_than_arithmetic() {
        // both forms mean `(max salary over `c) - salary`
        let bare   = sel("select g: max salary over `c - salary from t");
        let parens = sel("select g: (max salary over `c) - salary from t");
        let want = binop(
            window(Expr::Call { func: "max".into(), args: vec![cref("salary")] }, &["c"], &[]),
            "-",
            cref("salary"),
        );
        assert_eq!(bare.cols[0].expr, want);
        assert_eq!(parens.cols[0].expr, want);
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


