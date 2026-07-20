// AST-to-AST loop fusion — a direct Rust port of kestrel.js's
// fuseLoops(), same exact scope and safety argument (see
// kestrel-DESIGN.md): `let a = parallel_map(f, arr); let b =
// parallel_map(g, a);`, with `a` referenced nowhere else in the
// function, becomes a single parallel_map over `arr` with a
// synthesized pure fn computing `g(f(x))` — one pass and no
// intermediate array instead of two. Safe because `f` and `g` are
// already proven pure by the time this runs (called after
// check_purity/check_parallel_map/check_types all pass on the
// *original* program), and the synthesized fn is a trivial composition
// of two already-pure functions, not a new proof.
//
// Deliberately narrow, matching the JS version: only this exact
// adjacent-statement shape triggers it. A chain split across other
// statements, an intermediate array used more than once, or a source
// that isn't a bare parallel_map call are all left unfused rather than
// guessed at. Chains fuse transitively (a 3-deep chain collapses to one
// function), and it recurses into if/while bodies too, not just
// top-level statements.

use crate::ast::*;
use crate::span::Span;
use std::collections::HashMap;

/// Synthesized statements/functions (the fused body, the re-introduced
/// array `let`) have no real source position — this sentinel span makes
/// that explicit rather than borrowing an unrelated position.
const SYNTHESIZED: Span = Span { line: 0, col: 0, len: 0 };

fn count_ident_refs(stmts: &[Stmt], name: &str) -> usize {
    let mut count = 0;
    count_ident_refs_stmts(stmts, name, &mut count);
    count
}

fn count_ident_refs_stmts(stmts: &[Stmt], name: &str, count: &mut usize) {
    for s in stmts {
        match s {
            Stmt::Let { value, .. } => count_ident_refs_expr(value, name, count),
            Stmt::Assign { name: n, value, .. } => {
                if n == name {
                    *count += 1;
                }
                count_ident_refs_expr(value, name, count);
            }
            Stmt::If { cond, then_block, else_block, .. } => {
                count_ident_refs_expr(cond, name, count);
                count_ident_refs_stmts(then_block, name, count);
                if let Some(eb) = else_block {
                    count_ident_refs_stmts(eb, name, count);
                }
            }
            Stmt::While { cond, body, .. } => {
                count_ident_refs_expr(cond, name, count);
                count_ident_refs_stmts(body, name, count);
            }
            Stmt::Print { args, .. } => {
                for a in args {
                    count_ident_refs_expr(a, name, count);
                }
            }
            Stmt::Return { value, .. } => {
                if let Some(v) = value {
                    count_ident_refs_expr(v, name, count);
                }
            }
            Stmt::ExprStmt { expr, .. } => count_ident_refs_expr(expr, name, count),
        }
    }
}

fn count_ident_refs_expr(e: &Expr, name: &str, count: &mut usize) {
    match e {
        Expr::Ident(n) => {
            if n == name {
                *count += 1;
            }
        }
        Expr::Call { args, .. } => {
            for a in args {
                count_ident_refs_expr(a, name, count);
            }
        }
        Expr::Binop { left, right, .. } => {
            count_ident_refs_expr(left, name, count);
            count_ident_refs_expr(right, name, count);
        }
        Expr::Unary { expr, .. } => count_ident_refs_expr(expr, name, count),
        Expr::Index { target, index } => {
            count_ident_refs_expr(target, name, count);
            count_ident_refs_expr(index, name, count);
        }
        Expr::ArrayLit(elems) => {
            for el in elems {
                count_ident_refs_expr(el, name, count);
            }
        }
        _ => {}
    }
}

/// If `e` is `parallel_map(<bare ident>, <anything>)`, returns
/// (callee name, array-arg expression).
fn as_parallel_map_call(e: &Expr) -> Option<(&str, &Expr)> {
    if let Expr::Call { name, args } = e {
        if name == "parallel_map" && args.len() == 2 {
            if let Expr::Ident(f) = &args[0] {
                return Some((f.as_str(), &args[1]));
            }
        }
    }
    None
}

/// A match at statement index `i`: the two adjacent `let`s can fuse.
struct Match {
    a_name: String,
    b_name: String,
    f_name: String,
    g_name: String,
    arr_arg: Expr,
}

fn match_fusion(body: &[Stmt], i: usize) -> Option<Match> {
    let (Stmt::Let { name: a_name, value: v1, .. }, Stmt::Let { name: b_name, value: v2, .. }) =
        (&body[i], &body[i + 1])
    else {
        return None;
    };
    let (f_name, arr_arg) = as_parallel_map_call(v1)?;
    let (g_name, arr_arg2) = as_parallel_map_call(v2)?;
    let Expr::Ident(a2) = arr_arg2 else { return None };
    if a2 != a_name {
        return None;
    }
    Some(Match {
        a_name: a_name.clone(),
        b_name: b_name.clone(),
        f_name: f_name.to_string(),
        g_name: g_name.to_string(),
        arr_arg: arr_arg.clone(),
    })
}

fn fuse_body(
    body: &mut Vec<Stmt>,
    fns: &mut HashMap<String, Fn>,
    extra_fns: &mut Vec<Fn>,
    counter: &mut usize,
) {
    let mut i = 0;
    while i + 1 < body.len() {
        let Some(m) = match_fusion(body, i) else {
            i += 1;
            continue;
        };
        // The only allowed reference to `a` is the one inside the
        // second let's own parallel_map call (that's the "1" this
        // counts) — anything more means `a` escapes this fusion and
        // must stay materialized.
        if count_ident_refs(body, &m.a_name) != 1 {
            i += 1;
            continue;
        }
        let (Some(f_fn), Some(g_fn)) = (fns.get(&m.f_name), fns.get(&m.g_name)) else {
            i += 1;
            continue;
        };
        if !f_fn.pure || !g_fn.pure || f_fn.params.len() != 1 || g_fn.params.len() != 1 {
            i += 1;
            continue;
        }

        let fused_name = format!("__fused_{}_{}_{}", *counter, m.f_name, m.g_name);
        *counter += 1;
        let fused_fn = Fn {
            name: fused_name.clone(),
            pure: true,
            params: vec![Param { name: "__x".to_string(), ty: f_fn.params[0].ty.clone() }],
            return_type: g_fn.return_type.clone(),
            where_clause: None,
            body: vec![Stmt::Return {
                value: Some(Expr::Call {
                    name: m.g_name.clone(),
                    args: vec![Expr::Call {
                        name: m.f_name.clone(),
                        args: vec![Expr::Ident("__x".to_string())],
                    }],
                }),
                span: SYNTHESIZED,
            }],
            span: SYNTHESIZED,
        };
        // Register the synthesized function too, so a third chained
        // parallel_map (whose callee is now *this* fused function, not
        // an original source function) resolves when this same slot is
        // re-checked below.
        fns.insert(fused_name.clone(), fused_fn.clone());
        extra_fns.push(fused_fn);

        // kestrelc's codegen requires a parallel_map array argument to
        // be a plain identifier bound via a literal-length `let` — never
        // an inline array literal (see codegen.rs's static_array_len).
        // If the source array isn't already such an identifier (e.g.
        // it's the literal from the first parallel_map's own call),
        // reintroduce a `let` binding for it instead of inlining it.
        let mut replacement: Vec<Stmt> = Vec::new();
        let array_ident = match &m.arr_arg {
            Expr::Ident(name) => Expr::Ident(name.clone()),
            _ => {
                replacement.push(Stmt::Let { name: m.a_name.clone(), value: m.arr_arg, span: SYNTHESIZED });
                Expr::Ident(m.a_name.clone())
            }
        };
        replacement.push(Stmt::Let {
            name: m.b_name,
            value: Expr::Call {
                name: "parallel_map".to_string(),
                args: vec![Expr::Ident(fused_name), array_ident],
            },
            span: SYNTHESIZED,
        });
        body.splice(i..i + 2, replacement);
        // Re-check this same slot: a third chained parallel_map now
        // sits right after it.
    }

    for s in body.iter_mut() {
        match s {
            Stmt::If { then_block, else_block, .. } => {
                fuse_body(then_block, fns, extra_fns, counter);
                if let Some(eb) = else_block {
                    fuse_body(eb, fns, extra_fns, counter);
                }
            }
            Stmt::While { body: wbody, .. } => fuse_body(wbody, fns, extra_fns, counter),
            _ => {}
        }
    }
}

pub fn fuse_loops(program: &Program) -> Program {
    let mut fns: HashMap<String, Fn> = program.iter().map(|f| (f.name.clone(), f.clone())).collect();
    let mut extra_fns: Vec<Fn> = Vec::new();
    let mut counter: usize = 0;

    let mut new_program: Program = program.clone();
    for f in new_program.iter_mut() {
        fuse_body(&mut f.body, &mut fns, &mut extra_fns, &mut counter);
    }
    new_program.extend(extra_fns);
    new_program
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;

    fn parse_src(src: &str) -> Program {
        parse(lex(src).unwrap()).unwrap()
    }

    #[test]
    fn fuses_two_chained_parallel_map_calls_into_one_function() {
        let program = parse_src(
            "
            pure fn square(x: i32) -> i32 { return x * x; }
            pure fn inc(x: i32) -> i32 { return x + 1; }
            fn main() {
                let a = parallel_map(square, [1, 2, 3, 4]);
                let b = parallel_map(inc, a);
                print(b[0]);
            }
            ",
        );
        let fused = fuse_loops(&program);
        assert_eq!(fused.len(), 4, "should have added exactly one fused function");
        assert!(fused.iter().any(|f| f.name.starts_with("__fused_")));
    }

    #[test]
    fn fuses_a_three_deep_chain_down_to_one_function() {
        let program = parse_src(
            "
            pure fn a1(x: i32) -> i32 { return x + 1; }
            pure fn a2(x: i32) -> i32 { return x * 2; }
            pure fn a3(x: i32) -> i32 { return x - 3; }
            fn main() {
                let p = parallel_map(a1, [1, 2, 3]);
                let q = parallel_map(a2, p);
                let r = parallel_map(a3, q);
                print(r[0]);
            }
            ",
        );
        let fused = fuse_loops(&program);
        // 3 originals + main + two fusion stages collapsing down to one.
        assert_eq!(fused.len(), 6);
    }

    #[test]
    fn does_not_fuse_when_the_intermediate_array_is_used_more_than_once() {
        let program = parse_src(
            "
            pure fn sq(x: i32) -> i32 { return x * x; }
            pure fn inc(x: i32) -> i32 { return x + 1; }
            fn main() {
                let a = parallel_map(sq, [1, 2, 3]);
                let b = parallel_map(inc, a);
                print(a[0], b[0]);
            }
            ",
        );
        let fused = fuse_loops(&program);
        assert_eq!(fused.len(), 3, "no fused function should be added");
    }

    #[test]
    fn leaves_a_single_unchained_parallel_map_alone() {
        let program = parse_src(
            "
            pure fn square(x: i32) -> i32 { return x * x; }
            fn main() {
                let a = parallel_map(square, [1, 2, 3]);
                print(a[0]);
            }
            ",
        );
        let fused = fuse_loops(&program);
        assert_eq!(fused.len(), 2);
    }
}
