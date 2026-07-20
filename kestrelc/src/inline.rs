// Profile-guided inlining: the codegen half of kestrel-DESIGN.md idea
// #1's runtime feedback loop (see profile.rs for the "how do we know
// what's hot" half). Deliberately narrow scope, same spirit as
// where_info.rs's bounds-proof prover — a small, always-safe subset
// handled for real, rather than a general inliner attempted and
// half-working:
//
//   - only a `pure fn` with an *expression body* (exactly one `return
//     expr;` statement — no lets, no if/while, no intermediate
//     statements) is a candidate at all: substituting call arguments
//     into it is then just "replace each parameter identifier with the
//     argument expression," no fresh-variable/alpha-renaming machinery
//     needed, since there's no local variable to collide with a caller's
//     own names in the first place.
//   - only scalar parameters — an array parameter would need the
//     (pointer, length) two-value substitution codegen.rs's Slot enum
//     exists for, which this AST-level pass has no access to.
//   - never a function that's ever passed as parallel_map's callback
//     anywhere in the program — the runtime shim calls it through a real
//     function pointer, so it must keep existing as an actual compiled
//     function no matter how "hot" or small it is.
//   - never self-recursive (would substitute forever).
//   - not transitive: if hot function A's body calls hot function B,
//     inlining A's call sites splices in a body that still calls B as a
//     real function — B isn't further inlined into that copy. Avoids
//     needing cycle detection across the whole call graph for a first
//     pass; A and B are each still inlined at *their own* call sites
//     elsewhere in the program.
//   - driven by a real runtime call-count profile (see profile.rs), not
//     a static guess — a function only becomes a candidate once an
//     actual previous run recorded it being called at least
//     `HOT_CALL_THRESHOLD` times.

use crate::ast::*;
use std::collections::{HashMap, HashSet};

/// How many times a previous run must have actually called a function
/// before it's considered worth inlining. Arbitrary but not
/// meaningless: below this, the call-overhead savings are unlikely to
/// matter, and inlining always costs code size.
const HOT_CALL_THRESHOLD: u64 = 5;

struct Candidate {
    params: Vec<String>,
    body: Expr,
}

fn expr_calls(e: &Expr, name: &str, found: &mut bool) {
    if *found {
        return;
    }
    match e {
        Expr::Call { name: n, args } => {
            if n == name {
                *found = true;
                return;
            }
            for a in args {
                expr_calls(a, name, found);
            }
        }
        Expr::Unary { expr, .. } => expr_calls(expr, name, found),
        Expr::Binop { left, right, .. } => {
            expr_calls(left, name, found);
            expr_calls(right, name, found);
        }
        Expr::Index { target, index } => {
            expr_calls(target, name, found);
            expr_calls(index, name, found);
        }
        Expr::ArrayLit(elems) => {
            for el in elems {
                expr_calls(el, name, found);
            }
        }
        Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Ident(_) => {}
    }
}

fn calls_name(e: &Expr, name: &str) -> bool {
    let mut found = false;
    expr_calls(e, name, &mut found);
    found
}

fn walk_stmts_exprs<'a>(stmts: &'a [Stmt], on_expr: &mut impl FnMut(&'a Expr)) {
    for s in stmts {
        match s {
            Stmt::Let { value, .. } | Stmt::Assign { value, .. } | Stmt::ExprStmt { expr: value, .. } => {
                on_expr(value)
            }
            Stmt::If { cond, then_block, else_block, .. } => {
                on_expr(cond);
                walk_stmts_exprs(then_block, on_expr);
                if let Some(eb) = else_block {
                    walk_stmts_exprs(eb, on_expr);
                }
            }
            Stmt::While { cond, body, .. } => {
                on_expr(cond);
                walk_stmts_exprs(body, on_expr);
            }
            Stmt::Print { args, .. } => {
                for a in args {
                    on_expr(a);
                }
            }
            Stmt::Return { value: Some(e), .. } => on_expr(e),
            Stmt::Return { value: None, .. } => {}
        }
    }
}

/// Every function name ever passed as `parallel_map`'s first (callback)
/// argument, anywhere in the program — see the module doc comment for
/// why those can never be inlined away. `pub(crate)` because codegen.rs
/// reuses this exact same set for a different reason: memoization
/// (kestrel-DESIGN.md's own idea #2/#4) needs its cache to never be
/// touched from more than one OS thread at once, and a function in this
/// set is exactly the one kind of pure function that's ever called from
/// a `parallel_map` worker thread — excluding it is what makes
/// memoization's cache lock-free-safe without ever needing a real lock.
pub(crate) fn collect_parallel_map_callbacks(program: &Program) -> HashSet<String> {
    let mut callbacks = HashSet::new();
    fn note_calls(e: &Expr, callbacks: &mut HashSet<String>) {
        if let Expr::Call { name, args } = e {
            if name == "parallel_map" {
                if let Some(Expr::Ident(cb)) = args.first() {
                    callbacks.insert(cb.clone());
                }
            }
            for a in args {
                note_calls(a, callbacks);
            }
        }
    }
    for f in program {
        walk_stmts_exprs(&f.body, &mut |e| note_calls(e, &mut callbacks));
    }
    callbacks
}

fn expression_body(f: &Fn) -> Option<&Expr> {
    if f.params.iter().any(|p| matches!(p.ty, Type::Array { .. })) {
        return None;
    }
    match f.body.as_slice() {
        [Stmt::Return { value: Some(e), .. }] => Some(e),
        _ => None,
    }
}

fn substitute(e: &Expr, subst: &HashMap<&str, &Expr>) -> Expr {
    match e {
        Expr::Ident(n) => subst.get(n.as_str()).map(|v| (*v).clone()).unwrap_or_else(|| e.clone()),
        Expr::Unary { op, expr } => Expr::Unary { op: *op, expr: Box::new(substitute(expr, subst)) },
        Expr::Binop { op, left, right } => Expr::Binop {
            op: *op,
            left: Box::new(substitute(left, subst)),
            right: Box::new(substitute(right, subst)),
        },
        Expr::Index { target, index } => Expr::Index {
            target: Box::new(substitute(target, subst)),
            index: Box::new(substitute(index, subst)),
        },
        Expr::Call { name, args } => Expr::Call {
            name: name.clone(),
            args: args.iter().map(|a| substitute(a, subst)).collect(),
        },
        Expr::ArrayLit(elems) => Expr::ArrayLit(elems.iter().map(|e| substitute(e, subst)).collect()),
        Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) => e.clone(),
    }
}

fn inline_expr(e: &Expr, candidates: &HashMap<String, Candidate>) -> Expr {
    match e {
        Expr::Call { name, args } => {
            let new_args: Vec<Expr> = args.iter().map(|a| inline_expr(a, candidates)).collect();
            if let Some(c) = candidates.get(name) {
                if c.params.len() == new_args.len() {
                    let subst: HashMap<&str, &Expr> =
                        c.params.iter().map(|p| p.as_str()).zip(new_args.iter()).collect();
                    return substitute(&c.body, &subst);
                }
            }
            Expr::Call { name: name.clone(), args: new_args }
        }
        Expr::Unary { op, expr } => Expr::Unary { op: *op, expr: Box::new(inline_expr(expr, candidates)) },
        Expr::Binop { op, left, right } => Expr::Binop {
            op: *op,
            left: Box::new(inline_expr(left, candidates)),
            right: Box::new(inline_expr(right, candidates)),
        },
        Expr::Index { target, index } => Expr::Index {
            target: Box::new(inline_expr(target, candidates)),
            index: Box::new(inline_expr(index, candidates)),
        },
        Expr::ArrayLit(elems) => Expr::ArrayLit(elems.iter().map(|e| inline_expr(e, candidates)).collect()),
        Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Ident(_) => e.clone(),
    }
}

fn inline_stmts(stmts: &[Stmt], candidates: &HashMap<String, Candidate>) -> Vec<Stmt> {
    stmts
        .iter()
        .map(|s| match s {
            Stmt::Let { name, value, line, col } => {
                Stmt::Let { name: name.clone(), value: inline_expr(value, candidates), line: *line, col: *col }
            }
            Stmt::Assign { name, value, line, col } => {
                Stmt::Assign { name: name.clone(), value: inline_expr(value, candidates), line: *line, col: *col }
            }
            Stmt::If { cond, then_block, else_block, line, col } => Stmt::If {
                cond: inline_expr(cond, candidates),
                then_block: inline_stmts(then_block, candidates),
                else_block: else_block.as_ref().map(|b| inline_stmts(b, candidates)),
                line: *line,
                col: *col,
            },
            Stmt::While { cond, body, line, col } => Stmt::While {
                cond: inline_expr(cond, candidates),
                body: inline_stmts(body, candidates),
                line: *line,
                col: *col,
            },
            Stmt::Print { args, line, col } => {
                Stmt::Print { args: args.iter().map(|a| inline_expr(a, candidates)).collect(), line: *line, col: *col }
            }
            Stmt::Return { value, line, col } => {
                Stmt::Return { value: value.as_ref().map(|e| inline_expr(e, candidates)), line: *line, col: *col }
            }
            Stmt::ExprStmt { expr, line, col } => {
                Stmt::ExprStmt { expr: inline_expr(expr, candidates), line: *line, col: *col }
            }
        })
        .collect()
}

/// Rewrites every call site to a "hot" (per `profile`), small,
/// expression-bodied pure function into that function's substituted
/// body. The original function definitions are left in the output
/// program unchanged (and still compiled) — inlining only removes call
/// *overhead* at the chosen sites, it doesn't try to prove any site is
/// the *only* caller and eliminate the standalone function.
pub fn inline_hot_fns(program: &Program, profile: &HashMap<String, u64>) -> Program {
    if profile.is_empty() {
        return program.clone();
    }
    let pmap_callbacks = collect_parallel_map_callbacks(program);
    let mut candidates: HashMap<String, Candidate> = HashMap::new();
    for f in program {
        if !f.pure || f.name == "main" || pmap_callbacks.contains(&f.name) {
            continue;
        }
        let Some(count) = profile.get(&f.name) else { continue };
        if *count < HOT_CALL_THRESHOLD {
            continue;
        }
        let Some(body) = expression_body(f) else { continue };
        if calls_name(body, &f.name) {
            continue; // self-recursive — would substitute forever
        }
        candidates.insert(f.name.clone(), Candidate {
            params: f.params.iter().map(|p| p.name.clone()).collect(),
            body: body.clone(),
        });
    }
    if candidates.is_empty() {
        return program.clone();
    }
    program
        .iter()
        .map(|f| Fn { body: inline_stmts(&f.body, &candidates), ..f.clone() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer, parser};

    fn parse(src: &str) -> Program {
        parser::parse(lexer::lex(src).unwrap()).unwrap()
    }

    #[test]
    fn inlines_a_hot_small_pure_fn_at_its_call_site() {
        let program = parse(
            "pure fn double(x: i64) -> i64 { return x * 2; }\nfn main() { let y = double(5); print(y); }",
        );
        let mut profile = HashMap::new();
        profile.insert("double".to_string(), 10);
        let out = inline_hot_fns(&program, &profile);
        let main_fn = out.iter().find(|f| f.name == "main").unwrap();
        match &main_fn.body[0] {
            Stmt::Let { value, .. } => {
                assert!(!calls_name(value, "double"), "expected 'double' inlined away, got {value:?}");
            }
            other => panic!("expected a let, got {other:?}"),
        }
    }

    #[test]
    fn leaves_a_call_alone_when_the_profile_never_saw_it_called_enough() {
        let program = parse(
            "pure fn double(x: i64) -> i64 { return x * 2; }\nfn main() { let y = double(5); print(y); }",
        );
        let mut profile = HashMap::new();
        profile.insert("double".to_string(), 1); // below HOT_CALL_THRESHOLD
        let out = inline_hot_fns(&program, &profile);
        let main_fn = out.iter().find(|f| f.name == "main").unwrap();
        match &main_fn.body[0] {
            Stmt::Let { value, .. } => assert!(calls_name(value, "double")),
            other => panic!("expected a let, got {other:?}"),
        }
    }

    #[test]
    fn never_inlines_a_function_used_as_a_parallel_map_callback() {
        let program = parse(
            "pure fn triple(x: i64) -> i64 { return x * 3; }\nfn main() { let xs = [1, 2, 3]; let ys = parallel_map(triple, xs); let z = triple(9); print(z); }",
        );
        let mut profile = HashMap::new();
        profile.insert("triple".to_string(), 50);
        let out = inline_hot_fns(&program, &profile);
        let main_fn = out.iter().find(|f| f.name == "main").unwrap();
        // The direct call `triple(9)` must stay a real call — the
        // function still needs to exist as a real function for
        // parallel_map's function-pointer call to work.
        let last_let = main_fn
            .body
            .iter()
            .find_map(|s| match s {
                Stmt::Let { name, value, .. } if name == "z" => Some(value),
                _ => None,
            })
            .unwrap();
        assert!(calls_name(last_let, "triple"));
    }

    #[test]
    fn does_not_inline_a_self_recursive_function() {
        // Not a real expression-bodied recursive example (the grammar
        // needs a base case), but calls_name's guard is what's under
        // test: a body that mentions its own name is never a candidate.
        let program = parse(
            "pure fn weird(x: i64) -> i64 { return weird(x); }\nfn main() { let y = weird(5); print(y); }",
        );
        let mut profile = HashMap::new();
        profile.insert("weird".to_string(), 50);
        let out = inline_hot_fns(&program, &profile);
        let main_fn = out.iter().find(|f| f.name == "main").unwrap();
        match &main_fn.body[0] {
            Stmt::Let { value, .. } => assert!(calls_name(value, "weird")),
            other => panic!("expected a let, got {other:?}"),
        }
    }
}
