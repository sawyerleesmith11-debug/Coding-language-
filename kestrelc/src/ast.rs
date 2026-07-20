// AST shapes matching kestrel.js's parser output 1:1 — see docs/SYNTAX.md
// for the grammar. kestrelc's parser is complete (parses everything
// kestrel.js does); it's codegen that's scoped to a subset for now (see
// kestrelc/README.md) so unsupported programs fail with a clear error
// instead of silently miscompiling.

use crate::interner::Symbol;
use crate::span::Span;

#[derive(Debug, Clone)]
pub enum Type {
    Named(Symbol),
    Array { elem: Box<Type>, size: Symbol },
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: Symbol,
    pub ty: Type,
}

/// Every expression node carries its own `Span` now — the leading
/// token's position, same shallow convention `Fn`/`Stmt` already use
/// (not a true start..end range; see span.rs and the caret-rendering
/// code in main.rs/lib.rs, which only ever needs "point at the start of
/// the construct on its own line," not a real multi-token/multi-line
/// range). Wrapping `ExprKind` in a struct instead of putting `span` on
/// every variant (the way `Stmt` does it) keeps every consumer's match
/// arms from also having to carry a `span` binding they don't need —
/// most callers only care about `.span` at the one or two sites that
/// actually build an error.
#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

impl Expr {
    pub fn new(kind: ExprKind, span: Span) -> Self {
        Expr { kind, span }
    }
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Num(i64),
    Str(Symbol),
    Bool(bool),
    Ident(Symbol),
    ArrayLit(Vec<Expr>),
    Unary { op: UnOp, expr: Box<Expr> },
    Binop { op: BinOp, left: Box<Expr>, right: Box<Expr> },
    Index { target: Box<Expr>, index: Box<Expr> },
    Call { name: Symbol, args: Vec<Expr> },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Neq,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
}

// Every variant carries the Span of its first token — `Expr` now does
// too (see above), so a checker can report at whichever sub-expression
// actually has the problem instead of only the enclosing statement.
#[derive(Debug, Clone)]
pub enum Stmt {
    Let { name: Symbol, value: Expr, span: Span },
    Assign { name: Symbol, value: Expr, span: Span },
    If { cond: Expr, then_block: Vec<Stmt>, else_block: Option<Vec<Stmt>>, span: Span },
    While { cond: Expr, body: Vec<Stmt>, span: Span },
    Print { args: Vec<Expr>, span: Span },
    Return { value: Option<Expr>, span: Span },
    ExprStmt { expr: Expr, span: Span },
}

#[derive(Debug, Clone)]
pub struct Fn {
    pub name: Symbol,
    pub pure: bool,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub where_clause: Option<Expr>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

pub type Program = Vec<Fn>;
