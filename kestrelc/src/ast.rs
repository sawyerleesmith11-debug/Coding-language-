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

#[derive(Debug, Clone)]
pub enum Expr {
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

// Every variant carries the Span of its first token — statement
// granularity, not full per-expression (see kestrel-DESIGN.md's roadmap
// item 1: a span on every AST node, not just statements, is still future
// work). This is enough to point purity/type-check errors at a real
// source location instead of nothing, matching the granularity
// kestrel.js's own checkPurity/checkTypes already report at.
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
