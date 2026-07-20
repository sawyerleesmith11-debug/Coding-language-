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
// Deliberately narrow, matching the JS version for the shape that
// matters (an intermediate array used more than once, or a source
// that isn't a bare parallel_map call, are still left unfused rather
// than guessed at) but with one real generalization past the JS
// version and kestrelc's own original port: the two `let`s no longer
// need to be textually adjacent. A statement sitting between them is
// left exactly where it is, untouched, as long as it doesn't reference
// the intermediate array — safe because `parallel_map`'s callback is
// required `pure` (no I/O, no observable effect beyond its return
// value), so *when* `a`/`b` actually get computed relative to an
// unrelated statement can't change what the program observes. Chains
// still fuse transitively (a 3-deep chain collapses to one function),
// and it still recurses into if/while bodies, not just top-level
// statements.

use crate::ast::*;
use crate::interner::{self, Symbol};
use crate::span::Span;
use std::collections::HashMap;

/// Synthesized statements/functions (the fused body, the re-introduced
/// array `let`) have no real source position — this sentinel span makes
/// that explicit rather than borrowing an unrelated position.
const SYNTHESIZED: Span = Span { line: 0, col: 0, len: 0 };

fn count_ident_refs(stmts: &[Stmt], name: Symbol) -> usize {
    let mut count = 0;
    count_ident_refs_stmts(stmts, name, &mut count);
    count
}

fn count_ident_refs_stmts(stmts: &[Stmt], name: Symbol, count: &mut usize) {
    for s in stmts {
        match s {
            Stmt::Let { value, .. } => count_ident_refs_expr(value, name, count),
            Stmt::Assign { name: n, value, .. } => {
                if *n == name {
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

fn count_ident_refs_expr(e: &Expr, name: Symbol, count: &mut usize) {
    match e {
        Expr::Ident(n) => {
            if *n == name {
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
fn as_parallel_map_call(e: &Expr) -> Option<(Symbol, &Expr)> {
    if let Expr::Call { name, args } = e {
        if &*name.resolve() == "parallel_map" && args.len() == 2 {
            if let Expr::Ident(f) = &args[0] {
                return Some((*f, &args[1]));
            }
        }
    }
    None
}

/// A match starting at statement index `i`: `body[i]` produces `a`, and
/// `body[j]` (the *first* later statement in this same block that's
/// `let b = parallel_map(g, a)` — not necessarily `i + 1`) consumes it.
/// Everything strictly between `i` and `j` is left alone by the caller;
/// this only finds the pairing. `fuse_body`'s existing whole-body
/// `count_ident_refs(body, a_name) != 1` check (unchanged) is what
/// actually rejects the match if `a` is referenced anywhere else too —
/// including by one of the statements between `i` and `j` — so this
/// function doesn't need its own escape-analysis over that range.
struct Match {
    j: usize,
    a_name: Symbol,
    b_name: Symbol,
    f_name: Symbol,
    g_name: Symbol,
    arr_arg: Expr,
}

fn match_fusion(body: &[Stmt], i: usize) -> Option<Match> {
    let Stmt::Let { name: a_name, value: v1, .. } = &body[i] else {
        return None;
    };
    let (f_name, arr_arg) = as_parallel_map_call(v1)?;
    for (j, s) in body.iter().enumerate().skip(i + 1) {
        let Stmt::Let { name: b_name, value: v2, .. } = s else {
            continue;
        };
        let Some((g_name, arr_arg2)) = as_parallel_map_call(v2) else {
            continue;
        };
        if matches!(arr_arg2, Expr::Ident(a2) if a2 == a_name) {
            return Some(Match { j, a_name: *a_name, b_name: *b_name, f_name, g_name, arr_arg: arr_arg.clone() });
        }
    }
    None
}

fn fuse_body(
    body: &mut Vec<Stmt>,
    fns: &mut HashMap<Symbol, Fn>,
    extra_fns: &mut Vec<Fn>,
    counter: &mut usize,
) {
    let mut i = 0;
    while i < body.len() {
        let Some(m) = match_fusion(body, i) else {
            i += 1;
            continue;
        };
        // The only allowed reference to `a` is the one inside the
        // second let's own parallel_map call (that's the "1" this
        // counts) — anything more means `a` escapes this fusion and
        // must stay materialized.
        if count_ident_refs(body, m.a_name) != 1 {
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

        let fused_name = interner::intern(&format!("__fused_{}_{}_{}", *counter, m.f_name, m.g_name));
        *counter += 1;
        let x_sym = interner::intern("__x");
        let fused_fn = Fn {
            name: fused_name,
            pure: true,
            params: vec![Param { name: x_sym, ty: f_fn.params[0].ty.clone() }],
            return_type: g_fn.return_type.clone(),
            where_clause: None,
            body: vec![Stmt::Return {
                value: Some(Expr::Call {
                    name: m.g_name,
                    args: vec![Expr::Call { name: m.f_name, args: vec![Expr::Ident(x_sym)] }],
                }),
                span: SYNTHESIZED,
            }],
            span: SYNTHESIZED,
        };
        // Register the synthesized function too, so a third chained
        // parallel_map (whose callee is now *this* fused function, not
        // an original source function) resolves when this same slot is
        // re-checked below.
        fns.insert(fused_name, fused_fn.clone());
        extra_fns.push(fused_fn);

        // kestrelc's codegen requires a parallel_map array argument to
        // be a plain identifier bound via a literal-length `let` — never
        // an inline array literal (see codegen.rs's static_array_len).
        // If the source array isn't already such an identifier (e.g.
        // it's the literal from the first parallel_map's own call),
        // reintroduce a `let` binding for it instead of inlining it.
        let mut replacement: Vec<Stmt> = Vec::new();
        let array_ident = match &m.arr_arg {
            Expr::Ident(name) => Expr::Ident(*name),
            _ => {
                replacement.push(Stmt::Let { name: m.a_name, value: m.arr_arg, span: SYNTHESIZED });
                Expr::Ident(m.a_name)
            }
        };
        replacement.push(Stmt::Let {
            name: m.b_name,
            value: Expr::Call {
                name: interner::intern("parallel_map"),
                args: vec![Expr::Ident(fused_name), array_ident],
            },
            span: SYNTHESIZED,
        });
        // `j` is strictly after `i`, so removing it first doesn't
        // invalidate `i` — leaves body[0..i] ++ (old body[i+1..j], the
        // untouched interposed statements, unaffected) ++ body[j+1..],
        // then the splice below replaces just the old body[i] with the
        // fused version, preserving every interposed statement's exact
        // original relative order.
        body.remove(m.j);
        body.splice(i..i + 1, replacement);
        // Re-check this same slot: a third chained parallel_map may now
        // be reachable from here (possibly still not adjacent).
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
    let mut fns: HashMap<Symbol, Fn> = program.iter().map(|f| (f.name, f.clone())).collect();
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
        assert!(fused.iter().any(|f| f.name.resolve().starts_with("__fused_")));
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
    fn fuses_across_an_unrelated_statement_between_the_two_lets() {
        // The generalization past the JS version: `a` and `b`'s lets
        // don't have to be textually adjacent. `print("hi")` here
        // doesn't reference `a` at all, so it's safe to leave exactly
        // where it is while `a`/`b` still fuse into one function.
        // `src` pre-bound (rather than an inline array literal) so the
        // fusion's own replacement is exactly one statement — keeps this
        // test's body-layout assertion below about the interposed print
        // independent of the separate literal-rebinding behavior
        // (covered by the pre-existing fused-two-calls test instead).
        let program = parse_src(
            r#"
            pure fn square(x: i32) -> i32 { return x * x; }
            pure fn inc(x: i32) -> i32 { return x + 1; }
            fn main() {
                let src = [1, 2, 3, 4];
                let a = parallel_map(square, src);
                print("hi");
                let b = parallel_map(inc, a);
                print(b[0]);
            }
            "#,
        );
        let fused = fuse_loops(&program);
        assert_eq!(fused.len(), 4, "should have added exactly one fused function");
        assert!(fused.iter().any(|f| f.name.resolve().starts_with("__fused_")));
        // The interposed print must still be there, untouched, and must
        // still be the statement right after whatever now occupies the
        // fused `let a = ...`'s old slot.
        let main_fn = fused.iter().find(|f| &*f.name.resolve() == "main").unwrap();
        assert!(matches!(&main_fn.body[2], Stmt::Print { .. }), "interposed print should be preserved in place");
    }

    #[test]
    fn does_not_fuse_when_an_interposed_statement_reads_the_intermediate_array() {
        // Same shape as the test above, but the interposed statement
        // *does* reference `a` — that's an escaping use (the same rule
        // that already blocks fusion when `a` is read after both lets),
        // so no fusion should happen here even though `a`'s producer and
        // `b`'s consumer are still findable.
        let program = parse_src(
            r#"
            pure fn square(x: i32) -> i32 { return x * x; }
            pure fn inc(x: i32) -> i32 { return x + 1; }
            fn main() {
                let a = parallel_map(square, [1, 2, 3, 4]);
                print(a[0]);
                let b = parallel_map(inc, a);
                print(b[0]);
            }
            "#,
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
