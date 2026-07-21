// Name resolution: one pass, run once, right after parsing and before
// every checker — purity.rs, typecheck.rs, codegen.rs, and
// wasm_codegen.rs each used to build their own `HashMap<Symbol, &Fn>`
// from the same program and (in codegen.rs/wasm_codegen.rs only, and
// duplicated between the two backends) catch unknown identifiers/calls
// on their own, at codegen time — the last possible moment, and only on
// whichever backend you happened to be compiling to. purity.rs and
// typecheck.rs never checked names at all: a typo'd variable or a call
// to a function that doesn't exist passed both silently (the read side
// of `resolve_expr` below is the only thing that used to catch that,
// and only inside codegen). This module builds the function table once
// for every stage to share, and does the one thing nothing else did:
// catch two functions sharing a name — previously silent (last
// definition wins, HashMap::collect) except for a confusing internal
// linker error ("Duplicate definition of identifier:
// __kprofile_counter_<name>") surfacing from deep inside codegen with
// no useful position — plus unknown identifiers/calls, now reported
// together with (not instead of) purity/type errors in one pass instead
// of only surfacing on whichever backend gets compiled.
//
// Scope, honestly: a `where` clause's expression is deliberately not
// resolved here — it isn't checked by purity.rs or typecheck.rs either
// (where_info.rs reads it at codegen time only), so leaving it alone
// keeps this pass from being stricter than the rest of the compiler.

use crate::ast::*;
use crate::error::{ErrorKind, KestrelcError};
use crate::interner::Symbol;
use crate::span::Span;
use std::collections::{HashMap, HashSet};

/// The whole-program function table every later stage (purity, type,
/// codegen) needs — built once here instead of each stage re-collecting
/// its own identical copy. A program with a duplicate function name
/// keeps only the last definition (last write wins, same as every
/// per-stage `.collect()` this replaces already did) — `resolve` below
/// is what actually reports that as an error; this just mirrors the
/// same fallback behavior for the table itself.
pub fn build_fn_table(program: &Program) -> HashMap<Symbol, &Fn> {
    program.fns.iter().map(|f| (f.name, f)).collect()
}

/// Resolves every name in `program` against `fns` (see `build_fn_table`)
/// and each function's own locals, returning every problem found rather
/// than stopping at the first — same "report everything, not just the
/// first mistake" contract as `check_purity`/`check_types`.
pub fn resolve(program: &Program, fns: &HashMap<Symbol, &Fn>) -> Vec<KestrelcError> {
    let mut errors = Vec::new();
    check_duplicate_fns(program, &mut errors);
    for fn_ in &program.fns {
        resolve_fn(fn_, fns, &mut errors);
    }
    errors
}

fn check_duplicate_fns(program: &Program, errors: &mut Vec<KestrelcError>) {
    let mut seen: HashSet<Symbol> = HashSet::new();
    for f in &program.fns {
        if !seen.insert(f.name) {
            errors.push(KestrelcError::new(
                ErrorKind::Resolve,
                format!("'{}' is defined more than once", f.name),
                f.span,
            ));
        }
    }
}

fn resolve_fn(fn_: &Fn, fns: &HashMap<Symbol, &Fn>, errors: &mut Vec<KestrelcError>) {
    // Flat, non-block-scoped locals — a `let` inside an `if`/`while` is
    // visible for the rest of the function, matching every other pass's
    // (and every backend's runtime) existing scoping rule.
    let mut locals: HashSet<Symbol> = fn_.params.iter().map(|p| p.name).collect();
    for s in &fn_.body {
        resolve_stmt(s, &mut locals, fns, errors);
    }
}

// `span` is the enclosing statement's span, same statement-granularity
// tradeoff as purity.rs/typecheck.rs (see error.rs's doc comment).
fn resolve_expr(
    e: &Expr,
    locals: &HashSet<Symbol>,
    fns: &HashMap<Symbol, &Fn>,
    span: Span,
    errors: &mut Vec<KestrelcError>,
) {
    match &e.kind {
        ExprKind::Num(_) | ExprKind::Str(_) | ExprKind::Bool(_) => {}
        ExprKind::Ident(name) => {
            if !locals.contains(name) {
                // e.span, not the enclosing statement's span: an
                // identifier is one of the cases finer-grained than
                // statement-level Expr spans actually pays off for —
                // `print(a, b, c, d)` with one typo'd name now points at
                // that exact argument, not the whole print statement.
                errors.push(KestrelcError::new(
                    ErrorKind::Resolve,
                    format!("Unknown identifier '{name}'"),
                    e.span,
                ));
            }
        }
        ExprKind::ArrayLit(elems) => {
            for el in elems {
                resolve_expr(el, locals, fns, span, errors);
            }
        }
        ExprKind::Unary { expr, .. } => resolve_expr(expr, locals, fns, span, errors),
        ExprKind::Binop { left, right, .. } => {
            resolve_expr(left, locals, fns, span, errors);
            resolve_expr(right, locals, fns, span, errors);
        }
        ExprKind::Index { target, index } => {
            resolve_expr(target, locals, fns, span, errors);
            resolve_expr(index, locals, fns, span, errors);
        }
        ExprKind::Call { name, args } => {
            if &*name.resolve() == "parallel_map" {
                // Its first argument is a bare function name, not a
                // variable reference — check_parallel_map (purity.rs)
                // already validates it exists, is pure, and has the
                // right shape; resolving it here too would just
                // duplicate that check under a different error kind.
                // Mirrors typecheck.rs's infer_expr, which special-cases
                // this the same way.
                if args.len() == 2 {
                    resolve_expr(&args[1], locals, fns, span, errors);
                }
                return;
            }
            if !fns.contains_key(name) {
                errors.push(KestrelcError::new(
                    ErrorKind::Resolve,
                    format!("Unknown function '{name}'"),
                    e.span,
                ));
            }
            for a in args {
                resolve_expr(a, locals, fns, span, errors);
            }
        }
    }
}

fn resolve_stmt(
    s: &Stmt,
    locals: &mut HashSet<Symbol>,
    fns: &HashMap<Symbol, &Fn>,
    errors: &mut Vec<KestrelcError>,
) {
    match s {
        Stmt::Let { name, value, span } => {
            resolve_expr(value, locals, fns, *span, errors);
            locals.insert(*name);
        }
        Stmt::Assign { name, value, span } => {
            resolve_expr(value, locals, fns, *span, errors);
            if !locals.contains(name) {
                errors.push(KestrelcError::new(
                    ErrorKind::Resolve,
                    format!("Assignment to unknown variable '{name}'"),
                    *span,
                ));
            }
        }
        Stmt::If { cond, then_block, else_block, span } => {
            resolve_expr(cond, locals, fns, *span, errors);
            for st in then_block {
                resolve_stmt(st, locals, fns, errors);
            }
            if let Some(eb) = else_block {
                for st in eb {
                    resolve_stmt(st, locals, fns, errors);
                }
            }
        }
        Stmt::While { cond, body, span } => {
            resolve_expr(cond, locals, fns, *span, errors);
            for st in body {
                resolve_stmt(st, locals, fns, errors);
            }
        }
        Stmt::Print { args, span } => {
            for a in args {
                resolve_expr(a, locals, fns, *span, errors);
            }
        }
        Stmt::Return { value, span } => {
            if let Some(v) = value {
                resolve_expr(v, locals, fns, *span, errors);
            }
        }
        Stmt::ExprStmt { expr, span } => resolve_expr(expr, locals, fns, *span, errors),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;

    fn resolve_src(src: &str) -> Vec<KestrelcError> {
        let program = parse(lex(src).unwrap()).unwrap();
        let fns = build_fn_table(&program);
        resolve(&program, &fns)
    }

    #[test]
    fn accepts_a_well_formed_program() {
        let errors = resolve_src(
            "pure fn square(x: i64) -> i64 { return x * x; }\nfn main() { print(square(3)); }",
        );
        assert!(errors.is_empty());
    }

    #[test]
    fn catches_an_unknown_identifier_read() {
        let errors = resolve_src("fn main() { print(y); }");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("Unknown identifier 'y'"));
    }

    #[test]
    fn catches_an_unknown_function_call() {
        let errors = resolve_src("fn main() { print(missing(1)); }");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("Unknown function 'missing'"));
    }

    #[test]
    fn catches_assignment_to_a_never_declared_variable() {
        let errors = resolve_src("fn main() { x = 5; print(x); }");
        // Two: the assignment itself, and the read that follows it —
        // `x` still isn't a local even after the (rejected) assignment.
        assert_eq!(errors.len(), 2);
        assert!(errors[0].message.contains("Assignment to unknown variable 'x'"));
    }

    #[test]
    fn catches_two_functions_sharing_a_name() {
        let errors = resolve_src(
            "fn square(x: i64) -> i64 { return x * x; }\nfn square(x: i64) -> i64 { return x; }\nfn main() { print(square(2)); }",
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("'square' is defined more than once"));
    }

    #[test]
    fn a_let_inside_an_if_is_visible_for_the_rest_of_the_function() {
        // Matches the language's flat, non-block-scoped locals — see
        // kestrel.js's interpret() and this file's resolve_fn doc comment.
        let errors = resolve_src(
            "fn main() { if (1 < 2) { let x = 1; } print(x); }",
        );
        assert!(errors.is_empty());
    }

    #[test]
    fn parallel_maps_first_argument_is_not_treated_as_a_variable_reference() {
        let errors = resolve_src(
            "pure fn inc(x: i64) -> i64 { return x + 1; }\nfn main() { let a = [1, 2, 3]; let b = parallel_map(inc, a); print(b); }",
        );
        assert!(errors.is_empty());
    }
}
