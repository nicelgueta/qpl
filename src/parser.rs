use polars::prelude::JoinType;

use crate::ast::*;
use crate::builtins::BuiltIn;
use crate::errors::QplError;
use crate::tokens::{Token, TokenKind};

/// Words that can't be bound as a variable or parameter name: `while[..]` and
/// `noop` are parsed straight off the token stream (see `parse_primary`), so a
/// same-named binding would be unreachable.
const RESERVED: &[&str] = &["while", "noop"];

fn check_not_reserved(name: &str) -> Result<(), QplError> {
    if RESERVED.contains(&name) {
        return Err(QplError::Parse(format!("'{name}' is a reserved word")));
    }
    Ok(())
}

pub struct Parser {
    tokens: Vec<Token>,
    i: usize,
    /// Do juxtaposed string literals (`"a" "b"`) fold into one `StrVec`? On
    /// everywhere except the top level of a `log` argument list, where
    /// juxtaposition already means "separate items to concatenate" — there a
    /// string vector needs parens (`log ("a" "b")`). See [`Parser::with_str_runs`].
    str_runs: bool,
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
    fn peek3(&self) -> &TokenKind {
        if self.i + 2 < self.tokens.len() {
            &self.tokens[self.i + 2].kind
        } else {
            &TokenKind::Eof
        }
    }
    /// Is the parser sat right before `` `w!hopen ``? Distinguishes the
    /// write-mode connection modifier from the superficially similar
    /// `` `c!01b t `` sort-map / `` `a`b!... `` table-op grammar, both of
    /// which are also `Symbol` immediately followed by `Bang`.
    fn is_whopen_modifier(&self) -> bool {
        matches!(self.peek(), TokenKind::Symbol(s) if s == "w")
            && self.peek2() == &TokenKind::Bang
            && matches!(self.peek3(), TokenKind::Name(n) if n == "hopen")
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
            check_not_reserved(&name)?;
            // `name: {[..] ..}` — binding a function literal. Ordinary scalar
            // assignment of an ordinary value (see `Value::Closure`); it only
            // short-circuits `parse_body` here because a leading `{` has no
            // meaning on the table side.
            if self.peek() == &TokenKind::LBrace {
                return Ok(Stmt::ScalarAssign { name, expr: self.parse_func_lit()? });
            }
            // query keywords produce a table result; anything else is a scalar
            // expression. `parse_body` decides between a table statement and a
            // column expression (a one-column select → `Stmt::SingleVar`), so we
            // just rewrap whatever it returns.
            match self.peek() {
                TokenKind::Select
                | TokenKind::Update
                | TokenKind::Delete
                | TokenKind::Distinct
                | TokenKind::Cols
                | TokenKind::Load
                | TokenKind::Lazy
                | TokenKind::Collect => self.assign_from_body(name),
                // `\`c!01b <tbl>` (sort) / `\`a\`b drop <tbl>` (drop) are table ops
                // keyed off the leading symbol; a bare `\`a\`b\`c` is a symbol
                // vector value (e.g. an enum definition), and a bare `\`x` is a
                // plain symbol — tables are referenced by name, never by symbol.
                TokenKind::Symbol(_) | TokenKind::SymbolVec(_)
                    if !self.is_whopen_modifier()
                        && (matches!(self.peek2(), TokenKind::Bang | TokenKind::Drop | TokenKind::DropNull)
                            || matches!(self.peek2(), TokenKind::Name(n) if n == "_")) =>
                {
                    self.assign_from_body(name)
                }
                // `n limit <table-expr>` is a table operation; `n#…` is always a
                // take/slice value expression (the VM decides frame vs list).
                TokenKind::Int(_) if self.peek2() == &TokenKind::Limit => {
                    self.assign_from_body(name)
                }
                // `-n limit <table-expr>` — same, with a leading unary minus.
                TokenKind::Op(op)
                    if op == "-"
                        && matches!(self.peek2(), TokenKind::Int(_))
                        && self.peek3() == &TokenKind::Limit =>
                {
                    self.assign_from_body(name)
                }
                // `<name> limit <table-expr>` — a bound global as the count.
                TokenKind::Name(_) if self.peek2() == &TokenKind::Limit => {
                    self.assign_from_body(name)
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

    /// Parse `name: <body>` where `<body>` went through [`Parser::parse_body`].
    /// In assignment position a one-column `select` is a *column expression*:
    /// it materialises to a list global rather than a named table. A bare
    /// one-column select (no `name:`) still prints as a table.
    fn assign_from_body(&mut self, name: String) -> Result<Stmt, QplError> {
        match self.parse_body()? {
            Stmt::SingleVar(expr) => Ok(Stmt::ScalarAssign { name, expr }),
            Stmt::RetTable(te) if is_column_expr(&te) => {
                Ok(Stmt::ScalarAssign { name, expr: Expr::Table(Box::new(te)) })
            }
            other => Ok(Stmt::Assign { name, body: Box::new(other) }),
        }
    }

    /// `{[p1,p2] stmt; stmt; last-expr}` — a function literal, folded straight
    /// into an `Expr::Lit(Value::Closure(..))`. The param list is optional
    /// (`{[] ..}` / `{ .. }` are niladic). Statements are `;`-separated, each a
    /// full `parse_stmt` (so locals may be assigned); the body must be
    /// non-empty and end in an expression (its return value).
    fn parse_func_lit(&mut self) -> Result<Expr, QplError> {
        self.eat(&TokenKind::LBrace)?;
        let mut params = Vec::new();
        if self.peek() == &TokenKind::LBracket {
            self.next();
            while let TokenKind::Name(p) = self.peek().clone() {
                self.next();
                check_not_reserved(&p)?;
                params.push(p);
                if self.peek() != &TokenKind::Comma {
                    break;
                }
                self.next();
            }
            self.eat(&TokenKind::RBracket)?;
        }
        let mut body = Vec::new();
        while self.peek() != &TokenKind::RBrace {
            // tolerate a stray/extra `;` (an empty statement) between real
            // ones, rather than trying to parse a statement starting at it
            // and failing with "Unexpected token in primary: Semicolon".
            if self.peek() == &TokenKind::Semicolon {
                self.next();
                continue;
            }
            if matches!(self.peek(), TokenKind::Eof) {
                return Err(QplError::Parse("unterminated function: missing '}'".into()));
            }
            body.push(self.parse_stmt()?);
            match self.peek() {
                TokenKind::Semicolon => {
                    self.next();
                }
                TokenKind::RBrace => break,
                other => {
                    return Err(QplError::Parse(format!(
                        "expected ';' or '}}' in function body, got {other:?}"
                    )));
                }
            }
        }
        self.eat(&TokenKind::RBrace)?;
        if body.is_empty() {
            return Err(QplError::Parse("function body cannot be empty".into()));
        }
        if !matches!(body.last(), Some(Stmt::SingleVar(_) | Stmt::RetTable(_))) {
            return Err(QplError::Parse(
                "a function body must end with an expression, not an assignment".into(),
            ));
        }
        Ok(Expr::Lit(Value::Closure(std::sync::Arc::new(crate::ast::Function { params, body }))))
    }

    fn parse_body(&mut self) -> Result<Stmt, QplError> {
        // `<name> sink <path>` — sink a table referenced by name.
        if matches!(self.peek(), TokenKind::Name(_)) && self.peek2() == &TokenKind::Sink {
            let name = match self.next() {
                TokenKind::Name(n) => n,
                _ => unreachable!(),
            };
            self.next(); // `sink`
            let path = self.parse_expr()?;
            return Ok(Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Sink {
                src: Box::new(table_ref(name)),
                path,
            })));
        }

        // A leading `Int` only starts a *table* statement for `n limit …`;
        // `n#…` is a take/slice value expression handled by `parse_scalar_stmt`.
        let leading_int = matches!(self.peek(), TokenKind::Int(_));
        let int_table = leading_int && self.peek2() == &TokenKind::Limit;
        // `-n limit <table-expr>` — same, with a leading unary minus (tail).
        let leading_neg_int =
            matches!(self.peek(), TokenKind::Op(op) if op == "-") && matches!(self.peek2(), TokenKind::Int(_));
        let neg_int_table = leading_neg_int && self.peek3() == &TokenKind::Limit;
        // `<name> limit <table-expr>` — same, with a bound global as the count.
        let name_table_limit =
            matches!(self.peek(), TokenKind::Name(_)) && self.peek2() == &TokenKind::Limit;
        // `\`c!01b <tbl>` / `\`a\`b drop <tbl>` — a table op keyed off a leading
        // symbol. A bare `\`x` is a symbol value, not a table.
        let sym_table_op = !self.is_whopen_modifier()
            && matches!(self.peek(), TokenKind::Symbol(_) | TokenKind::SymbolVec(_))
            && (matches!(self.peek2(), TokenKind::Bang | TokenKind::Drop | TokenKind::DropNull)
                || matches!(self.peek2(), TokenKind::Name(n) if n == "_"));
        if (is_table_expr_start(self.peek()) && !leading_int) || int_table || neg_int_table
            || name_table_limit || sym_table_op
        {
            let tbl_expr = self.parse_table_expr()?;
            // postfix sink: `<table-expr> sink <path>`
            if matches!(self.peek(), TokenKind::Sink) {
                self.next(); // consume `sink`
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
        Ok(Stmt::SingleVar(self.parse_expr()?))
    }

    fn parse_table_expr(&mut self) -> Result<TableExpr, QplError> {
        // `<count> limit|# <table-expr>` — first/last `n` rows. `<count>` is
        // any primary-level scalar expression (a literal, a bound global, a
        // parenthesised expression, …), speculatively parsed and backtracked
        // out if no `limit`/`#` follows (so e.g. a bare `select …` or table
        // name falls through to the match below untouched).
        if let Some(te) = self.try_parse_table_limit()? {
            return Ok(te);
        }
        let peek = self.peek().clone();
        match peek {
            // `(<table-expr>)` — the parens give the parser an explicit end
            // point, which is what lets a join's right side (see `parse_join`)
            // hold an arbitrary table expression without the trailing
            // right_on symbols being ambiguous with a nested select's own join.
            TokenKind::LParen => {
                self.next();
                let inner = self.parse_table_expr()?;
                self.eat(&TokenKind::RParen)?;
                Ok(inner)
            }
            TokenKind::Select => Ok(TableExpr::Select(self.parse_query(false, false)?)),
            TokenKind::Update => Ok(TableExpr::Select(self.parse_query(true, false)?)),
            TokenKind::Delete => Ok(TableExpr::Select(self.parse_query(false, true)?)),
            // a bare table name, e.g. `cols t`, `distinct t`, `\`c drop t`
            TokenKind::Name(n) => {
                self.next();
                Ok(table_ref(n))
            }
            TokenKind::Distinct => {
                self.next();
                Ok(TableExpr::BuiltIn(BuiltIn::Distinct(Box::new(self.parse_table_expr()?))))
            }
            TokenKind::Load => {
                // standalone: load "path" → select all from the file
                self.next();
                Ok(TableExpr::Source(TableSource::Load(Box::new(self.parse_load_path()?))))
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
                    // `\`tbl` / `\`tbl sink path` used to name a table — tables are
                    // referenced by name now, so this is a plain symbol value.
                    TokenKind::Eof | TokenKind::Sink => Err(QplError::Parse(format!(
                        "reference tables by name, not by symbol: write '{s}', not '`{s}'"
                    ))),
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
                    TokenKind::DropNull => {
                        self.next();
                        Ok(TableExpr::BuiltIn(BuiltIn::DropNull(vec![s], Box::new(self.parse_table_expr()?))))
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
                } else if matches!(peek, TokenKind::DropNull) {
                    self.next();
                    Ok(TableExpr::BuiltIn(BuiltIn::DropNull(v, Box::new(self.parse_table_expr()?))))
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
        let from = Box::new(self.parse_table_expr()?);
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
        let left = self.parse_noun()?;

        // `u8!`$expr` (physical-width categorical) / `name::`$expr` (enum) —
        // a modifier token between the type and the `` `$ `` cast operator
        if let Some(cast) = self.parse_modified_cast(&left, false)? {
            return self.finish_window(cast, windows);
        }

        // `` `w!hopen <addr> `` — a write-mode IPC connection handle (bare
        // `hopen` is read-only by default). Reuses the same bang-modifier
        // convention as `` u8!`$col ``/`` `c!01b t ``; `w` is the only
        // accepted modifier. Lowered to `Call { func: "whopen", .. }` so
        // `resolve::eval_value` needs no new AST node for it.
        if let (Expr::Sym(w), TokenKind::Bang) = (&left, self.peek()) {
            if w == "w" {
                self.next(); // consume '!'
                return match self.parse_expr_inner(windows)? {
                    Expr::Call { func, args } if func == "hopen" => {
                        self.finish_window(Expr::Call { func: "whopen".into(), args }, windows)
                    }
                    other => Err(QplError::Parse(format!(
                        "expected 'hopen' after `w!, got {other:?}"
                    ))),
                };
            }
        }

        // infix dyadic verbs: `<param> verb <expr>` (q-style). `param` is `left`;
        // the value is the rest of the expression. They bind tighter than `over`,
        // so the value never swallows a window. The parser only records
        // `Call { func, args: [value, param] }`; `round` is lowered in the
        // compiler, the rest dispatch through `apply_dyadic` in the VM — except
        // `til`, a value-context list constructor handled by `resolve::eval_value`
        // before it would ever reach that column-context dispatch (so `args`
        // there means `[high, low]`, not `[column, param]`).
        if let TokenKind::Name(n) = self.peek()
            && matches!(n.as_str(),
                "round" | "quantile" | "pctl" | "shift" | "lag" | "lead" | "fill"
                | "diff" | "pctchange" | "til")
        {
            let name = n.clone();
            self.next();
            let value = match self.try_parse_table_operand()? {
                Some(e) => e,
                None => self.parse_value()?,
            };
            let call = Expr::Call { func: name, args: vec![value, left] };
            return self.finish_window(call, windows);
        }

        // bin op: left op right where right is the entire expr cos q is right to left eval
        if let TokenKind::Op(op) = self.peek().clone() {
            // cast: type$expr  e.g. f64$qty ;  `$expr  casts to a symbol / categorical
            if op == "$" {
                let target = cast_target(&left)?;
                self.next();
                let expr = match self.try_parse_table_operand()? {
                    Some(e) => e,
                    None => self.parse_expr_inner(windows)?,
                };
                return Ok(Expr::Cast { target, expr: Box::new(expr) });
            }
            self.next();
            let right = self.parse_expr_inner(windows)?;
            return Ok(binop(left, op, right));
        }
        // `like`: q-glob match, a real binary operator like `=`/`<>` — just
        // spelled as a bareword rather than an `Op` token.
        if matches!(self.peek(), TokenKind::Name(n) if n == "like") {
            self.next();
            let right = self.parse_expr_inner(windows)?;
            return Ok(Expr::BinOp {
                left: Box::new(left),
                op: "like".into(),
                right: Box::new(right),
            });
        }
        // `<conn> dispatch <rest>` / `<conn> async dispatch <rest>` — the payload
        // is a whole statement (often a table expression, e.g. `select from t`),
        // not a scalar `Expr`, so it can't be parsed as a normal argument; instead
        // capture everything left in the token stream verbatim and reconstruct
        // its source text (`render_tokens`) for the server to tokenise/parse/eval
        // independently, exactly as if it were typed at that server's REPL.
        let is_async_dispatch = matches!(self.peek(), TokenKind::Name(n) if n == "async")
            && matches!(self.peek2(), TokenKind::Name(n) if n == "dispatch");
        if is_async_dispatch || matches!(self.peek(), TokenKind::Name(n) if n == "dispatch") {
            if is_async_dispatch {
                self.next(); // consume `async`
            }
            self.next(); // consume `dispatch`
            let command = render_tokens(&self.tokens[self.i..]);
            self.i = self.tokens.len();
            return Ok(Expr::Dispatch { conn: Box::new(left), command, is_async: is_async_dispatch });
        }
        // call: left(args)
        if let Expr::ColRef(name) = &left {
            if is_noun_start(self.peek()) {
                let name = name.clone();
                let arg = self.parse_value()?;
                let call = Expr::Call { func: name, args: vec![arg] };
                return self.finish_window(call, windows);
            }
            // `<verb> select … from …` / `<verb> distinct …` — a reduction over a
            // column expression, e.g. `first select price from trades`.
            if is_table_expr_start(self.peek()) {
                let name = name.clone();
                let arg = Expr::Table(Box::new(self.parse_table_expr()?));
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
        // trailing `rolling <n>` sub-clause turns the aggregate into a fixed-size
        // rolling window (`sum px over `sym order `ts asc rolling 3`).
        let rolling = self.parse_rolling_modifier()?;
        let win = Expr::Window { func: Box::new(left), partition, order, rolling };
        // `over` binds tighter than arithmetic: fold trailing binary operators.
        if let TokenKind::Op(op) = self.peek().clone()
            && op != "$"
        {
            self.next();
            let right = self.parse_expr_inner(true)?;
            return Ok(binop(win, op, right));
        }
        Ok(win)
    }

    /// Trailing `rolling <n>` window sub-clause (after the `over` partition and
    /// any `order`). Returns `None` when there is no `rolling` keyword.
    fn parse_rolling_modifier(&mut self) -> Result<Option<usize>, QplError> {
        if !matches!(self.peek(), TokenKind::Name(n) if n == "rolling") {
            return Ok(None);
        }
        self.next(); // `rolling`
        match self.next() {
            TokenKind::Int(n) if n > 0 => Ok(Some(n as usize)),
            other => Err(QplError::Parse(format!(
                "`rolling` needs a positive integer window size, got {other:?}"
            ))),
        }
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
    /// in a `log` argument list `a b` is two items, not `a(b)`. Binary ops,
    /// casts, and noun-level postfixes (`` name`col ``, `f[a;b]` bracket
    /// application/indexing, `list where pred`, …) still compose — go through
    /// `parse_noun` rather than `parse_primary` directly, so only bareword
    /// juxtaposition-as-a-call (`f x`, handled in `parse_expr_inner`) is
    /// excluded; wrap that form of a call in parens instead.
    fn parse_expr_no_call(&mut self) -> Result<Expr, QplError> {
        let left = self.parse_noun()?;
        if let Some(cast) = self.parse_modified_cast(&left, true)? {
            return Ok(cast);
        }
        if let TokenKind::Op(op) = self.peek().clone() {
            if op == "$" {
                let target = cast_target(&left)?;
                self.next();
                let expr = match self.try_parse_table_operand()? {
                    Some(e) => e,
                    None => self.parse_expr_no_call()?,
                };
                return Ok(Expr::Cast { target, expr: Box::new(expr) });
            }
            self.next();
            let right = self.parse_expr_no_call()?;
            return Ok(binop(left, op, right));
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

    /// `while[test; s1; ...; sn]` (the `while` already consumed, sat on `[`).
    /// Every slot is a full statement, so the body may assign; the test must be
    /// an expression.
    fn parse_while(&mut self) -> Result<Expr, QplError> {
        self.eat(&TokenKind::LBracket)?;
        let mut slots = Vec::new();
        loop {
            if matches!(self.peek(), TokenKind::Semicolon | TokenKind::RBracket) {
                return Err(QplError::Parse("empty slot in `while[..]` — use noop".into()));
            }
            slots.push(self.parse_stmt()?);
            if self.peek() != &TokenKind::Semicolon {
                break;
            }
            self.next();
        }
        self.eat(&TokenKind::RBracket)?;
        if slots.len() < 2 {
            return Err(QplError::Parse("`while[..]` requires a test and at least one statement".into()));
        }
        let mut slots = slots.into_iter();
        let cond = match slots.next() {
            Some(Stmt::SingleVar(e)) => Box::new(e),
            _ => return Err(QplError::Parse("a `while` test must be an expression, not an assignment or table statement".into())),
        };
        Ok(Expr::While { cond, body: slots.collect() })
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
        // a bare name is only a plain table reference when it's not actually
        // the leading count of `<name>#…` / `<name> limit …` (`parse_table_expr`
        // handles that generally, via `try_parse_table_limit`).
        if let TokenKind::Name(name) = self.peek().clone()
            && !matches!(self.peek2(), TokenKind::Hash | TokenKind::Limit)
        {
            self.next();
            return Ok(table_ref(name));
        }
        self.parse_table_expr()
    }

    fn parse_tbl_src_expr(&mut self) -> Result<TableSource, QplError> {
        match self.next() {
            TokenKind::Name(name) => Ok(TableSource::InMem(name)),
            TokenKind::Load => Ok(TableSource::Load(Box::new(self.parse_load_path()?))),
            other => Err(QplError::Parse(format!("expected table name or load expression, got {other:?}"))),
        }
    }

    /// `load`'s path: a string literal, or a bound scalar global (resolved to
    /// a path string at run time). Deliberately just one token, not a general
    /// `parse_expr()` — `parse_tbl_src_expr`'s join-right-hand-side caller
    /// needs to stop here so a trailing `` `sym `` join key isn't swallowed
    /// into the path expression.
    fn parse_load_path(&mut self) -> Result<Expr, QplError> {
        match self.next() {
            TokenKind::Str(path) => Ok(Expr::Lit(Value::Str(path))),
            TokenKind::Name(name) => Ok(Expr::ColRef(name)),
            other => Err(QplError::Parse(format!(
                "expected a file path string or variable after 'load', got {other:?}"
            ))),
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
    fn parse_join(&mut self) -> Result<(Box<TableExpr>, Value, Value, JoinType), QplError> {
        let mut left_on = Vec::new();
        let mut right_on = Vec::new();
        while matches!(self.peek(), TokenKind::Symbol(_)) {
            if let TokenKind::Symbol(s) = self.next() {
                left_on.push(s);
            } else {
                unreachable!()
            }
        }
        let left_on = crate::ast::sym_vec(left_on);
        let join_type = match self.next() {
            TokenKind::Name(n) => match n.as_str() {
                "lj" => JoinType::Left,
                "ij" => JoinType::Inner,
                "rj" => JoinType::Right,
                other => return Err(QplError::Parse(format!("expected join type (lj|ij|rj), got {other}"))),
            },
            other => return Err(QplError::Parse(format!("expected join type (lj|ij|rj), got {:?}", other))),
        };
        // unparenthesised: a bare name or `load "path"`, same as always — the
        // trailing right_on symbols would otherwise be ambiguous with a nested
        // select's own join. Wrap it in parens — `(select ...)` — to join
        // against any other table expression.
        let join_src = if self.peek() == &TokenKind::LParen {
            Box::new(self.parse_table_expr()?)
        } else {
            Box::new(TableExpr::Source(self.parse_tbl_src_expr()?))
        };
        while matches!(self.peek(), TokenKind::Symbol(_)) {
            if let TokenKind::Symbol(s) = self.next() {
                right_on.push(s);
            } else {
                unreachable!()
            }
        }
        let right_on = crate::ast::sym_vec(right_on);
        Ok((join_src, left_on, right_on, join_type))
    }


    /// A "noun": a primary plus the value-context postfixes that bind tightest —
    /// `` name`col `` / `` name`c1`c2 `` table references and positional indexing
    /// (`(expr) 2 3`). Everything downstream (`parse_expr_inner`) sees the result
    /// as an opaque operand.
    fn parse_noun(&mut self) -> Result<Expr, QplError> {
        // `<n>#<operand>` — take / slice. Caught before `parse_primary` so the
        // leading int is not read as a literal.
        if let Some(take) = self.try_parse_take()? {
            return Ok(take);
        }
        let mut e = self.parse_primary()?;
        // did `parse_primary` just close a parenthesised group? `(x) 2 3` indexes
        // even when `x` is a bare name (`x 2 3` on its own is a call).
        let parenthesised = self.i > 0 && self.tokens[self.i - 1].kind == TokenKind::RParen;

        // `` name`col `` / `` name`c1`c2 `` — a column / table expression.
        // Skipped when the backtick vector is immediately followed by `!`:
        // that's `` <name> `k1`k2!v1 v2 `` — `name` is a call target (e.g.
        // `zip`) and the backtick vector is that call's dict-literal
        // argument, not a table-column reference. `name` is left as a bare
        // `ColRef` so the "call: left(args)" juxtaposition below picks it up.
        let dict_arg_follows = matches!(self.peek(), TokenKind::Symbol(_) | TokenKind::SymbolVec(_))
            && self.peek2() == &TokenKind::Bang;
        if let Expr::ColRef(name) = &e && !dict_arg_follows {
            match self.peek().clone() {
                TokenKind::Symbol(s) => {
                    let name = name.clone();
                    self.next();
                    e = self.finish_table_ref(name, vec![Alias { name: None, expr: Expr::ColRef(s) }])?;
                }
                TokenKind::SymbolVec(v) => {
                    let name = name.clone();
                    self.next();
                    let cols = v.into_iter().map(|s| Alias { name: None, expr: Expr::ColRef(s) }).collect();
                    e = self.finish_table_ref(name, cols)?;
                }
                _ => {}
            }
        }

        // `` `k1`k2!v1 v2 `` / `` `k!v `` — a dict literal: a symbol (vector)
        // key immediately followed by `!`, then one value noun per key. `` `w! ``
        // is excluded here (even though it's a single-symbol key like any
        // other) so it falls through to the whopen bang-modifier handled in
        // `parse_expr_inner` — see the comment there.
        let dict_keys: Option<Vec<String>> = match &e {
            Expr::Sym(s) if s != "w" => Some(vec![s.clone()]),
            Expr::Lit(v) if matches!(v.as_vec(), Some((VecKind::Sym, _))) => {
                Some(v.vec_strings().map_err(QplError::Parse)?)
            }
            _ => None,
        };
        if let Some(keys) = dict_keys && self.peek() == &TokenKind::Bang {
            self.next();
            let mut pairs = Vec::with_capacity(keys.len());
            for key in keys {
                pairs.push((key, self.parse_noun()?));
            }
            e = Expr::Dict(pairs);
        }

        // positional index. Two forms, both chainable:
        //   `<list>[<i>]` / `<list>[<i j k>]`  — bracket index, works on a bare name
        //   `(<expr>) 2 3`                     — juxtaposed int run, not after a
        //                                        bare name (that stays a call site)
        loop {
            if matches!(self.peek(), TokenKind::LBracket) {
                // `log[...]` — the bracket-scoped spelling of the bareword
                // `log a b c` stdout-write, so it can be delimited inside a
                // larger expression or a function body instead of always
                // running to the end of the line. Parsed like `parse_expr_seq`
                // (juxtaposed items, each a full `parse_expr_no_call`, `;`
                // between them optional) rather than the generic `f[a;b]`
                // call grammar below, which requires a separator and would
                // reject `log[str$.qpl.ts " - INFO " s]` after its first item.
                if matches!(&e, Expr::ColRef(n) if n == "log") {
                    self.next(); // `[`
                    let mut args = Vec::new();
                    while self.peek() != &TokenKind::RBracket {
                        args.push(self.with_str_runs(false, |p| p.parse_expr_no_call())?);
                        if self.peek() == &TokenKind::Semicolon {
                            self.next();
                        }
                    }
                    self.eat(&TokenKind::RBracket)?;
                    e = Expr::Call { func: "log".into(), args };
                    continue;
                }
                self.next(); // `[`
                // `f[]` / `f[a;b]` → `Expr::Apply`; a single expression with no
                // `;` stays `Expr::Index` (list index, or a monadic function
                // call resolved at run time).
                if self.peek() == &TokenKind::RBracket {
                    self.next();
                    e = Expr::Apply { func: Box::new(e), args: vec![] };
                    continue;
                }
                let mut args = vec![self.parse_call_arg()?];
                let mut multi = false;
                while self.peek() == &TokenKind::Semicolon {
                    multi = true;
                    self.next();
                    args.push(self.parse_call_arg()?);
                }
                self.eat(&TokenKind::RBracket)?;
                e = if multi {
                    Expr::Apply { func: Box::new(e), args }
                } else {
                    Expr::Index { expr: Box::new(e), idx: Box::new(args.pop().unwrap()) }
                };
                continue;
            }
            if parenthesised || !matches!(e, Expr::ColRef(_)) {
                if let Some(idx) = self.try_parse_int_run() {
                    e = Expr::Index { expr: Box::new(e), idx: Box::new(idx) };
                    continue;
                }
            }
            break;
        }

        // `<list-expr> where <predicate>[, <predicate>...]` — elementwise
        // filter on a list value, predicates written against `x` (a plain
        // column reference into the list's own materialisation). Distinct
        // from the `` name`col `` + `where` sugar in `finish_table_ref`,
        // which is already fully consumed by the time we get here, so a
        // single `where` is never double-handled. A *second* chained `where`
        // (`` t`price where size>100 where x>50 ``) is not reliably
        // supported: with no operator precedence in this grammar, the
        // trailing `where` attaches to whichever noun it immediately follows
        // inside the first predicate, not necessarily to the outer clause —
        // parenthesise instead: `` (t`price where size>100) where x>50 ``.
        if self.peek() == &TokenKind::Where {
            self.next();
            let where_ = self.parse_where()?.expect("parse_where always returns Some");
            e = Expr::ListWhere { list: Box::new(e), where_ };
        }
        Ok(e)
    }

    /// `` name`col … `` — build the one/many-column select and fold a trailing
    /// `where` (only valid in this sugar) into it.
    fn finish_table_ref(&mut self, table: String, cols: Vec<Alias>) -> Result<Expr, QplError> {
        let where_ = if self.peek() == &TokenKind::Where {
            self.next();
            self.parse_where()?
        } else {
            None
        };
        Ok(Expr::Table(Box::new(TableExpr::Select(SelectStmt {
            cols,
            from: Box::new(TableExpr::Source(TableSource::InMem(table))),
            by: None,
            where_,
            order: None,
            join: None,
            update: false,
            delete: false,
        }))))
    }

    /// `<count> limit|# <table-expr>` — returns `None` when this doesn't turn
    /// out to be that shape (restoring the parser position exactly). See
    /// [`Parser::try_parse_take`] for the value-context sibling of this.
    fn try_parse_table_limit(&mut self) -> Result<Option<TableExpr>, QplError> {
        let checkpoint = self.i;
        let count = match self.parse_primary() {
            Ok(count) => count,
            Err(_) => {
                self.i = checkpoint;
                return Ok(None);
            }
        };
        if !matches!(self.peek(), TokenKind::Limit | TokenKind::Hash) {
            self.i = checkpoint;
            return Ok(None);
        }
        self.next(); // `limit` / `#`
        Ok(Some(TableExpr::BuiltIn(BuiltIn::Limit(Box::new(self.parse_table_expr()?), count))))
    }

    /// `<count>#<operand>` — returns `None` when this doesn't turn out to be
    /// that shape. `<count>` is any primary-level scalar expression (a
    /// literal, a negative literal, a bound global, a parenthesised
    /// expression, …), not just a literal int — speculatively parsed and
    /// backtracked out if no `#` follows.
    fn try_parse_take(&mut self) -> Result<Option<Expr>, QplError> {
        let checkpoint = self.i;
        let n = match self.parse_primary() {
            Ok(n) => n,
            Err(_) => {
                self.i = checkpoint;
                return Ok(None);
            }
        };
        if self.peek() != &TokenKind::Hash {
            self.i = checkpoint;
            return Ok(None);
        }
        self.next(); // `#`
        Ok(Some(Expr::Take { n: Box::new(n), expr: Box::new(self.parse_take_operand()?) }))
    }

    /// If the next token starts a table expression (`select …`, `collect …`,
    /// `lazy …`, …), parse the whole thing as one, wrapped in `Expr::Table` —
    /// `resolve::eval_value` already applies a cast, or hands a frame to a
    /// called function's parameter, exactly like it does for a bare
    /// `` trades`price `` or `f[t]` with `t` a table name; the only thing
    /// missing was a parser path to *reach* those, since none of these
    /// keywords are a valid `parse_primary`. Used for a cast's RHS
    /// (`` `date$select ts from t ``) and for a bracket-call argument
    /// (`f[lazy load "x.csv"]`). Returns `None` for an ordinary scalar/noun
    /// operand, so the caller falls back to its normal expression parse.
    fn try_parse_table_operand(&mut self) -> Result<Option<Expr>, QplError> {
        if matches!(self.peek(),
            TokenKind::Select | TokenKind::Update | TokenKind::Delete
            | TokenKind::Distinct | TokenKind::Cols | TokenKind::Load
            | TokenKind::Lazy | TokenKind::Collect)
        {
            return Ok(Some(Expr::Table(Box::new(self.parse_table_expr()?))));
        }
        Ok(None)
    }

    /// One argument inside `f[..]` / `f[a;b;..]`: a table expression when one
    /// starts here (see `try_parse_table_operand`), otherwise an ordinary
    /// expression.
    fn parse_call_arg(&mut self) -> Result<Expr, QplError> {
        self.with_str_runs(true, |p| match p.try_parse_table_operand()? {
            Some(e) => Ok(e),
            None => p.parse_expr(),
        })
    }

    /// Runs `f` with string-run folding set to `on`, restoring the previous
    /// setting afterwards (also on error) — a bracket or paren group is its
    /// own context, so `log ("a" "b")` / `log[f["a" "b"]]` fold even though the
    /// enclosing `log` argument list doesn't.
    fn with_str_runs<T>(&mut self, on: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = std::mem::replace(&mut self.str_runs, on);
        let out = f(self);
        self.str_runs = outer;
        out
    }

    /// Operand of `<n>#…`: a table expression (`select …`, `` `tbl ``) or a noun
    /// (`` name`col ``, `(expr)`, a bare name / list global).
    fn parse_take_operand(&mut self) -> Result<Expr, QplError> {
        if matches!(self.peek(),
            TokenKind::Select | TokenKind::Update | TokenKind::Delete
            | TokenKind::Distinct | TokenKind::Cols | TokenKind::Load
            | TokenKind::Symbol(_) | TokenKind::SymbolVec(_))
        {
            return Ok(Expr::Table(Box::new(self.parse_table_expr()?)));
        }
        self.parse_noun()
    }

    /// A run of one or more consecutive `Int` tokens → `Int` / `IntVec` literal.
    fn try_parse_int_run(&mut self) -> Option<Expr> {
        if !matches!(self.peek(), TokenKind::Int(_)) {
            return None;
        }
        let mut ns = Vec::new();
        while let TokenKind::Int(n) = self.peek() {
            ns.push(*n);
            self.next();
        }
        Some(if ns.len() == 1 {
            Expr::Lit(Value::Int(ns[0]))
        } else {
            Expr::Lit(crate::ast::int_vec(ns))
        })
    }

    fn parse_primary(&mut self) -> Result<Expr, QplError> {
        // `{[..] ..}` anywhere an expression is expected — a function literal,
        // a first-class value like any other (see `Value::Closure`).
        if self.peek() == &TokenKind::LBrace {
            return self.parse_func_lit();
        }
        // a run of ints juxtaposed with no operator is an int-vector literal
        if matches!(self.peek(), TokenKind::Int(_)) && matches!(self.peek2(), TokenKind::Int(_)) {
            return Ok(self.try_parse_int_run().unwrap());
        }
        match self.next() {
            TokenKind::Int(n)      => Ok(Expr::Lit(Value::Int(n))),
            TokenKind::Float(n)    => Ok(Expr::Lit(Value::Float(n))),
            // a run of juxtaposed strings is a string-vector literal: `"a" "b"`
            TokenKind::Str(s) if self.str_runs && matches!(self.peek(), TokenKind::Str(_)) => {
                let mut v = vec![s];
                while let TokenKind::Str(next) = self.peek() {
                    v.push(next.clone());
                    self.next();
                }
                Ok(Expr::Lit(crate::ast::str_vec(v)))
            }
            TokenKind::Str(s)      => Ok(Expr::Lit(Value::Str(s))),
            TokenKind::Bool(b)     => Ok(Expr::Lit(Value::Bool(b))),
            TokenKind::BoolVec(v)  => Ok(Expr::Lit(crate::ast::bool_vec(v))),
            TokenKind::SymbolVec(v)=> Ok(Expr::Lit(crate::ast::sym_vec(v))),
            TokenKind::Symbol(s)   => Ok(Expr::Sym(s)),
            TokenKind::Temporal(v) => Ok(Expr::Lit(v)),
            TokenKind::Name(n) if n == "i" => Ok(Expr::IColRef),
            // `enlist <value>` — the one-element list of an atom. A literal
            // folds here; anything else is applied at run time (`resolve::eval_value`).
            TokenKind::Name(n) if n == "enlist" => Ok(enlist(self.parse_value()?)),
            // in expression position `distinct` is the column verb (alias of
            // `n_unique`); in table position `parse_table_expr` claims it first
            TokenKind::Distinct => Ok(Expr::ColRef("distinct".into())),
            TokenKind::Name(n) if n == "noop" => Ok(Expr::Noop),
            TokenKind::Name(n) if n == "while" && self.peek() == &TokenKind::LBracket => self.parse_while(),
            TokenKind::Name(n) if n == "while" => Err(QplError::Parse("'while' is a reserved word".into())),
            // Every other bare name — including `.qpl.dt`/`.qpl.tm`/`.qpl.ts`/`.qpl.dlta`
            // and any other namespaced name — is an ordinary variable/table/function
            // reference, resolved by lookup (see `Vm::lookup`, `resolve::call_niladic`).
            TokenKind::Name(n)     => Ok(Expr::ColRef(n)),
            TokenKind::Op(op) if op == "?" => self.parse_case(),
            // leading `-`: a negative literal (`-45.3`) or unary negation of the
            // next primary, lowered to `0 - x` so it composes like any `-`
            TokenKind::Op(op) if op == "-" => {
                let rhs = self.parse_primary()?;
                Ok(negate(rhs))
            }
            TokenKind::LParen      => {
                let expr = self.with_str_runs(true, |p| p.parse_expr())?;
                self.eat(&TokenKind::RParen)?;
                Ok(expr)
            },

            other => Err(QplError::Parse(format!("Unexpected token in primary: {:?}", other))),
        }
    }
}

/// `<left> <op> <right>`. `?` (roll: `3?6`, `2?10 20 30`) isn't a scalar
/// operator, so it lowers to `Call { func: "?", args: [right, left] }` —
/// value context only, evaluated by `resolve::eval_value` like `til`.
fn binop(left: Expr, op: String, right: Expr) -> Expr {
    if op == "?" {
        return Expr::Call { func: op, args: vec![right, left] };
    }
    Expr::BinOp { left: Box::new(left), op, right: Box::new(right) }
}

/// `enlist <operand>`: a literal atom folds to the one-element vector
/// straight away (so it works anywhere a literal does, e.g. in a `where`);
/// anything else defers to run time as `Call { func: "enlist", .. }`.
fn enlist(operand: Expr) -> Expr {
    let folded = match &operand {
        Expr::Lit(v) => v.enlist(),
        Expr::Sym(s) => Some(crate::ast::sym_vec(vec![s.clone()])),
        _ => None,
    };
    match folded {
        Some(v) => Expr::Lit(v),
        None => Expr::Call { func: "enlist".into(), args: vec![operand] },
    }
}

/// Applies a leading unary minus: folds a numeric literal in place, otherwise
/// lowers to `0 - expr` so it reuses the existing subtraction path everywhere
/// (scalar fold, column expr, filter).
fn negate(e: Expr) -> Expr {
    let flip = |v: Value| match v {
        Value::Int(n)       => Some(Value::Int(-n)),
        Value::Float(f)     => Some(Value::Float(-f)),
        // temporal literals negate their integer offset (kdb treats them as ints)
        Value::Date(n)      => Some(Value::Date(-n)),
        Value::Month(n)     => Some(Value::Month(-n)),
        Value::Minute(n)    => Some(Value::Minute(-n)),
        Value::Second(n)    => Some(Value::Second(-n)),
        Value::Time(n)      => Some(Value::Time(-n)),
        Value::Timestamp(n) => Some(Value::Timestamp(-n)),
        Value::Timespan(n)  => Some(Value::Timespan(-n)),
        _ => None,
    };
    match e {
        Expr::Lit(v) if flip(v.clone()).is_some() => Expr::Lit(flip(v).unwrap()),
        other => Expr::BinOp {
            left: Box::new(Expr::Lit(Value::Int(0))),
            op: "-".into(),
            right: Box::new(other),
        },
    }
}

/// A table expression that, used in a value context, is a *column expression*:
/// a single-column `select` with no `by` (it materialises to a list, not a table).
fn is_column_expr(te: &TableExpr) -> bool {
    matches!(te, TableExpr::Select(sel)
        if sel.cols.len() == 1 && sel.by.is_none() && !sel.update && !sel.delete)
}

pub fn parse(tokens: Vec<Token>) -> Result<Stmt, QplError> {
    let mut parser = Parser { tokens, i: 0, str_runs: true };
    let stmt = parser.parse_stmt()?;
    parser.eat(&TokenKind::Eof)?;
    Ok(stmt)
}

/// Parse one or more juxtaposed expressions (space-separated), consuming every
/// token. Used by the `log` stdout-write, which evaluates each as a scalar
/// and concatenates the rendered values. Top-level juxtaposition separates
/// items rather than forming a call — see [`Parser::parse_expr_no_call`].
pub fn parse_expr_seq(tokens: Vec<Token>) -> Result<Vec<Expr>, QplError> {
    let mut parser = Parser { tokens, i: 0, str_runs: false };
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
        // `` `date$x `` / `` `int$d `` — a named type before `` `$ ``
        Expr::Sym(s) => Ok(CastTarget::Prim(s.clone())),
        // `"p"$"…"` — a kdb single-char type code (or full name) before `$`
        Expr::Lit(Value::Str(s)) => temporal_type_from_code(s)
            .map(|t| CastTarget::Prim(t.into()))
            .ok_or_else(|| QplError::Parse(format!("unknown cast type code {s:?}"))),
        _ => Err(QplError::Parse(format!("expected type name before '$', got {left:?}"))),
    }
}

/// kdb single-char temporal type codes (and their full names) accepted as a
/// cast target, e.g. `"p"$"2024.03.15D…"`.
fn temporal_type_from_code(s: &str) -> Option<&'static str> {
    Some(match s {
        "d" | "date"      => "date",
        "m" | "month"     => "month",
        "t" | "time"      => "time",
        "u" | "minute"    => "minute",
        "v" | "second"    => "second",
        "p" | "timestamp" => "timestamp",
        "n" | "timespan"  => "timespan",
        _ => return None,
    })
}

fn is_noun_start(token: &TokenKind) -> bool {
    matches!(token,
        TokenKind::Name(_)
        | TokenKind::Int(_)
        | TokenKind::Float(_)
        | TokenKind::Str(_)
        | TokenKind::Bool(_)
        | TokenKind::Symbol(_)
        | TokenKind::SymbolVec(_)
        | TokenKind::BoolVec(_)
        | TokenKind::Temporal(_)
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
    )
}

/// A `SelectStmt` that reads a whole in-memory table by name.
fn table_ref(name: String) -> TableExpr {
    TableExpr::Source(TableSource::InMem(name))
}

/// Reconstructs source text from a token slice — the inverse of the lexer,
/// used by `dispatch` to ship the rest of a statement to another process as
/// plain text (see `Expr::Dispatch`). The lexer is whitespace-insensitive
/// around punctuation, so joining every rendered token with a single space is
/// always safe: the result doesn't have to be byte-identical to what the user
/// typed, only re-tokenise/re-parse to the same AST.
fn render_tokens(tokens: &[Token]) -> String {
    let mut parts = Vec::with_capacity(tokens.len());
    for t in tokens {
        let s = match &t.kind {
            TokenKind::Select => "select".to_string(),
            TokenKind::By => "by".to_string(),
            TokenKind::From => "from".to_string(),
            TokenKind::Where => "where".to_string(),
            TokenKind::Over => "over".to_string(),
            TokenKind::Order => "order".to_string(),
            TokenKind::Asc => "asc".to_string(),
            TokenKind::Desc => "desc".to_string(),
            TokenKind::Distinct => "distinct".to_string(),
            TokenKind::DropNull => "dropnull".to_string(),
            TokenKind::Limit => "limit".to_string(),
            TokenKind::Drop => "drop".to_string(),
            TokenKind::Update => "update".to_string(),
            TokenKind::Delete => "delete".to_string(),
            TokenKind::Load => "load".to_string(),
            TokenKind::Sink => "sink".to_string(),
            TokenKind::Cols => "cols".to_string(),
            TokenKind::Lazy => "lazy".to_string(),
            TokenKind::Collect => "collect".to_string(),
            TokenKind::Name(n) => n.clone(),
            TokenKind::Int(n) => n.to_string(),
            TokenKind::Float(f) => f.to_string(),
            TokenKind::Symbol(s) => format!("`{s}"),
            TokenKind::SymbolVec(v) => v.iter().map(|s| format!("`{s}")).collect(),
            TokenKind::Bool(b) => if *b { "1b".into() } else { "0b".into() },
            TokenKind::BoolVec(v) => {
                let bits: String = v.iter().map(|b| if *b { '1' } else { '0' }).collect();
                format!("{bits}b")
            }
            TokenKind::Str(s) => render_str_literal(s),
            TokenKind::Temporal(v) => crate::temporal::format_temporal(v)
                .unwrap_or_else(|| format!("{v:?}")),
            TokenKind::Colon => ":".to_string(),
            TokenKind::ColonColon => "::".to_string(),
            TokenKind::Comma => ",".to_string(),
            TokenKind::Semicolon => ";".to_string(),
            TokenKind::LParen => "(".to_string(),
            TokenKind::RParen => ")".to_string(),
            TokenKind::LBracket => "[".to_string(),
            TokenKind::RBracket => "]".to_string(),
            TokenKind::LBrace => "{".to_string(),
            TokenKind::RBrace => "}".to_string(),
            TokenKind::Bang => "!".to_string(),
            TokenKind::Hash => "#".to_string(),
            TokenKind::Op(op) => op.clone(),
            TokenKind::Eof => continue,
        };
        parts.push(s);
    }
    parts.join(" ")
}

fn render_str_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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
    fn expr_seq_str_run_needs_parens_to_be_a_vector() {
        // top level of a `log` argument list: juxtaposition means separate items
        assert_eq!(seq("\"a\" \"b\" \"c\"").len(), 3);
        // parenthesised: one string vector
        assert_eq!(seq("(\"a\" \"b\") \"c\""), vec![
            Expr::Lit(crate::ast::str_vec(vec!["a".into(), "b".into()])),
            Expr::Lit(Value::Str("c".into())),
        ]);
    }

    #[test]
    fn str_run_outside_log_is_a_str_vec() {
        assert_eq!(p("x: \"a\" \"b\""), Stmt::ScalarAssign {
            name: "x".into(),
            expr: Expr::Lit(crate::ast::str_vec(vec!["a".into(), "b".into()])),
        });
        assert_eq!(p("(\"a\" \"b\")"), Stmt::SingleVar(
            Expr::Lit(crate::ast::str_vec(vec!["a".into(), "b".into()]))));
    }

    #[test]
    fn enlist_literal_folds_to_a_vector_literal() {
        assert_eq!(p("enlist 23"), Stmt::SingleVar(Expr::Lit(crate::ast::int_vec(vec![23]))));
        assert_eq!(p("enlist \"s\""), Stmt::SingleVar(Expr::Lit(crate::ast::str_vec(vec!["s".into()]))));
        assert_eq!(p("enlist `s"), Stmt::SingleVar(Expr::Lit(crate::ast::sym_vec(vec!["s".into()]))));
    }

    #[test]
    fn enlist_of_a_name_defers_to_a_call() {
        assert_eq!(p("enlist n"), Stmt::SingleVar(Expr::Call {
            func: "enlist".into(),
            args: vec![Expr::ColRef("n".into())],
        }));
    }

    #[test]
    fn question_mark_infix_is_a_roll_call_with_the_list_first() {
        assert_eq!(p("3?6"), Stmt::SingleVar(Expr::Call {
            func: "?".into(),
            args: vec![Expr::Lit(Value::Int(6)), Expr::Lit(Value::Int(3))],
        }));
        assert_eq!(p("2 ? 10 20"), Stmt::SingleVar(Expr::Call {
            func: "?".into(),
            args: vec![Expr::Lit(crate::ast::int_vec(vec![10, 20])), Expr::Lit(Value::Int(2))],
        }));
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

    #[test]
    fn expr_seq_bracket_call_still_applies_without_parens() {
        // `f[a;b]`/`f[a]` is a noun-level postfix (unlike bareword `f a`
        // juxtaposition, which `parse_expr_no_call` deliberately treats as two
        // separate items) — so it applies even inside a `log` arg list.
        let got = seq("add[2;3]");
        assert_eq!(got, vec![Expr::Apply {
            func: Box::new(Expr::ColRef("add".into())),
            args: vec![Expr::Lit(Value::Int(2)), Expr::Lit(Value::Int(3))],
        }]);
    }

    #[test]
    fn bracket_call_arg_may_be_a_table_expression() {
        // regression: `f[lazy load out]` failed with "Unexpected token in
        // primary: Lazy" — a bracket-call argument only ever tried an
        // ordinary `parse_expr()`, which has no `parse_primary` case for
        // `select`/`lazy`/`collect`/etc. `resolve::eval_value` already hands
        // a frame to a called function's parameter fine (it's the same path
        // `` f[trades] `` uses) — this only needed a parser change.
        match p("f[lazy load out]") {
            Stmt::SingleVar(Expr::Index { idx, .. }) => {
                assert!(matches!(*idx, Expr::Table(_)), "{idx:?}");
            }
            other => panic!("expected an Index call, got {other:?}"),
        }
        match p("f[lazy load out; 2]") {
            Stmt::SingleVar(Expr::Apply { args, .. }) => {
                assert!(matches!(&args[0], Expr::Table(_)), "{:?}", args[0]);
                assert_eq!(args[1], Expr::Lit(Value::Int(2)));
            }
            other => panic!("expected an Apply call, got {other:?}"),
        }
    }

    #[test]
    fn log_bracket_call_parses_juxtaposed_items_like_a_bareword_log() {
        // no separator needed — same "space-separated items" grammar as the
        // bareword `log a b c` form, just delimited by `[..]` instead of
        // running to the end of the line.
        assert_eq!(
            p(r#"log["a" "b"]"#),
            Stmt::SingleVar(Expr::Call {
                func: "log".into(),
                args: vec![Expr::Lit(Value::Str("a".into())), Expr::Lit(Value::Str("b".into()))],
            })
        );
    }

    #[test]
    fn log_bracket_call_accepts_optional_semicolons() {
        assert_eq!(
            p(r#"log["a";"b";"c"]"#),
            Stmt::SingleVar(Expr::Call {
                func: "log".into(),
                args: vec![
                    Expr::Lit(Value::Str("a".into())),
                    Expr::Lit(Value::Str("b".into())),
                    Expr::Lit(Value::Str("c".into())),
                ],
            })
        );
    }

    #[test]
    fn log_bracket_call_empty_is_a_bare_log() {
        assert_eq!(p("log[]"), Stmt::SingleVar(Expr::Call { func: "log".into(), args: vec![] }));
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
        assert_eq!(*s.from, TableExpr::Source(TableSource::InMem("trades".into())));
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
        assert_eq!(*s.from, TableExpr::Source(TableSource::InMem("t".into())));
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
    fn where_like() {
        let s = sel(r#"select px from trades where sym like "AA*""#);
        assert_eq!(s.where_, Some(vec![binop(cref("sym"), "like", Expr::Lit(Value::Str("AA*".into())))]));
    }

    #[test]
    fn dispatch_captures_the_rest_of_the_statement_as_text() {
        match p("conn dispatch select from t where price > 100") {
            Stmt::SingleVar(Expr::Dispatch { conn, command, is_async }) => {
                assert_eq!(*conn, Expr::ColRef("conn".into()));
                assert_eq!(command, "select from t where price > 100");
                assert!(!is_async);
            }
            other => panic!("expected dispatch, got {other:?}"),
        }
    }

    #[test]
    fn async_dispatch_sets_the_async_flag() {
        match p("resp: conn async dispatch select from t") {
            Stmt::ScalarAssign { expr: Expr::Dispatch { command, is_async, .. }, .. } => {
                assert_eq!(command, "select from t");
                assert!(is_async);
            }
            other => panic!("expected async dispatch, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_command_round_trips_through_render_tokens() {
        // the rendered text isn't necessarily byte-identical to the input, but
        // it must re-tokenise/re-parse to the same AST the original would have.
        let src = r#"select sym, px: price from trades where sym like "AA*""#;
        let original = sel(src);
        match p(&format!("conn dispatch {src}")) {
            Stmt::SingleVar(Expr::Dispatch { command, .. }) => {
                let roundtripped = sel(&command);
                assert_eq!(roundtripped, original);
            }
            other => panic!("expected dispatch, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_string_literal_escaping_round_trips() {
        let src = r#"conn dispatch select from t where s = "a \"quoted\" str""#;
        match p(src) {
            Stmt::SingleVar(Expr::Dispatch { command, .. }) => {
                assert!(parse(tokenise(&command).unwrap()).is_ok());
                assert!(command.contains(r#"a \"quoted\" str"#));
            }
            other => panic!("expected dispatch, got {other:?}"),
        }
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
    fn dyadic_verb_accepts_a_table_operand() {
        match p("0 fill select b from t") {
            Stmt::SingleVar(Expr::Call { func, args }) => {
                assert_eq!(func, "fill");
                assert!(matches!(args[0], Expr::Table(_)));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn dropnull_select() {
        assert!(matches!(
            p("`price`qty dropnull select from trades"),
            Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::DropNull(cols, _))) if cols == vec!["price", "qty"]
        ));
    }

    #[test]
    fn distinct_select() {
        assert!(matches!(
            p("distinct select from trades"),
            Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Distinct(_)))
        ));
    }

    #[test]
    fn limit_keyword_is_a_table_op() {
        assert!(matches!(
            p("10 limit select from trades"),
            Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Limit(_, Expr::Lit(Value::Int(10)))))
        ));
    }

    #[test]
    fn negative_limit_keyword_is_a_tail_table_op() {
        for source in ["-10 limit select from trades", "-10 limit trades", "collect -10 limit trades"] {
            let stmt = p(source);
            let te = match &stmt {
                Stmt::RetTable(te) => te,
                _ => panic!("expected RetTable for '{source}', got {stmt:?}"),
            };
            // `collect` wraps the inner table expr; unwrap one level if present.
            let te = match te {
                TableExpr::BuiltIn(BuiltIn::Collect(inner)) => inner.as_ref(),
                other => other,
            };
            assert!(
                matches!(te, TableExpr::BuiltIn(BuiltIn::Limit(_, Expr::Lit(Value::Int(-10))))),
                "'{source}' -> {te:?}"
            );
        }
    }

    #[test]
    fn negative_hash_before_limit_in_named_assign() {
        assert!(matches!(
            p("t: -5 limit trades"),
            Stmt::Assign { body, .. } if matches!(*body, Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Limit(_, Expr::Lit(Value::Int(-5))))))
        ));
    }

    #[test]
    fn hash_take_is_a_value_expression() {
        // `n#…` is always a take/slice; the VM decides frame vs list at run time
        for source in ["10#trades", "10#select from trades", "10#trades`price", "-3#trades`price"] {
            assert!(matches!(p(source), Stmt::SingleVar(Expr::Take { .. })), "{source}");
        }
    }

    #[test]
    fn hash_take_count_need_not_be_a_literal() {
        // regression: only a literal (or negative-literal) int was accepted
        // before `#`; a bound global fell through to a parse error.
        for source in ["k#trades", "-k#trades", "(k+1)#trades"] {
            assert!(matches!(p(source), Stmt::SingleVar(Expr::Take { .. })), "{source}");
        }
    }

    #[test]
    fn collect_and_lazy_accept_a_variable_hash_count() {
        // regression: `collect k#t` / `collect (k#t)` errored ("expected
        // Eof/RParen, got Hash") because `parse_lazy_operand` and
        // `parse_table_expr` only recognised a *literal* int before `#`.
        for source in ["collect k#t", "collect (k#t)", "lazy k#t"] {
            let stmt = p(source);
            let te = match &stmt {
                Stmt::RetTable(te) => te,
                _ => panic!("expected RetTable for '{source}', got {stmt:?}"),
            };
            assert!(
                matches!(te, TableExpr::BuiltIn(BuiltIn::Collect(_)) | TableExpr::BuiltIn(BuiltIn::Lazy(_))),
                "'{source}' -> {te:?}"
            );
        }
    }

    #[test]
    fn limit_keyword_accepts_a_variable_count() {
        for source in ["k limit trades", "sample: k limit trades"] {
            let stmt = p(source);
            let te = match &stmt {
                Stmt::RetTable(te) => te,
                Stmt::Assign { body, .. } => match body.as_ref() {
                    Stmt::RetTable(te) => te,
                    other => panic!("expected RetTable body for '{source}', got {other:?}"),
                },
                other => panic!("expected a table statement for '{source}', got {other:?}"),
            };
            assert!(matches!(te, TableExpr::BuiltIn(BuiltIn::Limit(_, Expr::ColRef(n))) if n == "k"), "'{source}' -> {te:?}");
        }
    }

    #[test]
    fn index_forms_parse_to_expr_index() {
        for source in ["l[0]", "l[2 3 4]", "(l) 2 3", "trades`price[1]", "l[0][1]"] {
            assert!(matches!(p(source), Stmt::SingleVar(Expr::Index { .. })), "{source}");
        }
    }

    // --- functions ---

    /// The `Function` behind `p(source)`, which must be `name: {[..] ..}`.
    fn closure_of(source: &str) -> std::sync::Arc<crate::ast::Function> {
        match p(source) {
            Stmt::ScalarAssign { expr: Expr::Lit(Value::Closure(f)), .. } => f,
            other => panic!("expected a closure assignment, got {other:?}"),
        }
    }

    #[test]
    fn func_def_parses_to_a_closure_assignment() {
        match p("f: {[x,y] t: x*y; t+1}") {
            Stmt::ScalarAssign { name, expr: Expr::Lit(Value::Closure(f)) } => {
                assert_eq!(name, "f");
                assert_eq!(f.params, vec!["x".to_string(), "y".to_string()]);
                assert_eq!(f.body.len(), 2);
                assert!(matches!(f.body[0], Stmt::ScalarAssign { .. }));
                assert!(matches!(f.body[1], Stmt::SingleVar(_)));
            }
            other => panic!("expected a closure assignment, got {other:?}"),
        }
    }

    #[test]
    fn niladic_func_def_needs_no_param_list() {
        for source in ["f: {[] 42}", "f: {42}"] {
            assert!(closure_of(source).params.is_empty(), "{source}");
        }
    }

    #[test]
    fn a_func_literal_parses_in_expression_position() {
        // as a call argument (higher-order use) ...
        match p("apply[{[y] y*2}; 5]") {
            Stmt::SingleVar(Expr::Apply { args, .. }) => {
                assert!(matches!(args[0], Expr::Lit(Value::Closure(_))));
                assert!(matches!(args[1], Expr::Lit(Value::Int(5))));
            }
            other => panic!("expected an Apply, got {other:?}"),
        }
        // ... and as a bare expression
        assert!(matches!(p("{[x] x+1}"), Stmt::SingleVar(Expr::Lit(Value::Closure(_)))));
    }

    #[test]
    fn func_body_must_end_in_an_expression() {
        assert!(matches!(
            parse(tokenise("f: {[x] y: x+1}").unwrap()),
            Err(QplError::Parse(_)),
        ));
    }

    #[test]
    fn multi_arg_bracket_call_parses_to_apply() {
        match p("f[1;2;3]") {
            Stmt::SingleVar(Expr::Apply { func, args }) => {
                assert!(matches!(*func, Expr::ColRef(ref n) if n == "f"));
                assert_eq!(args.len(), 3);
            }
            other => panic!("expected Apply, got {other:?}"),
        }
        assert!(matches!(
            p("f[]"),
            Stmt::SingleVar(Expr::Apply { args, .. }) if args.is_empty()
        ));
    }

    #[test]
    fn single_arg_bracket_call_stays_index_for_runtime_dispatch() {
        assert!(matches!(p("f[`AAPL]"), Stmt::SingleVar(Expr::Index { .. })));
    }

    #[test]
    fn empty_function_body_is_a_parse_error() {
        assert!(parse(tokenise("f: {[x] }").unwrap()).is_err());
        assert!(parse(tokenise("{}").unwrap()).is_err());
    }

    #[test]
    fn reference_a_table_by_symbol_is_rejected() {
        // `\`name` is a plain symbol value or an outright parse error — never a table
        for source in ["`trades", "`trades sink \"out.parquet\"", "distinct `t", "3#`trades"] {
            let parsed = parse(tokenise(source).expect("lex"));
            assert!(
                !matches!(parsed, Ok(Stmt::RetTable(_))),
                "`{source}` should not resolve to a table, got {parsed:?}",
            );
        }
    }

    #[test]
    fn drop_single_symbol_keyword_and_shorthand() {
        for source in ["`price drop select from trades", "`price _ trades"] {
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
        match p("t: lazy load \"x.parquet\"") {
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
                            TableExpr::Source(TableSource::InMem(ref n)) if n == "t"
                        ));
                    }
                    other => panic!("expected collect body, got {other:?}"),
                }
            }
            other => panic!("expected collect assign, got {other:?}"),
        }
    }

    #[test]
    fn sink_takes_a_table_name_on_the_left_and_a_string_path() {
        match p("t sink \"out.parquet\"") {
            Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Sink { src, path })) => {
                assert!(matches!(
                    src.as_ref(),
                    TableExpr::Source(TableSource::InMem(n)) if n == "t"
                ));
                assert_eq!(path, Expr::Lit(Value::Str("out.parquet".into())));
            }
            other => panic!("expected sink, got {other:?}"),
        }
    }

    #[test]
    fn sink_accepts_a_full_select_on_the_left() {
        assert!(matches!(
            p("select price from trades sink \"out.parquet\""),
            Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Sink { src, .. }))
                if matches!(src.as_ref(), TableExpr::Select(_))
        ));
    }

    #[test]
    fn sink_rejects_a_symbol_on_the_left() {
        // tables are named, not symboled
        assert!(parse(tokenise("`t sink \"out.parquet\"").unwrap()).is_err());
    }

    #[test]
    fn load_standalone_takes_a_string_path() {
        match p("load \"x.parquet\"") {
            Stmt::RetTable(TableExpr::Source(TableSource::Load(path))) => {
                assert_eq!(*path, Expr::Lit(Value::Str("x.parquet".into())));
            }
            other => panic!("expected load, got {other:?}"),
        }
    }

    #[test]
    fn load_standalone_rejects_a_symbol_path() {
        assert!(parse(tokenise("load `x.parquet").unwrap()).is_err());
    }

    #[test]
    fn load_as_table_source_takes_a_string_path() {
        let s = sel("select price from load \"x.parquet\"");
        assert!(matches!(*s.from, TableExpr::Source(TableSource::Load(ref path))
            if **path == Expr::Lit(Value::Str("x.parquet".into()))));
    }

    #[test]
    fn load_accepts_a_variable_path() {
        // regression: `load out` (a bound scalar global) failed to parse —
        // `load` only ever accepted a literal string token.
        match p("load p") {
            Stmt::RetTable(TableExpr::Source(TableSource::Load(path))) => {
                assert_eq!(*path, Expr::ColRef("p".into()));
            }
            other => panic!("expected load, got {other:?}"),
        }
        let s = sel("select price from load p");
        assert!(matches!(*s.from, TableExpr::Source(TableSource::Load(ref path))
            if **path == Expr::ColRef("p".into())));
    }

    #[test]
    fn load_as_table_source_rejects_a_symbol_path() {
        assert!(parse(tokenise("select price from load `x.parquet").unwrap()).is_err());
    }

    #[test]
    fn from_accepts_a_nested_select() {
        let s = sel("select from select price from trades");
        assert!(matches!(
            *s.from,
            TableExpr::Select(SelectStmt { from: ref inner, .. })
                if matches!(**inner, TableExpr::Source(TableSource::InMem(ref n)) if n == "trades")
        ));
    }

    #[test]
    fn from_accepts_a_builtin_table_expr() {
        let s = sel("select from distinct trades");
        assert!(matches!(*s.from, TableExpr::BuiltIn(BuiltIn::Distinct(_))));
    }

    #[test]
    fn cols_accepts_a_nested_select() {
        match p("cols select from trades where price > 0") {
            Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Cols(inner))) => {
                assert!(matches!(*inner, TableExpr::Select(_)));
            }
            other => panic!("expected cols, got {other:?}"),
        }
    }

    #[test]
    fn join_right_side_without_parens_is_a_bare_source() {
        let s = sel("select price from trades `sym lj quotes `sym");
        let (join_src, ..) = s.join.expect("expected a join");
        assert!(matches!(*join_src, TableExpr::Source(TableSource::InMem(ref n)) if n == "quotes"));
    }

    #[test]
    fn join_right_side_without_parens_rejects_a_table_expr() {
        assert!(parse(tokenise("select price from trades `sym lj distinct quotes `sym").unwrap()).is_err());
    }

    #[test]
    fn join_right_side_accepts_a_parenthesised_table_expr() {
        let s = sel("select price from trades `sym lj (distinct quotes) `sym");
        let (join_src, ..) = s.join.expect("expected a join");
        assert!(matches!(*join_src, TableExpr::BuiltIn(BuiltIn::Distinct(_))));
    }

    #[test]
    fn join_right_side_accepts_a_parenthesised_nested_select() {
        let s = sel("select price from trades `sym lj (select sym, bid from quotes) `sym");
        let (join_src, ..) = s.join.expect("expected a join");
        assert!(matches!(*join_src, TableExpr::Select(_)));
    }

    #[test]
    fn angle_bracket_pairs_do_not_parse_as_load_or_sink() {
        assert!(parse(tokenise("t: << \"x.parquet\"").unwrap()).is_err());
        match p("t >> \"out.parquet\"") {
            Stmt::RetTable(TableExpr::BuiltIn(BuiltIn::Sink { .. })) => {
                panic!("`>>` should not be recognised as sink")
            }
            _ => {}
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
    fn temporal_literal_parses_to_a_lit() {
        match p("l: 2024.03.15") {
            Stmt::ScalarAssign { expr, .. } => assert_eq!(expr, Expr::Lit(Value::Date(8840))),
            other => panic!("expected scalar assign, got {other:?}"),
        }
    }

    #[test]
    fn qpl_now_function_parses_as_an_ordinary_variable_reference() {
        // `.qpl.ts` is a builtin (see `Vm::builtins`), but the parser doesn't
        // know that — it's a bare `ColRef` like any other name, resolved (and
        // auto-invoked, being niladic) by lookup at run time.
        match p("l: .qpl.ts") {
            Stmt::ScalarAssign { expr, .. } => assert_eq!(expr, Expr::ColRef(".qpl.ts".into())),
            other => panic!("expected scalar assign, got {other:?}"),
        }
    }

    #[test]
    fn namespaced_identifier_parses_as_an_ordinary_variable_reference() {
        // a general `.ns.name` is just a `ColRef` — resolved by name lookup
        // like any other identifier, not a zero-arg call.
        match p("l: .utils.helper") {
            Stmt::ScalarAssign { expr, .. } => assert_eq!(expr, Expr::ColRef(".utils.helper".into())),
            other => panic!("expected scalar assign, got {other:?}"),
        }
    }

    #[test]
    fn namespaced_identifier_composes_with_bareword_call_juxtaposition() {
        match p("l: .utils.helper 21") {
            Stmt::ScalarAssign { expr, .. } => assert_eq!(expr, Expr::Call {
                func: ".utils.helper".into(),
                args: vec![Expr::Lit(Value::Int(21))],
            }),
            other => panic!("expected scalar assign, got {other:?}"),
        }
    }

    #[test]
    fn backtick_and_string_temporal_cast_targets_parse() {
        // `` `date$x ``
        let s = sel("select c: `date$ts from t");
        assert!(matches!(
            &s.cols[0].expr,
            Expr::Cast { target: CastTarget::Prim(n), .. } if n == "date"
        ));
        // `"p"$"…"` — single-char kdb type code expands to the full name
        match p(r#"l: "p"$"2024.03.15D09:00:00""#) {
            Stmt::ScalarAssign { expr, .. } => assert!(matches!(
                expr,
                Expr::Cast { target: CastTarget::Prim(n), .. } if n == "timestamp"
            )),
            other => panic!("expected scalar assign, got {other:?}"),
        }
    }

    #[test]
    fn leading_minus_is_a_negative_literal() {
        match p("l: -45.3") {
            Stmt::ScalarAssign { expr, .. } => {
                assert_eq!(expr, Expr::Lit(Value::Float(-45.3)));
            }
            other => panic!("expected scalar assign, got {other:?}"),
        }
    }

    #[test]
    fn minus_on_a_name_lowers_to_zero_minus_expr() {
        match p("l: -y") {
            Stmt::ScalarAssign { expr, .. } => assert!(matches!(
                expr,
                Expr::BinOp { left, op, right }
                    if *left == Expr::Lit(Value::Int(0)) && op == "-"
                        && *right == Expr::ColRef("y".into())
            )),
            other => panic!("expected scalar assign, got {other:?}"),
        }
    }

    #[test]
    fn cast_of_a_negative_literal_parses() {
        // the lexer must not glom `$-` into one operator
        match p("l: int$-45.3") {
            Stmt::ScalarAssign { expr, .. } => assert!(matches!(
                expr,
                Expr::Cast { target: CastTarget::Prim(t), expr }
                    if t == "int" && *expr == Expr::Lit(Value::Float(-45.3))
            )),
            other => panic!("expected scalar assign, got {other:?}"),
        }
    }

    #[test]
    fn cast_of_a_select_statement_parses() {
        // regression: `` `date$select ts from t `` failed with "Unexpected
        // token in primary: Select" — a cast's RHS didn't know how to start a
        // table expression, even though `resolve::eval_value`'s `Expr::Cast`
        // arm already handles a frame/materialised-list operand fine.
        match p("d: `date$select ts from t where high = 20") {
            Stmt::ScalarAssign { expr: Expr::Cast { target: CastTarget::Prim(t), expr }, .. } => {
                assert_eq!(t, "date");
                assert!(matches!(*expr, Expr::Table(_)));
            }
            other => panic!("expected scalar assign with a cast, got {other:?}"),
        }
    }

    #[test]
    fn cast_of_a_collect_of_a_select_statement_parses() {
        match p("d: `date$collect select ts from t where high = 20") {
            Stmt::ScalarAssign { expr: Expr::Cast { target: CastTarget::Prim(t), expr }, .. } => {
                assert_eq!(t, "date");
                assert!(matches!(*expr, Expr::Table(_)));
            }
            other => panic!("expected scalar assign with a cast, got {other:?}"),
        }
    }

    #[test]
    fn bare_symbol_vector_is_a_scalar_value_assignment() {
        match p("lvl: `low`mid`high") {
            Stmt::ScalarAssign { name, expr } => {
                assert_eq!(name, "lvl");
                assert_eq!(expr, Expr::Lit(crate::ast::sym_vec(vec![
                    "low".into(), "mid".into(), "high".into(),
                ])));
            }
            other => panic!("expected scalar assign, got {other:?}"),
        }
    }

    #[test]
    fn bare_symbol_dict_sort_is_unaffected_by_cast_modifiers() {
        // `\`a\`b!01b t` must still parse as a sort, not a cast
        assert!(matches!(
            p("`c1`c2!01b t"),
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
            rolling: None,
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
    fn assign_multi_col_select_is_a_table_binding() {
        match p("t: select px, qty from trades") {
            Stmt::Assign { name, body } => {
                assert_eq!(name, "t");
                assert!(matches!(*body, Stmt::RetTable(_)));
            }
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn assign_one_col_select_is_a_column_expression() {
        // in assignment position a single-column select materialises to a list
        match p("t: select px from trades") {
            Stmt::ScalarAssign { name, expr } => {
                assert_eq!(name, "t");
                assert!(matches!(expr, Expr::Table(_)));
            }
            other => panic!("expected ScalarAssign, got {other:?}"),
        }
    }

    // --- full query (the key example) ---

    #[test]
    fn full_query() {
        // select dbl: c3*2 by c1 from t where c2>15
        let s = sel("select dbl: c3*2 by c1 from t where c2>15");
        assert_eq!(s.cols, vec![named("dbl", binop(cref("c3"), "*", Expr::Lit(Value::Int(2))))]);
        assert_eq!(s.by, Some(vec![col(cref("c1"))]));
        assert_eq!(*s.from, TableExpr::Source(TableSource::InMem("t".into())));
        assert_eq!(s.where_, Some(vec![binop(cref("c2"), ">", Expr::Lit(Value::Int(15)))]));
    }

    // --- `` `w!hopen `` write-mode connection modifier ---

    #[test]
    fn bare_hopen_stays_hopen() {
        match p("hopen 5001") {
            Stmt::SingleVar(Expr::Call { func, args }) => {
                assert_eq!(func, "hopen");
                assert_eq!(args, vec![Expr::Lit(Value::Int(5001))]);
            }
            other => panic!("expected SingleVar(Call), got {other:?}"),
        }
    }

    #[test]
    fn bang_w_modifier_lowers_hopen_to_whopen() {
        match p("`w!hopen 5001") {
            Stmt::SingleVar(Expr::Call { func, args }) => {
                assert_eq!(func, "whopen");
                assert_eq!(args, vec![Expr::Lit(Value::Int(5001))]);
            }
            other => panic!("expected SingleVar(Call), got {other:?}"),
        }
    }

    #[test]
    fn bang_w_modifier_works_in_an_assignment() {
        match p("conn: `w!hopen 5001") {
            Stmt::ScalarAssign { name, expr: Expr::Call { func, .. } } => {
                assert_eq!(name, "conn");
                assert_eq!(func, "whopen");
            }
            other => panic!("expected ScalarAssign(Call), got {other:?}"),
        }
    }

    #[test]
    fn bang_modifier_other_than_w_is_a_parse_error() {
        let tokens = tokenise("`x!hopen 5001").expect("lex error");
        assert!(parse(tokens).is_err());
    }

    #[test]
    fn bang_w_modifier_requires_hopen() {
        let tokens = tokenise("`w!1+1").expect("lex error");
        assert!(parse(tokens).is_err());
    }

    fn parse_err(src: &str) -> String {
        match parse(tokenise(src).expect("lex error")) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected a parse error for {src:?}"),
        }
    }

    #[test]
    fn while_parses_a_test_and_a_body() {
        match p("while[x>0; log x; x: x-1]") {
            Stmt::SingleVar(Expr::While { body, .. }) => {
                assert_eq!(body.len(), 2);
                assert!(matches!(body[1], Stmt::ScalarAssign { .. }));
            }
            other => panic!("expected a while, got {other:?}"),
        }
    }

    #[test]
    fn while_needs_a_test_and_a_statement() {
        assert!(parse_err("while[1b]").contains("at least one statement"));
        assert!(parse_err("while[x: 1; 2]").contains("not an assignment"));
        assert!(parse_err("while[1b;;2]").contains("empty slot"));
    }

    #[test]
    fn noop_parses_as_a_keyword() {
        assert_eq!(p("noop"), Stmt::SingleVar(Expr::Noop));
    }

    #[test]
    fn while_and_noop_are_reserved() {
        assert!(parse_err("while: 1").contains("reserved word"));
        assert!(parse_err("noop: 1").contains("reserved word"));
        assert!(parse_err("{[noop] 1}").contains("reserved word"));
        assert!(parse_err("while").contains("reserved word"));
    }

    #[test]
    fn while_and_noop_may_end_a_function_body() {
        p("f: {[n] while[n>0; n: n-1]}");
        p("f: {[] noop}");
    }

    #[test]
    fn while_test_must_be_an_expression_not_a_statement() {
        assert!(parse_err("while[select from t; 1]").contains("must be an expression"));
        assert!(parse_err("while[x: 1b; 1]").contains("must be an expression"));
    }

    #[test]
    fn a_trailing_or_doubled_semicolon_is_an_empty_slot() {
        assert!(parse_err("while[1b; 2;]").contains("empty slot"));
        assert!(parse_err("while[;1]").contains("empty slot"));
        assert!(parse_err("while[]").contains("empty slot"));
    }

    #[test]
    fn while_nests_and_may_sit_in_a_conditional_branch() {
        match p("while[a<3; while[b<2; b: b+1]; a: a+1]") {
            Stmt::SingleVar(Expr::While { body, .. }) => {
                assert!(matches!(&body[0], Stmt::SingleVar(Expr::While { .. })));
            }
            other => panic!("{other:?}"),
        }
        match p("?[c; while[d; 1]; noop]") {
            Stmt::SingleVar(Expr::Case { branches, default }) => {
                assert!(matches!(branches[0].1, Expr::While { .. }));
                assert_eq!(*default, Expr::Noop);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reserved_words_are_rejected_everywhere_a_name_is_bound() {
        assert!(parse_err("{[x,while] 1}").contains("reserved word"));
        assert!(parse_err("select while from t").contains("reserved word"));
        // a bare `while` without brackets is never a variable reference
        assert!(parse_err("1 + while").contains("reserved word"));
    }

    #[test]
    fn identifiers_that_merely_contain_the_words_are_fine() {
        p("whiled: 1");
        p("noop2: 1");
        p("nooper[1]");
        p(".ns.while: 1");
    }
}
