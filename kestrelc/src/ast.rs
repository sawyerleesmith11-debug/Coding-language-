// AST shapes matching kestrel.js's parser output 1:1 — see docs/SYNTAX.md
// for the grammar. kestrelc's parser is complete (parses everything
// kestrel.js does); it's codegen that's scoped to a subset for now (see
// kestrelc/README.md) so unsupported programs fail with a clear error
// instead of silently miscompiling.

#[derive(Debug, Clone)]
pub enum Type {
    Named(String),
    Array { elem: Box<Type>, size: String },
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Type,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Num(i64),
    Str(String),
    Bool(bool),
    Ident(String),
    ArrayLit(Vec<Expr>),
    Unary { op: UnOp, expr: Box<Expr> },
    Binop { op: BinOp, left: Box<Expr>, right: Box<Expr> },
    Index { target: Box<Expr>, index: Box<Expr> },
    Call { name: String, args: Vec<Expr> },
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

#[derive(Debug, Clone)]
pub enum Stmt {
    Let { name: String, value: Expr },
    Assign { name: String, value: Expr },
    If { cond: Expr, then_block: Vec<Stmt>, else_block: Option<Vec<Stmt>> },
    While { cond: Expr, body: Vec<Stmt> },
    Print { args: Vec<Expr> },
    Return { value: Option<Expr> },
    ExprStmt { expr: Expr },
}

#[derive(Debug, Clone)]
pub struct Fn {
    pub name: String,
    pub pure: bool,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub where_clause: Option<Expr>,
    pub body: Vec<Stmt>,
    pub line: usize,
}

pub type Program = Vec<Fn>;
