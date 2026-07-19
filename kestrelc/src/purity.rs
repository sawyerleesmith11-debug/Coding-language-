// Direct port of kestrel.js's checkPurity() — same rules, same
// recursion-safe traversal, same error wording.

use crate::ast::*;
use std::collections::{HashMap, HashSet};

pub fn check_purity(program: &Program) -> Vec<String> {
    let fns: HashMap<&str, &Fn> = program.iter().map(|f| (f.name.as_str(), f)).collect();
    let mut impure_cache: HashMap<String, bool> = HashMap::new();

    fn is_impure<'a>(
        fn_: &'a Fn,
        fns: &HashMap<&'a str, &'a Fn>,
        cache: &mut HashMap<String, bool>,
        stack: &mut HashSet<String>,
    ) -> bool {
        if let Some(v) = cache.get(&fn_.name) {
            return *v;
        }
        if stack.contains(&fn_.name) {
            return false; // recursion: assume ok, don't loop forever
        }
        stack.insert(fn_.name.clone());

        let mut impure = false;
        let mut locals: HashSet<String> = fn_.params.iter().map(|p| p.name.clone()).collect();

        fn visit_expr<'a>(
            e: &Expr,
            fns: &HashMap<&'a str, &'a Fn>,
            cache: &mut HashMap<String, bool>,
            stack: &mut HashSet<String>,
            impure: &mut bool,
        ) {
            if *impure {
                return;
            }
            match e {
                Expr::Call { name, args } => {
                    if let Some(callee) = fns.get(name.as_str()) {
                        if !callee.pure {
                            *impure = true;
                            return;
                        }
                        if is_impure(callee, fns, cache, stack) {
                            *impure = true;
                            return;
                        }
                    }
                    for a in args {
                        visit_expr(a, fns, cache, stack, impure);
                    }
                }
                Expr::Binop { left, right, .. } => {
                    visit_expr(left, fns, cache, stack, impure);
                    visit_expr(right, fns, cache, stack, impure);
                }
                Expr::Unary { expr, .. } => visit_expr(expr, fns, cache, stack, impure),
                Expr::Index { target, index } => {
                    visit_expr(target, fns, cache, stack, impure);
                    visit_expr(index, fns, cache, stack, impure);
                }
                Expr::ArrayLit(elems) => {
                    for el in elems {
                        visit_expr(el, fns, cache, stack, impure);
                    }
                }
                _ => {}
            }
        }

        fn visit_stmt<'a>(
            s: &Stmt,
            fns: &HashMap<&'a str, &'a Fn>,
            cache: &mut HashMap<String, bool>,
            stack: &mut HashSet<String>,
            locals: &mut HashSet<String>,
            impure: &mut bool,
        ) {
            if *impure {
                return;
            }
            match s {
                Stmt::Let { name, value } => {
                    locals.insert(name.clone());
                    visit_expr(value, fns, cache, stack, impure);
                }
                Stmt::Assign { name, value } => {
                    if !locals.contains(name) {
                        *impure = true; // mutating something outside itself
                        return;
                    }
                    visit_expr(value, fns, cache, stack, impure);
                }
                Stmt::If { cond, then_block, else_block } => {
                    visit_expr(cond, fns, cache, stack, impure);
                    for st in then_block {
                        visit_stmt(st, fns, cache, stack, locals, impure);
                    }
                    if let Some(eb) = else_block {
                        for st in eb {
                            visit_stmt(st, fns, cache, stack, locals, impure);
                        }
                    }
                }
                Stmt::While { cond, body } => {
                    visit_expr(cond, fns, cache, stack, impure);
                    for st in body {
                        visit_stmt(st, fns, cache, stack, locals, impure);
                    }
                }
                Stmt::Print { .. } => {
                    *impure = true; // I/O
                }
                Stmt::Return { value } => {
                    if let Some(v) = value {
                        visit_expr(v, fns, cache, stack, impure);
                    }
                }
                Stmt::ExprStmt { expr } => visit_expr(expr, fns, cache, stack, impure),
            }
        }

        for s in &fn_.body {
            visit_stmt(s, fns, cache, stack, &mut locals, &mut impure);
        }

        cache.insert(fn_.name.clone(), impure);
        stack.remove(&fn_.name);
        impure
    }

    let mut errors = Vec::new();
    for fn_ in program {
        if fn_.pure {
            let mut stack = HashSet::new();
            if is_impure(fn_, &fns, &mut impure_cache, &mut stack) {
                errors.push(format!(
                    "'{}' is marked pure but calls print or an impure function",
                    fn_.name
                ));
            }
        }
    }
    errors
}
