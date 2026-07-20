// Direct port of kestrel.js's parse() — same grammar, same precedence
// climbing structure. See docs/SYNTAX.md.

use crate::ast::*;
use crate::error::{ErrorKind, KestrelcError};
use crate::interner::Symbol;
use crate::lexer::{Tok, Token};

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

type PResult<T> = Result<T, KestrelcError>;

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0 }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn advance(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn at(&self, tok: &Tok) -> bool {
        &self.peek().tok == tok
    }

    fn expect(&mut self, tok: Tok) -> PResult<Token> {
        if self.at(&tok) {
            Ok(self.advance())
        } else {
            Err(KestrelcError::new(
                ErrorKind::Parse,
                format!("Expected '{:?}' but found '{:?}'", tok, self.peek().tok),
                self.peek().span,
            ))
        }
    }

    fn expect_ident(&mut self) -> PResult<Symbol> {
        match &self.peek().tok {
            Tok::Ident(s) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            other => Err(KestrelcError::new(
                ErrorKind::Parse,
                format!("Expected identifier but found '{:?}'", other),
                self.peek().span,
            )),
        }
    }

    pub fn parse_program(&mut self) -> PResult<Program> {
        let mut items = Vec::new();
        while !self.at(&Tok::Eof) {
            items.push(self.parse_fn_decl()?);
        }
        Ok(items)
    }

    fn parse_type(&mut self) -> PResult<Type> {
        if self.at(&Tok::LBracket) {
            self.advance();
            let elem = self.parse_type()?;
            self.expect(Tok::Semi)?;
            let size = self.expect_ident()?;
            self.expect(Tok::RBracket)?;
            return Ok(Type::Array { elem: Box::new(elem), size });
        }
        Ok(Type::Named(self.expect_ident()?))
    }

    fn parse_params(&mut self) -> PResult<Vec<Param>> {
        let mut params = Vec::new();
        if !self.at(&Tok::RParen) {
            loop {
                let name = self.expect_ident()?;
                self.expect(Tok::Colon)?;
                let ty = self.parse_type()?;
                params.push(Param { name, ty });
                if self.at(&Tok::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        Ok(params)
    }

    fn parse_args(&mut self) -> PResult<Vec<Expr>> {
        let mut args = Vec::new();
        if !self.at(&Tok::RParen) {
            loop {
                args.push(self.parse_expr()?);
                if self.at(&Tok::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
        }
        Ok(args)
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let t = self.peek().clone();
        match t.tok {
            Tok::Number(n) => {
                self.advance();
                Ok(Expr::Num(n))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Str(s))
            }
            Tok::True => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            Tok::False => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            Tok::LBracket => {
                self.advance();
                let mut elems = Vec::new();
                if !self.at(&Tok::RBracket) {
                    loop {
                        elems.push(self.parse_expr()?);
                        if self.at(&Tok::Comma) {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                }
                self.expect(Tok::RBracket)?;
                Ok(Expr::ArrayLit(elems))
            }
            Tok::LParen => {
                self.advance();
                let e = self.parse_expr()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Tok::Ident(name) => {
                self.advance();
                Ok(Expr::Ident(name))
            }
            other => Err(KestrelcError::new(
                ErrorKind::Parse,
                format!("Unexpected token '{:?}'", other),
                t.span,
            )),
        }
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut expr = self.parse_primary()?;
        loop {
            if self.at(&Tok::LBracket) {
                self.advance();
                let index = self.parse_expr()?;
                self.expect(Tok::RBracket)?;
                expr = Expr::Index { target: Box::new(expr), index: Box::new(index) };
            } else if self.at(&Tok::LParen) {
                if let Expr::Ident(name) = &expr {
                    let name = name.clone();
                    self.advance();
                    let args = self.parse_args()?;
                    self.expect(Tok::RParen)?;
                    expr = Expr::Call { name, args };
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        if self.at(&Tok::Minus) {
            self.advance();
            return Ok(Expr::Unary { op: UnOp::Neg, expr: Box::new(self.parse_unary()?) });
        }
        if self.at(&Tok::Bang) {
            self.advance();
            return Ok(Expr::Unary { op: UnOp::Not, expr: Box::new(self.parse_unary()?) });
        }
        self.parse_postfix()
    }

    fn parse_term(&mut self) -> PResult<Expr> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek().tok {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary()?;
            left = Expr::Binop { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> PResult<Expr> {
        let mut left = self.parse_term()?;
        loop {
            let op = match self.peek().tok {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_term()?;
            left = Expr::Binop { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> PResult<Expr> {
        let mut left = self.parse_additive()?;
        loop {
            let op = match self.peek().tok {
                Tok::EqEq => BinOp::Eq,
                Tok::NotEq => BinOp::Neq,
                Tok::Lt => BinOp::Lt,
                Tok::Gt => BinOp::Gt,
                Tok::LtEq => BinOp::Le,
                Tok::GtEq => BinOp::Ge,
                _ => break,
            };
            self.advance();
            let right = self.parse_additive()?;
            left = Expr::Binop { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_expr(&mut self) -> PResult<Expr> {
        let mut left = self.parse_comparison()?;
        loop {
            let op = match self.peek().tok {
                Tok::AndAnd => BinOp::And,
                Tok::OrOr => BinOp::Or,
                _ => break,
            };
            self.advance();
            let right = self.parse_comparison()?;
            left = Expr::Binop { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect(Tok::LBrace)?;
        let mut stmts = Vec::new();
        while !self.at(&Tok::RBrace) {
            stmts.push(self.parse_stmt()?);
        }
        self.expect(Tok::RBrace)?;
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        let span = self.peek().span;
        if self.at(&Tok::Let) {
            self.advance();
            let name = self.expect_ident()?;
            self.expect(Tok::Eq)?;
            let value = self.parse_expr()?;
            self.expect(Tok::Semi)?;
            return Ok(Stmt::Let { name, value, span });
        }
        if self.at(&Tok::If) {
            self.advance();
            self.expect(Tok::LParen)?;
            let cond = self.parse_expr()?;
            self.expect(Tok::RParen)?;
            let then_block = self.parse_block()?;
            let else_block = if self.at(&Tok::Else) {
                self.advance();
                Some(self.parse_block()?)
            } else {
                None
            };
            return Ok(Stmt::If { cond, then_block, else_block, span });
        }
        if self.at(&Tok::While) {
            self.advance();
            self.expect(Tok::LParen)?;
            let cond = self.parse_expr()?;
            self.expect(Tok::RParen)?;
            let body = self.parse_block()?;
            return Ok(Stmt::While { cond, body, span });
        }
        if self.at(&Tok::Print) {
            self.advance();
            self.expect(Tok::LParen)?;
            let args = self.parse_args()?;
            self.expect(Tok::RParen)?;
            self.expect(Tok::Semi)?;
            return Ok(Stmt::Print { args, span });
        }
        if self.at(&Tok::Return) {
            self.advance();
            let value = if self.at(&Tok::Semi) { None } else { Some(self.parse_expr()?) };
            self.expect(Tok::Semi)?;
            return Ok(Stmt::Return { value, span });
        }
        if let Tok::Ident(name) = &self.peek().tok {
            let name = name.clone();
            if self.tokens[self.pos + 1].tok == Tok::Eq {
                self.advance();
                self.advance();
                let value = self.parse_expr()?;
                self.expect(Tok::Semi)?;
                return Ok(Stmt::Assign { name, value, span });
            }
        }
        let expr = self.parse_expr()?;
        self.expect(Tok::Semi)?;
        Ok(Stmt::ExprStmt { expr, span })
    }

    fn parse_fn_decl(&mut self) -> PResult<Fn> {
        let span = self.peek().span;
        let pure = if self.at(&Tok::Pure) {
            self.advance();
            true
        } else {
            false
        };
        self.expect(Tok::Fn)?;
        let name = self.expect_ident()?;
        self.expect(Tok::LParen)?;
        let params = self.parse_params()?;
        self.expect(Tok::RParen)?;
        let return_type = if self.at(&Tok::Arrow) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };
        let where_clause = if self.at(&Tok::Where) {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        let body = self.parse_block()?;
        Ok(Fn { name, pure, params, return_type, where_clause, body, span })
    }
}

pub fn parse(tokens: Vec<Token>) -> PResult<Program> {
    Parser::new(tokens).parse_program()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;

    #[test]
    fn missing_semicolon_error_points_at_the_next_token() {
        // "return x" with no ';' — the parser reports the token it found
        // instead ('}'), at that token's own line/column.
        let src = "fn f() -> i32 {\n    return 5\n}\n";
        let tokens = lex(src).unwrap();
        let err = parse(tokens).unwrap_err();
        assert_eq!(err.span.line, 3);
        assert_eq!(err.span.col, 1); // the '}' that closes the function
        assert_eq!(err.span.len, 1);
    }

    #[test]
    fn unexpected_token_error_carries_the_bad_token_s_span() {
        let tokens = lex("fn main() { let x = ; }").unwrap();
        let err = parse(tokens).unwrap_err();
        assert_eq!(err.span.line, 1);
        assert_eq!(err.span.col, 21); // the ';' where an expression was expected
        assert_eq!(err.span.len, 1);
    }
}
