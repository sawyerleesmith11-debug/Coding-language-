// First honest type checker — see kestrel-DESIGN.md's roadmap for
// exactly what this is and isn't. Types are still just written, not
// checked, as declared annotations (docs/SYNTAX.md's Types section) —
// this instead infers each expression's value *kind* (Int or Bool)
// purely from literals and operators, and rejects mixing them
// (`5 + true`, `!5`, a literal number used directly as an `if`/`while`
// condition), plus a plain function-call argument *count* mismatch.
// Deliberately does NOT check a call site's argument kinds against the
// callee's declared parameter type names — that needs a real decision
// about what Kestrel's built-in types actually are, a bigger step than
// this. A parameter's kind inside its own function body is always
// Unknown for the same reason (its declared type name carries no kind
// information yet) — every rule below only fires when it's *sure*,
// never guesses, so a program that would otherwise run correctly is
// never rejected. Direct port of kestrel.js's checkTypes — same rules,
// same wording.

use crate::ast::*;
use crate::span::Span;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Bool,
    Array,
    Str,
    Unknown,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Int => "int",
            Kind::Bool => "bool",
            Kind::Array => "array",
            Kind::Str => "str",
            Kind::Unknown => "unknown",
        }
    }
}

pub fn check_types(program: &Program) -> Vec<CheckError> {
    let fns: HashMap<&str, &Fn> = program.iter().map(|f| (f.name.as_str(), f)).collect();
    let mut errors = Vec::new();

    fn is_numeric(k: Kind) -> bool {
        matches!(k, Kind::Unknown | Kind::Int)
    }
    fn is_boolean(k: Kind) -> bool {
        matches!(k, Kind::Unknown | Kind::Bool)
    }
    fn op_symbol(op: BinOp) -> &'static str {
        match op {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Eq => "==",
            BinOp::Neq => "!=",
            BinOp::Lt => "<",
            BinOp::Gt => ">",
            BinOp::Le => "<=",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }

    // `span` is the enclosing statement's span (see ast.rs's CheckError
    // doc comment) — every error found anywhere inside one statement's
    // expression tree points at that statement, not the exact
    // sub-expression.
    fn infer_expr(
        e: &Expr,
        locals: &HashMap<String, Kind>,
        fns: &HashMap<&str, &Fn>,
        span: Span,
        errors: &mut Vec<CheckError>,
    ) -> Kind {
        let push = |errors: &mut Vec<CheckError>, message: String| {
            errors.push(CheckError { message, span });
        };
        match e {
            Expr::Num(_) => Kind::Int,
            Expr::Bool(_) => Kind::Bool,
            Expr::Str(_) => Kind::Str,
            Expr::Ident(name) => locals.get(name).copied().unwrap_or(Kind::Unknown),
            Expr::ArrayLit(elems) => {
                for el in elems {
                    infer_expr(el, locals, fns, span, errors);
                }
                Kind::Array
            }
            Expr::Index { target, index } => {
                infer_expr(target, locals, fns, span, errors);
                let idx_kind = infer_expr(index, locals, fns, span, errors);
                if idx_kind != Kind::Unknown && idx_kind != Kind::Int {
                    push(errors, format!("array index must be a number, found {}", idx_kind.name()));
                }
                Kind::Int // Kestrel arrays are integer-valued so far
            }
            Expr::Unary { op, expr } => {
                let k = infer_expr(expr, locals, fns, span, errors);
                match op {
                    UnOp::Neg => {
                        if k != Kind::Unknown && k != Kind::Int {
                            push(errors, format!("'-' needs a number, found {}", k.name()));
                        }
                        Kind::Int
                    }
                    UnOp::Not => {
                        if k != Kind::Unknown && k != Kind::Bool {
                            push(errors, format!("'!' needs a boolean, found {}", k.name()));
                        }
                        Kind::Bool
                    }
                }
            }
            Expr::Binop { op, left, right } => {
                let l = infer_expr(left, locals, fns, span, errors);
                let r = infer_expr(right, locals, fns, span, errors);
                let sym = op_symbol(*op);
                match op {
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                        if !is_numeric(l) || !is_numeric(r) {
                            push(errors, format!("'{sym}' needs two numbers, found {} and {}", l.name(), r.name()));
                        }
                        Kind::Int
                    }
                    BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                        if !is_numeric(l) || !is_numeric(r) {
                            push(errors, format!("'{sym}' needs two numbers, found {} and {}", l.name(), r.name()));
                        }
                        Kind::Bool
                    }
                    BinOp::And | BinOp::Or => {
                        if !is_boolean(l) || !is_boolean(r) {
                            push(errors, format!("'{sym}' needs two booleans, found {} and {}", l.name(), r.name()));
                        }
                        Kind::Bool
                    }
                    BinOp::Eq | BinOp::Neq => {
                        if l != Kind::Unknown && r != Kind::Unknown && l != r {
                            push(errors, format!("'{sym}' compares mismatched types: {} and {}", l.name(), r.name()));
                        }
                        Kind::Bool
                    }
                }
            }
            Expr::Call { name, args } => {
                if name == "parallel_map" {
                    // Already validated by check_parallel_map; just infer the array arg.
                    if args.len() == 2 {
                        infer_expr(&args[1], locals, fns, span, errors);
                    }
                    return Kind::Array;
                }
                if let Some(callee) = fns.get(name.as_str()) {
                    if callee.params.len() != args.len() {
                        push(errors, format!(
                            "'{name}' expects {} argument{}, got {}",
                            callee.params.len(),
                            if callee.params.len() == 1 { "" } else { "s" },
                            args.len()
                        ));
                    }
                }
                for a in args {
                    infer_expr(a, locals, fns, span, errors);
                }
                Kind::Unknown // return kind isn't tracked yet
            }
        }
    }

    fn visit_stmt(s: &Stmt, locals: &mut HashMap<String, Kind>, fns: &HashMap<&str, &Fn>, errors: &mut Vec<CheckError>) {
        match s {
            Stmt::Let { name, value, span } => {
                let k = infer_expr(value, locals, fns, *span, errors);
                locals.entry(name.clone()).or_insert(k);
            }
            Stmt::Assign { name, value, span } => {
                let k = infer_expr(value, locals, fns, *span, errors);
                if let Some(&prior) = locals.get(name) {
                    if prior != Kind::Unknown && k != Kind::Unknown && prior != k {
                        errors.push(CheckError {
                            message: format!(
                                "'{name}' was first bound as {}, can't assign {} to it",
                                prior.name(),
                                k.name()
                            ),
                            span: *span,
                        });
                    }
                }
            }
            Stmt::If { cond, then_block, else_block, span } => {
                let k = infer_expr(cond, locals, fns, *span, errors);
                if k != Kind::Unknown && k != Kind::Bool {
                    errors.push(CheckError {
                        message: format!("if-condition must be a boolean expression, found {}", k.name()),
                        span: *span,
                    });
                }
                for st in then_block {
                    visit_stmt(st, locals, fns, errors);
                }
                if let Some(eb) = else_block {
                    for st in eb {
                        visit_stmt(st, locals, fns, errors);
                    }
                }
            }
            Stmt::While { cond, body, span } => {
                let k = infer_expr(cond, locals, fns, *span, errors);
                if k != Kind::Unknown && k != Kind::Bool {
                    errors.push(CheckError {
                        message: format!("while-condition must be a boolean expression, found {}", k.name()),
                        span: *span,
                    });
                }
                for st in body {
                    visit_stmt(st, locals, fns, errors);
                }
            }
            Stmt::Print { args, span } => {
                for a in args {
                    infer_expr(a, locals, fns, *span, errors);
                }
            }
            Stmt::Return { value, span } => {
                if let Some(v) = value {
                    infer_expr(v, locals, fns, *span, errors);
                }
            }
            Stmt::ExprStmt { expr, span } => {
                infer_expr(expr, locals, fns, *span, errors);
            }
        }
    }

    for fn_ in program {
        let mut locals: HashMap<String, Kind> = HashMap::new();
        let mut fn_errors = Vec::new();
        for s in &fn_.body {
            visit_stmt(s, &mut locals, &fns, &mut fn_errors);
        }
        for e in fn_errors {
            errors.push(CheckError { message: format!("in '{}': {}", fn_.name, e.message), span: e.span });
        }
    }
    errors
}
