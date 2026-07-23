// Direct port of kestrel.js's parse() — same grammar, same precedence
// climbing structure. See docs/SYNTAX.md.

use crate::ast::*;
use crate::error::{ErrorKind, KestrelcError};
use crate::interner::Symbol;
use crate::lexer::{Tok, Token};
use crate::span::Span;

/// Rewrites every `continue;` belonging to THIS loop (not a nested one)
/// into `{ step; continue; }` in place -- see the general-for desugar's
/// call site for why. Does not recurse into a nested `While`/`RangeFor`
/// body: a `continue` in there belongs to that inner loop, not this
/// one, and must be left untouched.
fn inject_step_before_continues(stmts: &mut Vec<Stmt>, step: &Stmt) {
    let mut i = 0;
    while i < stmts.len() {
        match &mut stmts[i] {
            Stmt::Continue { .. } => {
                stmts.insert(i, step.clone());
                i += 1; // skip the just-inserted step, land back on the continue
            }
            Stmt::If { then_block, else_block, .. } => {
                inject_step_before_continues(then_block, step);
                if let Some(eb) = else_block {
                    inject_step_before_continues(eb, step);
                }
            }
            _ => {}
        }
        i += 1;
    }
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// True while parsing a `where` clause's expression, directly at the
    /// top level (not inside `(...)`/`[...]`/call args). Suppresses
    /// `IDENT {` being read as a struct literal, since the `{` right
    /// after a where clause is the function body's opening brace, not a
    /// literal -- e.g. `fn f(...) -> i32 where i < N { ... }` must not
    /// parse `N { ... }` as a struct literal swallowing the whole body.
    /// Cleared inside any delimited sub-expression, where the ambiguity
    /// doesn't apply (a matching close delimiter still disambiguates
    /// the ending), and restored after leaving it.
    suppress_struct_lit: bool,
}

type PResult<T> = Result<T, KestrelcError>;

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Parser { tokens, pos: 0, suppress_struct_lit: false }
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
        let mut fns = Vec::new();
        let mut structs = Vec::new();
        while !self.at(&Tok::Eof) {
            if self.at(&Tok::Struct) {
                structs.push(self.parse_struct_decl()?);
            } else {
                fns.push(self.parse_fn_decl()?);
            }
        }
        Ok(Program { fns, structs })
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
                    if self.at(&Tok::RParen) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        Ok(params)
    }

    fn parse_struct_decl(&mut self) -> PResult<StructDecl> {
        let span = self.peek().span;
        self.expect(Tok::Struct)?;
        let name = self.expect_ident()?;
        self.expect(Tok::LBrace)?;
        let mut fields = Vec::new();
        if !self.at(&Tok::RBrace) {
            loop {
                let field_name = self.expect_ident()?;
                self.expect(Tok::Colon)?;
                let ty = self.parse_type()?;
                fields.push(Param { name: field_name, ty });
                if self.at(&Tok::Comma) {
                    self.advance();
                    if self.at(&Tok::RBrace) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(Tok::RBrace)?;
        Ok(StructDecl { name, fields, span })
    }

    /// Called from `parse_primary` once it's seen `IDENT {` — `name`
    /// and `span` are the already-consumed identifier's own name/span.
    /// See the design doc's documented edge case: a `where` clause
    /// (unparenthesized) immediately followed by a function body could
    /// misparse here if the clause bare-references a name that
    /// collides with a declared struct name. Accepted, not fixed, in
    /// v1 — the program wouldn't type-check either way.
    fn parse_struct_lit(&mut self, name: Symbol, span: Span) -> PResult<Expr> {
        self.expect(Tok::LBrace)?;
        let mut fields = Vec::new();
        if !self.at(&Tok::RBrace) {
            loop {
                let field_name = self.expect_ident()?;
                self.expect(Tok::Colon)?;
                let value = self.parse_expr()?;
                fields.push((field_name, value));
                if self.at(&Tok::Comma) {
                    self.advance();
                    if self.at(&Tok::RBrace) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(Tok::RBrace)?;
        Ok(Expr::new(ExprKind::StructLit { name, fields }, span))
    }

    fn parse_args(&mut self) -> PResult<Vec<Expr>> {
        let saved = self.suppress_struct_lit;
        self.suppress_struct_lit = false;
        let mut args = Vec::new();
        if !self.at(&Tok::RParen) {
            loop {
                args.push(self.parse_expr()?);
                if self.at(&Tok::Comma) {
                    self.advance();
                    if self.at(&Tok::RParen) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.suppress_struct_lit = saved;
        Ok(args)
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let t = self.peek().clone();
        let span = t.span;
        match t.tok {
            Tok::Number(n) => {
                self.advance();
                Ok(Expr::new(ExprKind::Num(n), span))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::new(ExprKind::Str(s), span))
            }
            Tok::True => {
                self.advance();
                Ok(Expr::new(ExprKind::Bool(true), span))
            }
            Tok::False => {
                self.advance();
                Ok(Expr::new(ExprKind::Bool(false), span))
            }
            Tok::LBracket => {
                self.advance();
                let saved = self.suppress_struct_lit;
                self.suppress_struct_lit = false;
                let mut elems = Vec::new();
                if !self.at(&Tok::RBracket) {
                    loop {
                        elems.push(self.parse_expr()?);
                        if self.at(&Tok::Comma) {
                            self.advance();
                            if self.at(&Tok::RBracket) {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                }
                self.suppress_struct_lit = saved;
                self.expect(Tok::RBracket)?;
                Ok(Expr::new(ExprKind::ArrayLit(elems), span))
            }
            Tok::LParen => {
                self.advance();
                let saved = self.suppress_struct_lit;
                self.suppress_struct_lit = false;
                let e = self.parse_expr()?;
                self.suppress_struct_lit = saved;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Tok::Ident(name) => {
                self.advance();
                if self.at(&Tok::LBrace) && !self.suppress_struct_lit {
                    return self.parse_struct_lit(name, span);
                }
                Ok(Expr::new(ExprKind::Ident(name), span))
            }
            other => Err(KestrelcError::new(
                ErrorKind::Parse,
                format!("Unexpected token '{:?}'", other),
                t.span,
            )),
        }
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        let span = self.peek().span;
        let mut expr = self.parse_primary()?;
        loop {
            if self.at(&Tok::LBracket) {
                self.advance();
                let index = self.parse_expr()?;
                self.expect(Tok::RBracket)?;
                expr = Expr::new(ExprKind::Index { target: Box::new(expr), index: Box::new(index) }, span);
            } else if self.at(&Tok::LParen) {
                if let ExprKind::Ident(name) = &expr.kind {
                    let name = name.clone();
                    self.advance();
                    let args = self.parse_args()?;
                    self.expect(Tok::RParen)?;
                    expr = Expr::new(ExprKind::Call { name, args }, span);
                } else {
                    break;
                }
            } else if self.at(&Tok::Dot) {
                self.advance();
                let field = self.expect_ident()?;
                // `arr.map(f)` sugar for `parallel_map(f, arr)` -- only
                // when `map` is immediately followed by `(`, so a real
                // struct field literally named `map` (`p.map`, no call)
                // still parses as plain field access below, unchanged.
                // Argument order swaps deliberately: parallel_map's own
                // signature takes the callback first
                // (`parallel_map(f, arr)`), but the receiver naturally
                // comes first in method-call syntax (`arr.map(f)`).
                if field == crate::interner::well_known::map() && self.at(&Tok::LParen) {
                    self.advance();
                    let callback = self.parse_expr()?;
                    self.expect(Tok::RParen)?;
                    expr = Expr::new(
                        ExprKind::Call { name: crate::interner::well_known::parallel_map(), args: vec![callback, expr] },
                        span,
                    );
                } else {
                    expr = Expr::new(ExprKind::Field { target: Box::new(expr), field }, span);
                }
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        if self.at(&Tok::Minus) {
            let span = self.peek().span;
            self.advance();
            return Ok(Expr::new(ExprKind::Unary { op: UnOp::Neg, expr: Box::new(self.parse_unary()?) }, span));
        }
        if self.at(&Tok::Bang) {
            let span = self.peek().span;
            self.advance();
            return Ok(Expr::new(ExprKind::Unary { op: UnOp::Not, expr: Box::new(self.parse_unary()?) }, span));
        }
        self.parse_postfix()
    }

    fn parse_term(&mut self) -> PResult<Expr> {
        let span = self.peek().span;
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
            left = Expr::new(ExprKind::Binop { op, left: Box::new(left), right: Box::new(right) }, span);
        }
        Ok(left)
    }

    fn parse_additive(&mut self) -> PResult<Expr> {
        let span = self.peek().span;
        let mut left = self.parse_term()?;
        loop {
            let op = match self.peek().tok {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_term()?;
            left = Expr::new(ExprKind::Binop { op, left: Box::new(left), right: Box::new(right) }, span);
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> PResult<Expr> {
        let span = self.peek().span;
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
            left = Expr::new(ExprKind::Binop { op, left: Box::new(left), right: Box::new(right) }, span);
        }
        Ok(left)
    }

    fn parse_expr(&mut self) -> PResult<Expr> {
        let span = self.peek().span;
        let mut left = self.parse_comparison()?;
        loop {
            let op = match self.peek().tok {
                Tok::AndAnd => BinOp::And,
                Tok::OrOr => BinOp::Or,
                _ => break,
            };
            self.advance();
            let right = self.parse_comparison()?;
            left = Expr::new(ExprKind::Binop { op, left: Box::new(left), right: Box::new(right) }, span);
        }
        Ok(left)
    }

    fn parse_block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect(Tok::LBrace)?;
        let mut stmts = Vec::new();
        while !self.at(&Tok::RBrace) {
            stmts.extend(self.parse_stmt()?);
        }
        self.expect(Tok::RBrace)?;
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> PResult<Vec<Stmt>> {
        let span = self.peek().span;
        if self.at(&Tok::Let) {
            self.advance();
            let name = self.expect_ident()?;
            self.expect(Tok::Eq)?;
            let value = self.parse_expr()?;
            self.expect(Tok::Semi)?;
            return Ok(vec![Stmt::Let { name, value, span }]);
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
            return Ok(vec![Stmt::If { cond, then_block, else_block, span }]);
        }
        if self.at(&Tok::While) {
            self.advance();
            self.expect(Tok::LParen)?;
            let cond = self.parse_expr()?;
            self.expect(Tok::RParen)?;
            let body = self.parse_block()?;
            return Ok(vec![Stmt::While { cond, body, span }]);
        }
        if self.at(&Tok::For) {
            self.advance();
            let var = self.expect_ident()?;
            if self.at(&Tok::From) {
                self.advance();
                // Same ambiguity as a `where` clause: `end` is followed
                // directly by the loop body's `{`, with no parens in
                // between (unlike `while (cond) { ... }`) to separate an
                // `Ident {` from a struct-literal's own `Ident { field:
                // value }` syntax. Suppress struct-literal parsing for
                // both bounds so e.g. `for i from 0 to n { ... }` parses
                // `n` as a plain identifier expression, not the start of
                // a (invalid, since it has no fields) struct literal.
                let saved = self.suppress_struct_lit;
                self.suppress_struct_lit = true;
                let start = self.parse_expr()?;
                self.expect(Tok::To)?;
                let end = self.parse_expr()?;
                self.suppress_struct_lit = saved;
                let body = self.parse_block()?;
                return Ok(vec![Stmt::RangeFor { var, start, end, body, span }]);
            }
            // General-for: `for i = <init>, <cond>, i = <step> { body }` --
            // desugars directly into `let i = <init>; while (<cond>) {
            // body...; i = <step>; }`, so every downstream pass (resolve,
            // purity, typecheck, all three codegens, fusion, CSE) handles
            // it automatically via the existing Let/While/Assign arms it
            // already has -- no new AST node, no new arm anywhere else in
            // the compiler.
            self.expect(Tok::Eq)?;
            let init_value = self.parse_expr()?;
            self.expect(Tok::Comma)?;
            let cond = self.parse_expr()?;
            self.expect(Tok::Comma)?;
            let step_span = self.peek().span;
            let step_name = self.expect_ident()?;
            if step_name != var {
                return Err(KestrelcError::new(
                    ErrorKind::Parse,
                    format!(
                        "for-loop step must update the same loop variable '{}', found '{}'",
                        var, step_name
                    ),
                    step_span,
                ));
            }
            self.expect(Tok::Eq)?;
            let step_value = self.parse_expr()?;
            let step_stmt = Stmt::Assign { name: var, value: step_value, span: step_span };
            let mut body = self.parse_block()?;
            // A bare `continue;` inside this body would otherwise jump
            // straight to the desugared `while`'s condition recheck,
            // skipping the step entirely -- for a `while` loop that's
            // correct (it has no separate step to preserve), but
            // general-for is meant to provide C `for`-loop semantics,
            // where `continue` still runs the step before rechecking.
            // Rewriting every such `continue` into `{ step; continue; }`
            // preserves that without needing a dedicated AST node or
            // codegen support (see ast.rs's Stmt::Continue doc comment
            // for why RangeFor needs one but general-for doesn't).
            inject_step_before_continues(&mut body, &step_stmt);
            body.push(step_stmt);
            return Ok(vec![
                Stmt::Let { name: var, value: init_value, span },
                Stmt::While { cond, body, span },
            ]);
        }
        if self.at(&Tok::Print) {
            self.advance();
            self.expect(Tok::LParen)?;
            let args = self.parse_args()?;
            self.expect(Tok::RParen)?;
            self.expect(Tok::Semi)?;
            return Ok(vec![Stmt::Print { args, span }]);
        }
        if self.at(&Tok::Return) {
            self.advance();
            let value = if self.at(&Tok::Semi) { None } else { Some(self.parse_expr()?) };
            self.expect(Tok::Semi)?;
            return Ok(vec![Stmt::Return { value, span }]);
        }
        if self.at(&Tok::Break) {
            self.advance();
            self.expect(Tok::Semi)?;
            return Ok(vec![Stmt::Break { span }]);
        }
        if self.at(&Tok::Continue) {
            self.advance();
            self.expect(Tok::Semi)?;
            return Ok(vec![Stmt::Continue { span }]);
        }
        if let Tok::Ident(name) = &self.peek().tok {
            let name = name.clone();
            // `p.x = value;` -- field assignment. Checked via 4-token
            // lookahead (ident, dot, ident, eq) before the plain-ident
            // checks below, since without this `p.x` would otherwise be
            // parsed as a bare Field expression by parse_expr() and then
            // choke on the following `=` (ExprStmt expects `;` next, not
            // `=`) instead of being recognized as an assignment target.
            if self.tokens.get(self.pos + 1).is_some_and(|t| t.tok == Tok::Dot) {
                if let Some(Tok::Ident(field)) = self.tokens.get(self.pos + 2).map(|t| &t.tok) {
                    if self.tokens.get(self.pos + 3).is_some_and(|t| t.tok == Tok::Eq) {
                        let field = field.clone();
                        self.advance(); // ident
                        self.advance(); // dot
                        self.advance(); // field ident
                        self.advance(); // =
                        let value = self.parse_expr()?;
                        self.expect(Tok::Semi)?;
                        return Ok(vec![Stmt::FieldAssign { target: name, field, value, span }]);
                    }
                }
            }
            if self.tokens[self.pos + 1].tok == Tok::Eq {
                self.advance();
                self.advance();
                let value = self.parse_expr()?;
                self.expect(Tok::Semi)?;
                return Ok(vec![Stmt::Assign { name, value, span }]);
            }
            // Compound assignment (`x += 1;` etc.) desugars directly to
            // `x = x + 1;` at parse time -- same reasoning as general-for's
            // step clause: no new AST shape, every existing pass (resolve,
            // purity, typecheck, both codegens) already handles a plain
            // `Stmt::Assign` with a `Binop` value with zero changes.
            let compound_op = match &self.tokens[self.pos + 1].tok {
                Tok::PlusEq => Some(BinOp::Add),
                Tok::MinusEq => Some(BinOp::Sub),
                Tok::StarEq => Some(BinOp::Mul),
                Tok::SlashEq => Some(BinOp::Div),
                Tok::PercentEq => Some(BinOp::Mod),
                _ => None,
            };
            if let Some(op) = compound_op {
                self.advance();
                self.advance();
                let rhs = self.parse_expr()?;
                self.expect(Tok::Semi)?;
                let value = Expr::new(
                    ExprKind::Binop {
                        op,
                        left: Box::new(Expr::new(ExprKind::Ident(name), span)),
                        right: Box::new(rhs),
                    },
                    span,
                );
                return Ok(vec![Stmt::Assign { name, value, span }]);
            }
        }
        let expr = self.parse_expr()?;
        self.expect(Tok::Semi)?;
        Ok(vec![Stmt::ExprStmt { expr, span }])
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
            let saved = self.suppress_struct_lit;
            self.suppress_struct_lit = true;
            let clause = self.parse_expr()?;
            self.suppress_struct_lit = saved;
            Some(clause)
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
    fn compound_assignment_desugars_to_a_plain_assign_with_a_binop_value() {
        let program = crate::parser::parse(crate::lexer::lex(
            "fn main() { let x = 10; x += 5; }"
        ).unwrap()).unwrap();
        let main_fn = &program.fns[0];
        assert_eq!(main_fn.body.len(), 2);
        let Stmt::Assign { name, value, .. } = &main_fn.body[1] else {
            panic!("expected Assign, got {:?}", main_fn.body[1]);
        };
        assert_eq!(name.resolve().as_ref(), "x");
        let ExprKind::Binop { op, left, right } = &value.kind else {
            panic!("expected Binop value, got {:?}", value.kind);
        };
        assert!(matches!(op, BinOp::Add));
        assert!(matches!(&left.kind, ExprKind::Ident(n) if n.resolve().as_ref() == "x"));
        assert!(matches!(right.kind, ExprKind::Num(5)));
    }

    #[test]
    fn a_trailing_comma_is_allowed_in_struct_decl_fields_struct_lit_fields_params_call_args_and_array_lits() {
        let program = crate::parser::parse(crate::lexer::lex(
            "struct Point { x: i64, y: i64, }\n\
             pure fn add(a: i64, b: i64,) -> i64 { return a + b; }\n\
             fn main() {\n\
             \x20   let p = Point { x: 1, y: 2, };\n\
             \x20   let arr = [1, 2, 3,];\n\
             \x20   print(add(p.x, p.y,));\n\
             }\n"
        ).unwrap()).unwrap();
        assert_eq!(program.structs.len(), 1);
        assert_eq!(program.structs[0].fields.len(), 2);
        assert_eq!(program.fns.len(), 2);
    }

    #[test]
    fn parses_range_for_into_a_rangefor_node() {
        let program = crate::parser::parse(crate::lexer::lex(
            "fn main() { for i from 0 to 5 { print(i); } }"
        ).unwrap()).unwrap();
        let main_fn = &program.fns[0];
        assert_eq!(main_fn.body.len(), 1);
        let Stmt::RangeFor { var, start, end, body, .. } = &main_fn.body[0] else {
            panic!("expected RangeFor, got {:?}", main_fn.body[0]);
        };
        assert_eq!(var.resolve().as_ref(), "i");
        assert!(matches!(start.kind, ExprKind::Num(0)));
        assert!(matches!(end.kind, ExprKind::Num(5)));
        assert_eq!(body.len(), 1);
    }

    #[test]
    fn parses_general_for_as_a_let_followed_by_a_while() {
        let program = crate::parser::parse(crate::lexer::lex(
            "fn main() { for i = 0, i < 5, i = i + 2 { print(i); } }"
        ).unwrap()).unwrap();
        let main_fn = &program.fns[0];
        assert_eq!(main_fn.body.len(), 2, "general-for must desugar to exactly two top-level statements");
        assert!(matches!(&main_fn.body[0], Stmt::Let { .. }));
        let Stmt::While { body, .. } = &main_fn.body[1] else {
            panic!("expected While as the second desugared statement, got {:?}", main_fn.body[1]);
        };
        // original print(i) plus the appended step assignment
        assert_eq!(body.len(), 2);
        assert!(matches!(&body[1], Stmt::Assign { .. }));
    }

    #[test]
    fn a_bare_continue_in_a_general_for_body_gets_the_step_injected_before_it() {
        let program = crate::parser::parse(crate::lexer::lex(
            "fn main() { for i = 0, i < 5, i = i + 1 { continue; } }"
        ).unwrap()).unwrap();
        let main_fn = &program.fns[0];
        let Stmt::While { body, .. } = &main_fn.body[1] else { panic!("expected While") };
        // body: [step, continue, step] -- the injected step before the
        // user's own continue, then the loop's own trailing step (for
        // the normal fallthrough path) still appended after.
        assert_eq!(body.len(), 3, "got: {:?}", body);
        assert!(matches!(&body[0], Stmt::Assign { .. }), "expected injected step before continue, got: {:?}", body[0]);
        assert!(matches!(&body[1], Stmt::Continue { .. }));
        assert!(matches!(&body[2], Stmt::Assign { .. }));
    }

    #[test]
    fn a_continue_inside_a_nested_loop_does_not_get_the_outer_steps_step_injected() {
        let program = crate::parser::parse(crate::lexer::lex(
            "fn main() { for i = 0, i < 5, i = i + 1 { while (true) { continue; } } }"
        ).unwrap()).unwrap();
        let main_fn = &program.fns[0];
        let Stmt::While { body: outer_body, .. } = &main_fn.body[1] else { panic!("expected outer While") };
        let Stmt::While { body: inner_body, .. } = &outer_body[0] else { panic!("expected inner While") };
        // The inner while's own continue must be untouched -- no step
        // injected, since that continue belongs to the inner loop.
        assert_eq!(inner_body.len(), 1, "got: {:?}", inner_body);
        assert!(matches!(&inner_body[0], Stmt::Continue { .. }));
    }

    #[test]
    fn general_for_step_targeting_a_different_variable_is_a_parse_error() {
        let result = crate::parser::parse(crate::lexer::lex(
            "fn main() { for i = 0, i < 5, j = i + 1 { print(i); } }"
        ).unwrap());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("same loop variable"), "got: {}", err.message);
    }

    #[test]
    fn a_where_clause_ending_in_a_bare_identifier_does_not_swallow_the_function_body_as_a_struct_literal() {
        // Regression test: `where i < N {` must parse `N` as the where
        // clause's own trailing identifier, not as `N { ... }`, a struct
        // literal that would swallow the entire function body up to the
        // first colon-less token and fail there instead.
        let src = "fn get_safe(arr: [i32; N], i: usize) -> i32 where i < N {\n    return arr[i];\n}\n";
        let program = parse(lex(src).unwrap()).unwrap();
        assert_eq!(program.fns[0].body.len(), 1);
    }

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

    #[test]
    fn parses_a_struct_declaration_and_adds_it_to_program_structs() {
        let program = parse(lex("struct Point { x: i64, y: i64 }\nfn main() {}\n").unwrap()).unwrap();
        assert_eq!(program.structs.len(), 1);
        let decl = &program.structs[0];
        assert_eq!(&*decl.name.resolve(), "Point");
        assert_eq!(decl.fields.len(), 2);
        assert_eq!(&*decl.fields[0].name.resolve(), "x");
        assert_eq!(&*decl.fields[1].name.resolve(), "y");
    }

    #[test]
    fn parses_a_struct_literal_with_fields_in_written_order() {
        let program = parse(lex(
            "struct Point { x: i64, y: i64 }\nfn main() { let p = Point { y: 2, x: 1 }; }\n"
        ).unwrap()).unwrap();
        let Stmt::Let { value, .. } = &program.fns[0].body[0] else { panic!("expected a let") };
        let ExprKind::StructLit { name, fields } = &value.kind else { panic!("expected a struct literal") };
        assert_eq!(&*name.resolve(), "Point");
        assert_eq!(fields.len(), 2);
        assert_eq!(&*fields[0].0.resolve(), "y"); // written order preserved
        assert_eq!(&*fields[1].0.resolve(), "x");
    }

    #[test]
    fn parses_field_access_as_a_postfix_operator() {
        let program = parse(lex("fn main() { let a = p.x; }\n").unwrap()).unwrap();
        let Stmt::Let { value, .. } = &program.fns[0].body[0] else { panic!("expected a let") };
        let ExprKind::Field { target, field } = &value.kind else { panic!("expected field access") };
        assert!(matches!(&target.kind, ExprKind::Ident(_)));
        assert_eq!(&*field.resolve(), "x");
    }

    #[test]
    fn parses_chained_field_access() {
        // Doesn't need to *mean* anything yet (nested structs are out
        // of scope) -- this only proves the parser's postfix loop
        // handles more than one `.` in a row without special-casing.
        let program = parse(lex("fn main() { let a = p.x.y; }\n").unwrap()).unwrap();
        let Stmt::Let { value, .. } = &program.fns[0].body[0] else { panic!("expected a let") };
        let ExprKind::Field { target, field } = &value.kind else { panic!("expected field access") };
        assert_eq!(&*field.resolve(), "y");
        assert!(matches!(&target.kind, ExprKind::Field { .. }));
    }

    #[test]
    fn parses_arr_dot_map_as_sugar_for_parallel_map_with_swapped_argument_order() {
        let program = parse(lex("fn main() { let out = arr.map(f); }\n").unwrap()).unwrap();
        let Stmt::Let { value, .. } = &program.fns[0].body[0] else { panic!("expected a let") };
        let ExprKind::Call { name, args } = &value.kind else { panic!("expected a call, got {:?}", value.kind) };
        assert_eq!(*name, crate::interner::well_known::parallel_map());
        assert_eq!(args.len(), 2);
        // parallel_map(f, arr) takes the callback first -- args[0] must
        // be the callback (`f`) even though `arr.map(f)` writes the
        // receiver (`arr`) first.
        assert!(matches!(&args[0].kind, ExprKind::Ident(n) if &*n.resolve() == "f"));
        assert!(matches!(&args[1].kind, ExprKind::Ident(n) if &*n.resolve() == "arr"));
    }

    #[test]
    fn a_struct_field_literally_named_map_without_parens_still_parses_as_plain_field_access() {
        // `.map` sugar only triggers when immediately followed by `(` --
        // a real struct field named `map` (no call) must be unaffected.
        let program = parse(lex("fn main() { let a = p.map; }\n").unwrap()).unwrap();
        let Stmt::Let { value, .. } = &program.fns[0].body[0] else { panic!("expected a let") };
        let ExprKind::Field { field, .. } = &value.kind else { panic!("expected field access, got {:?}", value.kind) };
        assert_eq!(&*field.resolve(), "map");
    }
}
