// AST-to-AST common-subexpression elimination for repeated pure-function
// calls within one straight-line statement list — e.g. `let a = f(x); ...
// total = total + f(x);` becomes `let a = f(x); ... total = total + a;`,
// removing the second call entirely. Safe because `f` is proven pure by
// the time this runs (called after check_purity passes on the original
// program, same precondition fusion.rs's fuse_loops already relies on),
// so replacing a repeated call with the first call's already-computed
// result can't change what the program observes.
//
// Deliberately narrow, matching fusion.rs's own posture: only a call
// that is the *entire* value of a `let` statement seeds a reusable
// entry (that `let`'s own name is already a safe variable to reuse —
// no new statement needs inserting). A repeated call nested inside a
// larger expression (e.g. `f(x) + f(x)` with neither occurrence
// let-bound) is left alone rather than hoisted into a synthesized
// temporary — real, doable, but a separate, larger piece of work (needs
// inserting new statements into the block, not just rewriting
// expressions) than this pass's scope covers.
//
// An entry stays valid only within the same straight-line block it was
// created in: entering an `if`/`while` body starts a fresh, empty table
// (discarded on exit, never merged back), since a variable's value in a
// branch that may or may not run — or a loop body that may run more
// than once — isn't safely assumed equal to its value before the
// branch/loop. An entry is also dropped the moment any statement
// (re)binds an identifier that entry's argument expression refers to —
// conservative on purpose: reassigning a variable might not actually
// change its value, but this pass has no way to know that, so it always
// treats a rebinding as "this argument expression's value may now
// differ."

use crate::ast::*;
use crate::interner::Symbol;
use std::collections::HashMap;

struct AvailEntry {
    f_name: Symbol,
    args: Vec<Expr>,
    var: Symbol,
}

fn expr_eq(a: &Expr, b: &Expr) -> bool {
    match (&a.kind, &b.kind) {
        (ExprKind::Num(x), ExprKind::Num(y)) => x == y,
        (ExprKind::Str(x), ExprKind::Str(y)) => x == y,
        (ExprKind::Bool(x), ExprKind::Bool(y)) => x == y,
        (ExprKind::Ident(x), ExprKind::Ident(y)) => x == y,
        (ExprKind::ArrayLit(x), ExprKind::ArrayLit(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| expr_eq(a, b)),
        (ExprKind::Unary { op: ox, expr: x }, ExprKind::Unary { op: oy, expr: y }) => ox == oy && expr_eq(x, y),
        (ExprKind::Binop { op: ox, left: lx, right: rx }, ExprKind::Binop { op: oy, left: ly, right: ry }) => {
            ox == oy && expr_eq(lx, ly) && expr_eq(rx, ry)
        }
        (ExprKind::Index { target: tx, index: ix }, ExprKind::Index { target: ty, index: iy }) => expr_eq(tx, ty) && expr_eq(ix, iy),
        (ExprKind::Call { name: nx, args: ax }, ExprKind::Call { name: ny, args: ay }) => {
            nx == ny && ax.len() == ay.len() && ax.iter().zip(ay).all(|(a, b)| expr_eq(a, b))
        }
        (ExprKind::StructLit { name: nx, fields: fx }, ExprKind::StructLit { name: ny, fields: fy }) => {
            nx == ny && fx.len() == fy.len() && fx.iter().zip(fy).all(|((kx, vx), (ky, vy))| kx == ky && expr_eq(vx, vy))
        }
        (ExprKind::Field { target: tx, field: fx }, ExprKind::Field { target: ty, field: fy }) => fx == fy && expr_eq(tx, ty),
        _ => false,
    }
}

fn collect_idents(e: &Expr, out: &mut Vec<Symbol>) {
    match &e.kind {
        ExprKind::Ident(n) => out.push(*n),
        ExprKind::ArrayLit(elems) => {
            for el in elems {
                collect_idents(el, out);
            }
        }
        ExprKind::Unary { expr, .. } => collect_idents(expr, out),
        ExprKind::Binop { left, right, .. } => {
            collect_idents(left, out);
            collect_idents(right, out);
        }
        ExprKind::Index { target, index } => {
            collect_idents(target, out);
            collect_idents(index, out);
        }
        ExprKind::Call { args, .. } => {
            for a in args {
                collect_idents(a, out);
            }
        }
        ExprKind::StructLit { fields, .. } => {
            for (_, v) in fields {
                collect_idents(v, out);
            }
        }
        ExprKind::Field { target, .. } => collect_idents(target, out),
        ExprKind::Num(_) | ExprKind::Str(_) | ExprKind::Bool(_) => {}
    }
}

/// Post-order rewrite: transforms children first, then (if the whole
/// expression is itself a call matching an available entry) replaces it
/// with a reference to that entry's variable.
fn rewrite_expr(e: &mut Expr, fns: &HashMap<Symbol, Fn>, available: &[AvailEntry]) {
    match &mut e.kind {
        ExprKind::ArrayLit(elems) => {
            for el in elems {
                rewrite_expr(el, fns, available);
            }
        }
        ExprKind::Unary { expr, .. } => rewrite_expr(expr, fns, available),
        ExprKind::Binop { left, right, .. } => {
            rewrite_expr(left, fns, available);
            rewrite_expr(right, fns, available);
        }
        ExprKind::Index { target, index } => {
            rewrite_expr(target, fns, available);
            rewrite_expr(index, fns, available);
        }
        ExprKind::Call { args, .. } => {
            for a in args {
                rewrite_expr(a, fns, available);
            }
        }
        ExprKind::StructLit { fields, .. } => {
            for (_, v) in fields {
                rewrite_expr(v, fns, available);
            }
        }
        ExprKind::Field { target, .. } => rewrite_expr(target, fns, available),
        ExprKind::Num(_) | ExprKind::Str(_) | ExprKind::Bool(_) | ExprKind::Ident(_) => {}
    }

    if let ExprKind::Call { name, args } = &e.kind {
        if fns.get(name).is_some_and(|f| f.pure) {
            if let Some(entry) = available.iter().find(|c| c.f_name == *name && c.args.len() == args.len() && c.args.iter().zip(args).all(|(a, b)| expr_eq(a, b))) {
                e.kind = ExprKind::Ident(entry.var);
            }
        }
    }
}

fn cse_block(body: &mut [Stmt], fns: &HashMap<Symbol, Fn>) {
    let mut available: Vec<AvailEntry> = Vec::new();

    for s in body.iter_mut() {
        match s {
            Stmt::Let { name, value, .. } => {
                rewrite_expr(value, fns, &available);
                available.retain(|c| !c.args.iter().any(|a| {
                    let mut ids = Vec::new();
                    collect_idents(a, &mut ids);
                    ids.contains(name)
                }));
                if let ExprKind::Call { name: f_name, args } = &value.kind {
                    if fns.get(f_name).is_some_and(|f| f.pure) {
                        available.push(AvailEntry { f_name: *f_name, args: args.clone(), var: *name });
                    }
                }
            }
            Stmt::Assign { name, value, .. } => {
                rewrite_expr(value, fns, &available);
                available.retain(|c| !c.args.iter().any(|a| {
                    let mut ids = Vec::new();
                    collect_idents(a, &mut ids);
                    ids.contains(name)
                }));
            }
            Stmt::If { cond, then_block, else_block, .. } => {
                rewrite_expr(cond, fns, &available);
                cse_block(then_block, fns);
                if let Some(eb) = else_block {
                    cse_block(eb, fns);
                }
            }
            Stmt::While { cond, body: wbody, .. } => {
                rewrite_expr(cond, fns, &available);
                cse_block(wbody, fns);
            }
            Stmt::Print { args, .. } => {
                for a in args {
                    rewrite_expr(a, fns, &available);
                }
            }
            Stmt::Return { value, .. } => {
                if let Some(v) = value {
                    rewrite_expr(v, fns, &available);
                }
            }
            Stmt::ExprStmt { expr, .. } => rewrite_expr(expr, fns, &available),
        }
    }
}

pub fn eliminate_common_calls(program: &Program) -> Program {
    let fns: HashMap<Symbol, Fn> = program.fns.iter().map(|f| (f.name, f.clone())).collect();
    let mut new_program = program.clone();
    for f in new_program.fns.iter_mut() {
        cse_block(&mut f.body, &fns);
    }
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
    fn reuses_a_let_bound_call_result_for_a_later_identical_call() {
        let program = parse_src(
            "
            pure fn square(x: i32) -> i32 { return x * x; }
            fn main() {
                let a = square(5);
                let total = 0;
                total = total + square(5);
                print(total);
            }
            ",
        );
        let out = eliminate_common_calls(&program);
        let main_fn = out.fns.iter().find(|f| f.name.resolve().as_ref() == "main").unwrap();
        let Stmt::Assign { value, .. } = &main_fn.body[2] else { panic!("expected assign") };
        let ExprKind::Binop { right, .. } = &value.kind else { panic!("expected binop") };
        assert!(matches!(&right.kind, ExprKind::Ident(n) if n.resolve().as_ref() == "a"), "second call should be rewritten to reuse `a`, got: {:?}", right.kind);
    }

    #[test]
    fn does_not_reuse_across_a_reassignment_of_the_argument() {
        let program = parse_src(
            "
            pure fn square(x: i32) -> i32 { return x * x; }
            fn main() {
                let x = 5;
                let a = square(x);
                x = 6;
                let b = square(x);
                print(a, b);
            }
            ",
        );
        let out = eliminate_common_calls(&program);
        let main_fn = out.fns.iter().find(|f| f.name.resolve().as_ref() == "main").unwrap();
        let Stmt::Let { value, .. } = &main_fn.body[3] else { panic!("expected let") };
        assert!(matches!(&value.kind, ExprKind::Call { .. }), "second call must not be reused after x was reassigned, got: {:?}", value.kind);
    }

    #[test]
    fn does_not_reuse_an_impure_functions_repeated_call() {
        let program = parse_src(
            "
            fn noisy(x: i32) -> i32 { print(x); return x; }
            fn main() {
                let a = noisy(5);
                let b = noisy(5);
                print(a, b);
            }
            ",
        );
        let out = eliminate_common_calls(&program);
        let main_fn = out.fns.iter().find(|f| f.name.resolve().as_ref() == "main").unwrap();
        let Stmt::Let { value, .. } = &main_fn.body[1] else { panic!("expected let") };
        assert!(matches!(&value.kind, ExprKind::Call { .. }), "impure calls must never be deduplicated, got: {:?}", value.kind);
    }

    #[test]
    fn does_not_carry_availability_into_a_while_loop_body() {
        let program = parse_src(
            "
            pure fn square(x: i32) -> i32 { return x * x; }
            fn main() {
                let a = square(5);
                let i = 0;
                while (i < 3) {
                    let b = square(5);
                    i = i + 1;
                }
                print(a);
            }
            ",
        );
        let out = eliminate_common_calls(&program);
        let main_fn = out.fns.iter().find(|f| f.name.resolve().as_ref() == "main").unwrap();
        let Stmt::While { body, .. } = &main_fn.body[2] else { panic!("expected while") };
        let Stmt::Let { value, .. } = &body[0] else { panic!("expected let") };
        assert!(matches!(&value.kind, ExprKind::Call { .. }), "a call inside a loop body must not reuse an entry from outside it, got: {:?}", value.kind);
    }
}
