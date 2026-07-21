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

/// Sibling to `build_fn_table` — same "build once, share across every
/// stage" idea, for struct declarations instead of function
/// declarations.
pub fn build_struct_table(program: &Program) -> HashMap<Symbol, &StructDecl> {
    program.structs.iter().map(|s| (s.name, s)).collect()
}

/// Resolves every name in `program` against `fns` (see `build_fn_table`)
/// and each function's own locals, returning every problem found rather
/// than stopping at the first — same "report everything, not just the
/// first mistake" contract as `check_purity`/`check_types`.
pub fn resolve(
    program: &Program,
    fns: &HashMap<Symbol, &Fn>,
    structs: &HashMap<Symbol, &StructDecl>,
) -> Vec<KestrelcError> {
    let mut errors = Vec::new();
    check_duplicate_fns(program, &mut errors);
    check_struct_decls(program, structs, &mut errors);
    check_no_struct_returning_fns(program, structs, &mut errors);
    for fn_ in &program.fns {
        resolve_fn(fn_, fns, structs, &mut errors);
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

/// Enforces the v1 scope limit that every struct field must be scalar
/// — no array-typed fields, no nested structs. Without this, an
/// out-of-scope field type wouldn't be caught until codegen.rs
/// silently miscompiled it (Slot::Struct assumes exactly one Cranelift
/// Variable per field, which is wrong for an array field's real
/// (pointer, length) ABI shape, and plain wrong for a nested struct's
/// own multi-field shape). Called once per struct declaration, not per
/// use site — a bad field type is wrong regardless of whether the
/// struct is ever instantiated.
fn check_struct_decls(program: &Program, structs: &HashMap<Symbol, &StructDecl>, errors: &mut Vec<KestrelcError>) {
    for decl in &program.structs {
        for field in &decl.fields {
            let is_scalar = match &field.ty {
                Type::Array { .. } => false,
                Type::Named(ty_name) => !structs.contains_key(ty_name),
            };
            if !is_scalar {
                errors.push(KestrelcError::new(
                    ErrorKind::Resolve,
                    format!("'{}.{}' must be a scalar field — array fields and nested structs aren't supported yet", decl.name, field.name),
                    decl.span,
                ));
            }
        }
    }
}

/// Enforces the v1 scope limit that no function may return a struct
/// value (design doc). Without this, a `fn make() -> Point { ... }`
/// isn't caught here at all -- it fails later, but only because a
/// struct value happens to be unusable in value position everywhere
/// else in the compiler, which is a confusing error far from the
/// actual cause. Checking the declared return type directly gives a
/// clear message right at the function that violates the limit.
fn check_no_struct_returning_fns(program: &Program, structs: &HashMap<Symbol, &StructDecl>, errors: &mut Vec<KestrelcError>) {
    for f in &program.fns {
        if let Some(Type::Named(ty_name)) = &f.return_type {
            if structs.contains_key(ty_name) {
                errors.push(KestrelcError::new(
                    ErrorKind::Resolve,
                    format!("'{}' can't return a struct — struct return values aren't supported yet", f.name),
                    f.span,
                ));
            }
        }
    }
}

fn resolve_fn(
    fn_: &Fn,
    fns: &HashMap<Symbol, &Fn>,
    structs: &HashMap<Symbol, &StructDecl>,
    errors: &mut Vec<KestrelcError>,
) {
    // Flat, non-block-scoped locals — a `let` inside an `if`/`while` is
    // visible for the rest of the function, matching every other pass's
    // (and every backend's runtime) existing scoping rule. `struct_locals`
    // tracks, for each local that's a struct value, which struct type it
    // is -- needed to validate `.field` access. A local not in this map
    // either isn't in scope at all (caught by the plain `locals` check)
    // or is a non-struct value (scalar/array).
    let mut locals: HashSet<Symbol> = fn_.params.iter().map(|p| p.name).collect();
    let mut struct_locals: HashMap<Symbol, Symbol> = HashMap::new();
    for p in &fn_.params {
        if let Type::Named(ty_name) = &p.ty {
            if structs.contains_key(ty_name) {
                struct_locals.insert(p.name, *ty_name);
            }
        }
    }
    for s in &fn_.body {
        resolve_stmt(s, &mut locals, &mut struct_locals, fns, structs, errors);
    }
}

// `span` is the enclosing statement's span, same statement-granularity
// tradeoff as purity.rs/typecheck.rs (see error.rs's doc comment).
fn resolve_expr(
    e: &Expr,
    locals: &HashSet<Symbol>,
    struct_locals: &HashMap<Symbol, Symbol>,
    fns: &HashMap<Symbol, &Fn>,
    structs: &HashMap<Symbol, &StructDecl>,
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
            // 100MB / 8 bytes per i64 element. A safety net against a
            // literal so large it would itself cause compile-time or
            // runtime memory problems regardless of allocation
            // strategy -- not a meaningful limit for any real program
            // (see codegen.rs's heap-allocation threshold at 4KB for
            // where the *normal* large-array path kicks in well below
            // this cap).
            const MAX_ARRAY_LITERAL_ELEMENTS: usize = 12_500_000;
            if elems.len() > MAX_ARRAY_LITERAL_ELEMENTS {
                errors.push(KestrelcError::new(
                    ErrorKind::Resolve,
                    format!(
                        "array literal with {} elements is too large to compile (over 100MB) — this is almost certainly a mistake",
                        elems.len()
                    ),
                    e.span,
                ));
            }
            for el in elems {
                resolve_expr(el, locals, struct_locals, fns, structs, span, errors);
            }
        }
        ExprKind::Unary { expr, .. } => resolve_expr(expr, locals, struct_locals, fns, structs, span, errors),
        ExprKind::Binop { left, right, .. } => {
            resolve_expr(left, locals, struct_locals, fns, structs, span, errors);
            resolve_expr(right, locals, struct_locals, fns, structs, span, errors);
        }
        ExprKind::Index { target, index } => {
            resolve_expr(target, locals, struct_locals, fns, structs, span, errors);
            resolve_expr(index, locals, struct_locals, fns, structs, span, errors);
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
                    resolve_expr(&args[1], locals, struct_locals, fns, structs, span, errors);
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
                resolve_expr(a, locals, struct_locals, fns, structs, span, errors);
            }
        }
        ExprKind::StructLit { name, fields } => {
            match structs.get(name) {
                None => {
                    errors.push(KestrelcError::new(
                        ErrorKind::Resolve,
                        format!("Unknown struct '{name}'"),
                        e.span,
                    ));
                }
                Some(decl) => {
                    let declared: HashSet<Symbol> = decl.fields.iter().map(|f| f.name).collect();
                    let written: HashSet<Symbol> = fields.iter().map(|(n, _)| *n).collect();
                    for missing in declared.difference(&written) {
                        errors.push(KestrelcError::new(
                            ErrorKind::Resolve,
                            format!("'{name}' literal is missing field '{missing}'"),
                            e.span,
                        ));
                    }
                    for (field_name, _) in fields {
                        if !declared.contains(field_name) {
                            errors.push(KestrelcError::new(
                                ErrorKind::Resolve,
                                format!("'{name}' has no field '{field_name}'"),
                                e.span,
                            ));
                        }
                    }
                    // A field written more than once (`Point { x: 1, x: 2,
                    // y: 3 }`) passes both checks above silently -- `written`
                    // is a set, so the duplicate doesn't affect missing/
                    // unknown-field detection at all. Without this,
                    // gen_binding's `.find()` would quietly take the first
                    // occurrence and drop the rest with no error anywhere.
                    let mut seen: HashSet<Symbol> = HashSet::new();
                    for (field_name, _) in fields {
                        if !seen.insert(*field_name) {
                            errors.push(KestrelcError::new(
                                ErrorKind::Resolve,
                                format!("'{name}' sets field '{field_name}' more than once"),
                                e.span,
                            ));
                        }
                    }
                }
            }
            for (_, value) in fields {
                resolve_expr(value, locals, struct_locals, fns, structs, span, errors);
            }
        }
        ExprKind::Field { target, field } => {
            resolve_expr(target, locals, struct_locals, fns, structs, span, errors);
            if let ExprKind::Ident(target_name) = &target.kind {
                match struct_locals.get(target_name) {
                    None => {
                        // Only report "not a struct" if `target_name` is
                        // actually a known local at all -- an unknown
                        // identifier there is already reported by the
                        // recursive resolve_expr call just above, and
                        // reporting both would be a confusing double
                        // error about the exact same typo.
                        if locals.contains(target_name) {
                            errors.push(KestrelcError::new(
                                ErrorKind::Resolve,
                                format!("'{target_name}' is not a struct"),
                                e.span,
                            ));
                        }
                    }
                    Some(struct_name) => {
                        if let Some(decl) = structs.get(struct_name) {
                            if !decl.fields.iter().any(|f| f.name == *field) {
                                errors.push(KestrelcError::new(
                                    ErrorKind::Resolve,
                                    format!("'{struct_name}' has no field '{field}'"),
                                    e.span,
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
}

fn resolve_stmt(
    s: &Stmt,
    locals: &mut HashSet<Symbol>,
    struct_locals: &mut HashMap<Symbol, Symbol>,
    fns: &HashMap<Symbol, &Fn>,
    structs: &HashMap<Symbol, &StructDecl>,
    errors: &mut Vec<KestrelcError>,
) {
    match s {
        Stmt::Let { name, value, span } => {
            resolve_expr(value, locals, struct_locals, fns, structs, *span, errors);
            // Only a direct struct literal is tracked -- `let p2 = p1;`
            // aliasing an existing struct-typed local isn't detected
            // here (matches codegen.rs's slot_kind_for_let, which has
            // the exact same documented limitation for arrays: only a
            // literal expression is recognized, not an alias).
            if let ExprKind::StructLit { name: struct_name, .. } = &value.kind {
                struct_locals.insert(*name, *struct_name);
            }
            locals.insert(*name);
        }
        Stmt::Assign { name, value, span } => {
            resolve_expr(value, locals, struct_locals, fns, structs, *span, errors);
            if !locals.contains(name) {
                errors.push(KestrelcError::new(
                    ErrorKind::Resolve,
                    format!("Assignment to unknown variable '{name}'"),
                    *span,
                ));
            }
        }
        Stmt::If { cond, then_block, else_block, span } => {
            resolve_expr(cond, locals, struct_locals, fns, structs, *span, errors);
            for st in then_block {
                resolve_stmt(st, locals, struct_locals, fns, structs, errors);
            }
            if let Some(eb) = else_block {
                for st in eb {
                    resolve_stmt(st, locals, struct_locals, fns, structs, errors);
                }
            }
        }
        Stmt::While { cond, body, span } => {
            resolve_expr(cond, locals, struct_locals, fns, structs, *span, errors);
            for st in body {
                resolve_stmt(st, locals, struct_locals, fns, structs, errors);
            }
        }
        Stmt::Print { args, span } => {
            for a in args {
                resolve_expr(a, locals, struct_locals, fns, structs, *span, errors);
            }
        }
        Stmt::Return { value, span } => {
            if let Some(v) = value {
                resolve_expr(v, locals, struct_locals, fns, structs, *span, errors);
            }
        }
        Stmt::ExprStmt { expr, span } => resolve_expr(expr, locals, struct_locals, fns, structs, *span, errors),
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
        let structs = build_struct_table(&program);
        resolve(&program, &fns, &structs)
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

    #[test]
    fn accepts_a_well_formed_struct_literal_and_field_access() {
        let errors = resolve_src(
            "struct Point { x: i64, y: i64 }\nfn main() { let p = Point { x: 1, y: 2 }; print(p.x, p.y); }",
        );
        assert!(errors.is_empty());
    }

    #[test]
    fn catches_a_struct_literal_naming_an_undeclared_struct() {
        let errors = resolve_src("fn main() { let p = Bogus { x: 1 }; print(p); }");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("Unknown struct 'Bogus'"));
    }

    #[test]
    fn catches_a_struct_literal_missing_a_declared_field() {
        let errors = resolve_src(
            "struct Point { x: i64, y: i64 }\nfn main() { let p = Point { x: 1 }; print(p.x); }",
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("missing field 'y'"));
    }

    #[test]
    fn catches_a_struct_literal_with_an_unknown_field() {
        let errors = resolve_src(
            "struct Point { x: i64, y: i64 }\nfn main() { let p = Point { x: 1, y: 2, z: 3 }; print(p.x); }",
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("'Point' has no field 'z'"));
    }

    #[test]
    fn catches_field_access_on_a_plain_scalar_local() {
        let errors = resolve_src("fn main() { let x = 5; print(x.y); }");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("'x' is not a struct"));
    }

    #[test]
    fn catches_field_access_naming_a_field_the_struct_does_not_have() {
        let errors = resolve_src(
            "struct Point { x: i64, y: i64 }\nfn main() { let p = Point { x: 1, y: 2 }; print(p.z); }",
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("'Point' has no field 'z'"));
    }

    #[test]
    fn catches_a_struct_declaration_with_an_array_typed_field() {
        // v1 scope limit (design doc): scalar fields only. Without this
        // check, an array-typed field would silently miscompile later
        // (codegen.rs's Slot::Struct assumes exactly one Variable per
        // field) instead of being rejected with a clear message here.
        let errors = resolve_src("struct Bad { xs: [i64; N] }\nfn main() {}");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("'Bad.xs' must be a scalar field"));
    }

    #[test]
    fn catches_a_struct_declaration_with_a_nested_struct_field() {
        // v1 scope limit: no nested structs either -- same reasoning
        // and same error shape as the array case above.
        let errors = resolve_src(
            "struct Inner { x: i64 }\nstruct Outer { inner: Inner }\nfn main() {}",
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("'Outer.inner' must be a scalar field"));
    }

    #[test]
    fn catches_a_fn_that_returns_a_struct() {
        // v1 scope limit (design doc): no struct-returning functions.
        // Without this check, struct return values fail later with a
        // confusing error far from the actual cause (struct values are
        // unusable in value position everywhere else) instead of being
        // rejected clearly here.
        let errors = resolve_src(
            "struct Point { x: i64, y: i64 }\nfn make() -> Point { let p = Point { x: 1, y: 2 }; return p; }\nfn main() { }",
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0]
            .message
            .contains("'make' can't return a struct — struct return values aren't supported yet"));
    }

    #[test]
    fn a_normal_non_struct_return_type_is_unaffected() {
        let errors = resolve_src(
            "pure fn square(x: i64) -> i64 { return x * x; }\nfn main() { print(square(3)); }",
        );
        assert!(errors.is_empty());
    }

    #[test]
    fn catches_a_struct_literal_setting_the_same_field_twice() {
        let errors = resolve_src(
            "struct Point { x: i64, y: i64 }\nfn main() { let p = Point { x: 1, x: 2, y: 3 }; print(p.x); }",
        );
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("'Point' sets field 'x' more than once"));
    }
}
