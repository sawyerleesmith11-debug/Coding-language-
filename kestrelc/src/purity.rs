// Direct port of kestrel.js's checkPurity() — same rules, same
// recursion-safe traversal, same error wording. Returns KestrelcError
// (message + source position) rather than a bare String — see
// error.rs's doc comment for the statement-granularity scope.

use crate::ast::*;
use crate::error::{ErrorKind, KestrelcError};
use crate::interner::Symbol;
use crate::span::Span;
use std::collections::{HashMap, HashSet};

pub fn check_purity(program: &Program) -> Vec<KestrelcError> {
    let fns: HashMap<Symbol, &Fn> = program.iter().map(|f| (f.name, f)).collect();
    let mut impure_cache: HashMap<Symbol, bool> = HashMap::new();

    fn is_impure<'a>(
        fn_: &'a Fn,
        fns: &HashMap<Symbol, &'a Fn>,
        cache: &mut HashMap<Symbol, bool>,
        stack: &mut HashSet<Symbol>,
    ) -> bool {
        if let Some(v) = cache.get(&fn_.name) {
            return *v;
        }
        if stack.contains(&fn_.name) {
            return false; // recursion: assume ok, don't loop forever
        }
        stack.insert(fn_.name);

        let mut impure = false;
        let mut locals: HashSet<Symbol> = fn_.params.iter().map(|p| p.name).collect();

        fn visit_expr<'a>(
            e: &Expr,
            fns: &HashMap<Symbol, &'a Fn>,
            cache: &mut HashMap<Symbol, bool>,
            stack: &mut HashSet<Symbol>,
            impure: &mut bool,
        ) {
            if *impure {
                return;
            }
            match e {
                Expr::Call { name, args } => {
                    if let Some(callee) = fns.get(name) {
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
            fns: &HashMap<Symbol, &'a Fn>,
            cache: &mut HashMap<Symbol, bool>,
            stack: &mut HashSet<Symbol>,
            locals: &mut HashSet<Symbol>,
            impure: &mut bool,
        ) {
            if *impure {
                return;
            }
            match s {
                Stmt::Let { name, value, .. } => {
                    locals.insert(*name);
                    visit_expr(value, fns, cache, stack, impure);
                }
                Stmt::Assign { name, value, .. } => {
                    if !locals.contains(name) {
                        *impure = true; // mutating something outside itself
                        return;
                    }
                    visit_expr(value, fns, cache, stack, impure);
                }
                Stmt::If { cond, then_block, else_block, .. } => {
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
                Stmt::While { cond, body, .. } => {
                    visit_expr(cond, fns, cache, stack, impure);
                    for st in body {
                        visit_stmt(st, fns, cache, stack, locals, impure);
                    }
                }
                Stmt::Print { .. } => {
                    *impure = true; // I/O
                }
                Stmt::Return { value, .. } => {
                    if let Some(v) = value {
                        visit_expr(v, fns, cache, stack, impure);
                    }
                }
                Stmt::ExprStmt { expr, .. } => visit_expr(expr, fns, cache, stack, impure),
            }
        }

        for s in &fn_.body {
            visit_stmt(s, fns, cache, stack, &mut locals, &mut impure);
        }

        cache.insert(fn_.name, impure);
        stack.remove(&fn_.name);
        impure
    }

    let mut errors = Vec::new();
    for fn_ in program {
        if fn_.pure {
            let mut stack = HashSet::new();
            if is_impure(fn_, &fns, &mut impure_cache, &mut stack) {
                errors.push(KestrelcError::new(
                    ErrorKind::Purity,
                    format!(
                        "'{}' is marked pure but calls print or an impure function",
                        fn_.name
                    ),
                    fn_.span,
                ));
            }
        }
    }
    errors
}

/// `parallel_map(f, arr)` is a reserved builtin call name (like `print`),
/// not a keyword — see kestrel-DESIGN.md idea #5. Purity is what makes
/// this safe: a `pure fn` can't observe or be affected by any other call
/// to itself, so applying it once per array element has nothing to race
/// over no matter what order (or how much overlap) those calls happen
/// in. This check runs unconditionally (not just inside `pure fn`
/// bodies, unlike `check_purity`) since misusing it is a bug regardless
/// of the caller's own purity. Direct port of kestrel.js's
/// `checkParallelMap` — same rules, same wording.
pub fn check_parallel_map(program: &Program) -> Vec<KestrelcError> {
    let fns: HashMap<Symbol, &Fn> = program.iter().map(|f| (f.name, f)).collect();
    let mut errors = Vec::new();

    // `span` is the *enclosing statement's* span, not the exact
    // parallel_map(...) call's own position — see error.rs's doc
    // comment on the statement-granularity scope this is limited to.
    fn visit_expr(e: &Expr, fns: &HashMap<Symbol, &Fn>, span: Span, errors: &mut Vec<KestrelcError>) {
        let push = |errors: &mut Vec<KestrelcError>, message: String| {
            errors.push(KestrelcError::new(ErrorKind::ParallelMap, message, span));
        };
        match e {
            Expr::Call { name, args } if &*name.resolve() == "parallel_map" => {
                if args.len() != 2 {
                    push(errors, format!(
                        "parallel_map() takes exactly 2 arguments (a pure function and an array), got {}",
                        args.len()
                    ));
                    return;
                }
                match &args[0] {
                    Expr::Ident(func_name) => match fns.get(func_name) {
                        None => push(errors, format!("parallel_map(): unknown function '{func_name}'")),
                        Some(callee) if !callee.pure => push(errors, format!(
                            "parallel_map(): '{func_name}' must be a 'pure fn' — parallel safety comes entirely from the purity proof"
                        )),
                        Some(callee) if callee.params.len() != 1 => push(errors, format!(
                            "parallel_map(): '{func_name}' must take exactly one parameter (one array element in, one result out), has {}",
                            callee.params.len()
                        )),
                        Some(callee) if !matches!(callee.params[0].ty, Type::Named(_)) => push(errors, format!(
                            "parallel_map(): '{func_name}'s parameter must be a scalar (one array element), not an array"
                        )),
                        Some(_) => {}
                    },
                    _ => push(errors,
                        "parallel_map()'s first argument must be a bare function name, not an expression".to_string(),
                    ),
                }
                visit_expr(&args[1], fns, span, errors);
            }
            Expr::Call { args, .. } => {
                for a in args {
                    visit_expr(a, fns, span, errors);
                }
            }
            Expr::Binop { left, right, .. } => {
                visit_expr(left, fns, span, errors);
                visit_expr(right, fns, span, errors);
            }
            Expr::Unary { expr, .. } => visit_expr(expr, fns, span, errors),
            Expr::Index { target, index } => {
                visit_expr(target, fns, span, errors);
                visit_expr(index, fns, span, errors);
            }
            Expr::ArrayLit(elems) => {
                for el in elems {
                    visit_expr(el, fns, span, errors);
                }
            }
            _ => {}
        }
    }

    fn visit_stmt(s: &Stmt, fns: &HashMap<Symbol, &Fn>, errors: &mut Vec<KestrelcError>) {
        match s {
            Stmt::Let { value, span, .. } | Stmt::Assign { value, span, .. } => {
                visit_expr(value, fns, *span, errors)
            }
            Stmt::If { cond, then_block, else_block, span } => {
                visit_expr(cond, fns, *span, errors);
                for st in then_block {
                    visit_stmt(st, fns, errors);
                }
                if let Some(eb) = else_block {
                    for st in eb {
                        visit_stmt(st, fns, errors);
                    }
                }
            }
            Stmt::While { cond, body, span } => {
                visit_expr(cond, fns, *span, errors);
                for st in body {
                    visit_stmt(st, fns, errors);
                }
            }
            Stmt::Print { args, span } => {
                for a in args {
                    visit_expr(a, fns, *span, errors);
                }
            }
            Stmt::Return { value, span } => {
                if let Some(v) = value {
                    visit_expr(v, fns, *span, errors);
                }
            }
            Stmt::ExprStmt { expr, span } => visit_expr(expr, fns, *span, errors),
        }
    }

    for fn_ in program {
        for s in &fn_.body {
            visit_stmt(s, &fns, &mut errors);
        }
    }
    errors
}
