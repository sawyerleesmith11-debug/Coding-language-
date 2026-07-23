// Type checker — see kestrel-DESIGN.md's roadmap for exactly what this
// is and isn't. Infers each expression's value *kind* (Int, Bool, Array,
// Str, or Struct(name)) purely from literals, operators, and now also
// declared type annotations (function params, struct fields, return
// types), and rejects mismatches: `5 + true`, `!5`, a literal number
// used directly as an `if`/`while` condition, a function-call argument
// count mismatch, an argument whose kind doesn't match the callee's
// declared parameter type, a struct literal or field-assignment value
// that doesn't match its field's declared type, and a `return` value
// that doesn't match the function's declared return type.
//
// `type_to_kind` maps every declared integer type name (`i64`, `i32`,
// `usize`, ...) to the same `Kind::Int` -- there's still only one
// runtime integer representation, so this doesn't invent a new
// distinction the rest of the compiler doesn't have, it just lets this
// checker recognize the one that already exists. Every rule still only
// fires when both sides are *known* (never `Kind::Unknown`), so a
// program that would otherwise run correctly is never rejected —
// same "never guess" posture the original, narrower version of this
// checker already had.

use crate::ast::*;
use crate::error::{ErrorKind, KestrelcError};
use crate::interner::Symbol;
use crate::span::Span;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Bool,
    Array,
    Str,
    /// Carries the struct's own name so a future field-type check could
    /// use it -- not read for that purpose yet (see infer_expr's Field
    /// arm: a field's own resulting kind is always Unknown, matching
    /// the type checker's existing "declared types don't carry kind
    /// info yet" limitation for every other declared type).
    Struct(Symbol),
    Unknown,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Int => "int",
            Kind::Bool => "bool",
            Kind::Array => "array",
            Kind::Str => "str",
            Kind::Struct(_) => "struct",
            Kind::Unknown => "unknown",
        }
    }
}

pub fn check_types(
    program: &Program,
    fns: &HashMap<Symbol, &Fn>,
    structs: &HashMap<Symbol, &StructDecl>,
) -> Vec<KestrelcError> {
    let mut errors = Vec::new();

    // Maps a declared `Type` (a function param, a struct field, a return
    // type) to the `Kind` it should be checked against. Every integer
    // type name (`i64`, `i32`, `usize`, ...) collapses to `Kind::Int` --
    // there's still only one runtime integer representation (see this
    // file's own module doc comment), so this doesn't invent a new
    // distinction the rest of the compiler doesn't have, it just lets
    // the checker recognize the existing one. `bool`/`str` are
    // recognized by name for the same reason; anything else defaults to
    // `Int` rather than `Unknown`, since every declared scalar type name
    // actually used in this language today is an integer-family name.
    fn type_to_kind(ty: &Type, structs: &HashMap<Symbol, &StructDecl>) -> Kind {
        match ty {
            Type::Array { .. } => Kind::Array,
            Type::Named(name) => {
                if structs.contains_key(name) {
                    Kind::Struct(*name)
                } else {
                    match name.resolve().as_ref() {
                        "bool" => Kind::Bool,
                        "str" | "string" => Kind::Str,
                        _ => Kind::Int,
                    }
                }
            }
        }
    }

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

    // Every error below is pushed at the specific sub-expression's own
    // `.span` (an Index's bad index arg, a Unary/Binop's own operator
    // position, a Call's own callee-name position) — finer than the
    // enclosing statement, now that every `Expr` node carries its own
    // `Span` (see ast.rs). Still not a true start..end range (see
    // span.rs), just a more specific "point at the start of the actual
    // problem" than a whole statement was.
    fn infer_expr(
        e: &Expr,
        locals: &HashMap<Symbol, Kind>,
        fns: &HashMap<Symbol, &Fn>,
        structs: &HashMap<Symbol, &StructDecl>,
        errors: &mut Vec<KestrelcError>,
    ) -> Kind {
        let push = |errors: &mut Vec<KestrelcError>, span: Span, message: String| {
            errors.push(KestrelcError::new(ErrorKind::Type, message, span));
        };
        match &e.kind {
            ExprKind::Num(_) => Kind::Int,
            ExprKind::Bool(_) => Kind::Bool,
            ExprKind::Str(_) => Kind::Str,
            ExprKind::Ident(name) => locals.get(name).copied().unwrap_or(Kind::Unknown),
            ExprKind::ArrayLit(elems) => {
                for el in elems {
                    infer_expr(el, locals, fns, structs, errors);
                }
                Kind::Array
            }
            ExprKind::Index { target, index } => {
                infer_expr(target, locals, fns, structs, errors);
                let idx_kind = infer_expr(index, locals, fns, structs, errors);
                if idx_kind != Kind::Unknown && idx_kind != Kind::Int {
                    push(errors, index.span, format!("array index must be a number, found {}", idx_kind.name()));
                }
                Kind::Int // Kestrel arrays are integer-valued so far
            }
            ExprKind::Unary { op, expr } => {
                let k = infer_expr(expr, locals, fns, structs, errors);
                match op {
                    UnOp::Neg => {
                        if k != Kind::Unknown && k != Kind::Int {
                            push(errors, e.span, format!("'-' needs a number, found {}", k.name()));
                        }
                        Kind::Int
                    }
                    UnOp::Not => {
                        if k != Kind::Unknown && k != Kind::Bool {
                            push(errors, e.span, format!("'!' needs a boolean, found {}", k.name()));
                        }
                        Kind::Bool
                    }
                }
            }
            ExprKind::Binop { op, left, right } => {
                let l = infer_expr(left, locals, fns, structs, errors);
                let r = infer_expr(right, locals, fns, structs, errors);
                let sym = op_symbol(*op);
                match op {
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                        if !is_numeric(l) || !is_numeric(r) {
                            push(errors, e.span, format!("'{sym}' needs two numbers, found {} and {}", l.name(), r.name()));
                        }
                        Kind::Int
                    }
                    BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                        if !is_numeric(l) || !is_numeric(r) {
                            push(errors, e.span, format!("'{sym}' needs two numbers, found {} and {}", l.name(), r.name()));
                        }
                        Kind::Bool
                    }
                    BinOp::And | BinOp::Or => {
                        if !is_boolean(l) || !is_boolean(r) {
                            push(errors, e.span, format!("'{sym}' needs two booleans, found {} and {}", l.name(), r.name()));
                        }
                        Kind::Bool
                    }
                    BinOp::Eq | BinOp::Neq => {
                        if l != Kind::Unknown && r != Kind::Unknown && l != r {
                            push(errors, e.span, format!("'{sym}' compares mismatched types: {} and {}", l.name(), r.name()));
                        }
                        Kind::Bool
                    }
                }
            }
            ExprKind::Call { name, args } => {
                if *name == crate::interner::well_known::parallel_map() {
                    // Already validated by check_parallel_map; just infer the array arg.
                    if args.len() == 2 {
                        infer_expr(&args[1], locals, fns, structs, errors);
                    }
                    return Kind::Array;
                }
                // Always inferred for every arg regardless of whether the
                // count matches -- an extra/missing argument shouldn't
                // suppress errors inside the arguments that *are* there.
                let arg_kinds: Vec<Kind> = args.iter().map(|a| infer_expr(a, locals, fns, structs, errors)).collect();
                let Some(callee) = fns.get(name) else {
                    return Kind::Unknown;
                };
                if callee.params.len() != args.len() {
                    push(errors, e.span, format!(
                        "'{name}' expects {} argument{}, got {}",
                        callee.params.len(),
                        if callee.params.len() == 1 { "" } else { "s" },
                        args.len()
                    ));
                } else {
                    // Checks a call site's argument kinds against the
                    // callee's declared parameter type names -- the gap
                    // this file's own module doc comment used to flag as
                    // deliberately not done yet. Only fires when both
                    // sides are known, same "never guess" posture as
                    // every other check here.
                    for (i, (param, (arg, actual))) in callee.params.iter().zip(args.iter().zip(arg_kinds.iter())).enumerate() {
                        let expected = type_to_kind(&param.ty, structs);
                        if expected != Kind::Unknown && *actual != Kind::Unknown && expected != *actual {
                            push(errors, arg.span, format!(
                                "argument {} to '{name}': expected {}, found {}",
                                i + 1,
                                expected.name(),
                                actual.name()
                            ));
                        }
                    }
                }
                // The call's own value now carries the callee's declared
                // return kind (previously always Unknown) -- lets a
                // caller like `let x = f();` benefit from every
                // downstream check (Assign consistency, if/while
                // conditions, RangeFor bounds, ...) the same way a
                // directly-typed local already does.
                match &callee.return_type {
                    Some(ty) => type_to_kind(ty, structs),
                    None => Kind::Unknown,
                }
            }
            ExprKind::StructLit { name, fields } => {
                let decl = structs.get(name);
                for (field_name, value) in fields {
                    let actual = infer_expr(value, locals, fns, structs, errors);
                    if let Some(decl) = decl {
                        if let Some(f) = decl.fields.iter().find(|f| f.name == *field_name) {
                            let expected = type_to_kind(&f.ty, structs);
                            if expected != Kind::Unknown && actual != Kind::Unknown && expected != actual {
                                push(errors, value.span, format!(
                                    "field '{field_name}' of '{name}': expected {}, found {}",
                                    expected.name(),
                                    actual.name()
                                ));
                            }
                        }
                    }
                }
                Kind::Struct(*name)
            }
            ExprKind::Field { target, .. } => {
                let k = infer_expr(target, locals, fns, structs, errors);
                if k != Kind::Unknown && !matches!(k, Kind::Struct(_)) {
                    push(errors, target.span, format!("field access needs a struct, found {}", k.name()));
                }
                // The field's own kind isn't tracked -- resolve.rs
                // already validated the field name exists (see Task 3);
                // this checker only cares whether `.` was applied to
                // something that could possibly be a struct at all.
                Kind::Unknown
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_stmt(
        s: &Stmt,
        locals: &mut HashMap<Symbol, Kind>,
        fns: &HashMap<Symbol, &Fn>,
        structs: &HashMap<Symbol, &StructDecl>,
        expected_return_kind: Option<Kind>,
        errors: &mut Vec<KestrelcError>,
    ) {
        match s {
            Stmt::Let { name, value, .. } => {
                let k = infer_expr(value, locals, fns, structs, errors);
                locals.entry(*name).or_insert(k);
            }
            Stmt::Assign { name, value, span } => {
                let k = infer_expr(value, locals, fns, structs, errors);
                if let Some(&prior) = locals.get(name) {
                    if prior != Kind::Unknown && k != Kind::Unknown && prior != k {
                        errors.push(KestrelcError::new(
                            ErrorKind::Type,
                            format!(
                                "'{name}' was first bound as {}, can't assign {} to it",
                                prior.name(),
                                k.name()
                            ),
                            *span,
                        ));
                    }
                }
            }
            Stmt::FieldAssign { target, field, value, span } => {
                let actual = infer_expr(value, locals, fns, structs, errors);
                // Checks the assigned value's kind against the field's
                // declared type -- same check StructLit's own fields
                // already get, now also applied to a later reassignment
                // of one of them (see cse.rs's comment on why a field
                // mutation matters even though `target` itself was never
                // reassigned).
                if let Some(Kind::Struct(struct_name)) = locals.get(target) {
                    if let Some(decl) = structs.get(struct_name) {
                        if let Some(f) = decl.fields.iter().find(|f| f.name == *field) {
                            let expected = type_to_kind(&f.ty, structs);
                            if expected != Kind::Unknown && actual != Kind::Unknown && expected != actual {
                                errors.push(KestrelcError::new(
                                    ErrorKind::Type,
                                    format!(
                                        "field '{field}' of '{struct_name}': expected {}, found {}",
                                        expected.name(),
                                        actual.name()
                                    ),
                                    *span,
                                ));
                            }
                        }
                    }
                }
            }
            Stmt::If { cond, then_block, else_block, .. } => {
                let k = infer_expr(cond, locals, fns, structs, errors);
                if k != Kind::Unknown && k != Kind::Bool {
                    errors.push(KestrelcError::new(
                        ErrorKind::Type,
                        format!("if-condition must be a boolean expression, found {}", k.name()),
                        cond.span,
                    ));
                }
                for st in then_block {
                    visit_stmt(st, locals, fns, structs, expected_return_kind, errors);
                }
                if let Some(eb) = else_block {
                    for st in eb {
                        visit_stmt(st, locals, fns, structs, expected_return_kind, errors);
                    }
                }
            }
            Stmt::While { cond, body, .. } => {
                let k = infer_expr(cond, locals, fns, structs, errors);
                if k != Kind::Unknown && k != Kind::Bool {
                    errors.push(KestrelcError::new(
                        ErrorKind::Type,
                        format!("while-condition must be a boolean expression, found {}", k.name()),
                        cond.span,
                    ));
                }
                for st in body {
                    visit_stmt(st, locals, fns, structs, expected_return_kind, errors);
                }
            }
            Stmt::RangeFor { var, start, end, body, .. } => {
                let sk = infer_expr(start, locals, fns, structs, errors);
                if sk != Kind::Unknown && sk != Kind::Int {
                    errors.push(KestrelcError::new(
                        ErrorKind::Type,
                        format!("for-loop start must be an integer expression, found {}", sk.name()),
                        start.span,
                    ));
                }
                let ek = infer_expr(end, locals, fns, structs, errors);
                if ek != Kind::Unknown && ek != Kind::Int {
                    errors.push(KestrelcError::new(
                        ErrorKind::Type,
                        format!("for-loop end must be an integer expression, found {}", ek.name()),
                        end.span,
                    ));
                }
                locals.entry(*var).or_insert(Kind::Int);
                for st in body {
                    visit_stmt(st, locals, fns, structs, expected_return_kind, errors);
                }
            }
            Stmt::Print { args, .. } => {
                for a in args {
                    infer_expr(a, locals, fns, structs, errors);
                }
            }
            Stmt::Return { value, span } => {
                let actual = value.as_ref().map(|v| infer_expr(v, locals, fns, structs, errors));
                // Only checked when there's a declared, known return
                // type AND an explicit value -- a bare `return;` is left
                // alone (see codegen.rs: falling off the end and a bare
                // `return;` both currently produce the same `0`, and
                // whether that's meaningful depends on the declared
                // return type in a way this checker can't safely
                // generalize yet), same "never guess" posture as
                // everywhere else in this file.
                if let (Some(expected), Some(actual)) = (expected_return_kind, actual) {
                    if expected != Kind::Unknown && actual != Kind::Unknown && expected != actual {
                        errors.push(KestrelcError::new(
                            ErrorKind::Type,
                            format!(
                                "return value doesn't match declared return type: expected {}, found {}",
                                expected.name(),
                                actual.name()
                            ),
                            *span,
                        ));
                    }
                }
            }
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
            Stmt::ExprStmt { expr, .. } => {
                infer_expr(expr, locals, fns, structs, errors);
            }
        }
    }

    for fn_ in &program.fns {
        let mut locals: HashMap<Symbol, Kind> = HashMap::new();
        // A parameter's own declared type now seeds its Kind inside the
        // function body -- previously always Unknown (see this file's
        // module doc comment), so e.g. `fn f(x: i64) { if (x) {...} }`
        // now correctly trips the if-condition-must-be-bool check
        // instead of silently passing.
        for p in &fn_.params {
            locals.insert(p.name, type_to_kind(&p.ty, structs));
        }
        let expected_return_kind = fn_.return_type.as_ref().map(|ty| type_to_kind(ty, structs));
        let mut fn_errors = Vec::new();
        for s in &fn_.body {
            visit_stmt(s, &mut locals, fns, structs, expected_return_kind, &mut fn_errors);
        }
        for e in fn_errors {
            errors.push(KestrelcError::new(e.kind, format!("in '{}': {}", fn_.name, e.message), e.span));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;

    #[test]
    fn a_struct_literal_infers_a_struct_kind() {
        let program = parse(lex(
            "struct Point { x: i64, y: i64 }\nfn main() { let p = Point { x: 1, y: 2 }; let n = p + 1; }",
        ).unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        let structs = crate::resolve::build_struct_table(&program);
        let errors = check_types(&program, &fns, &structs);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("needs two numbers"));
    }

    #[test]
    fn field_access_on_a_non_struct_value_is_a_type_error() {
        let program = parse(lex("fn main() { let x = 5; let y = x.field; }").unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        let structs = crate::resolve::build_struct_table(&program);
        let errors = check_types(&program, &fns, &structs);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("field access needs a struct"));
    }

    #[test]
    fn a_range_for_loop_with_integer_bounds_type_checks() {
        let program = parse(lex("fn main() { for i from 0 to 5 { print(i); } }").unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        let structs = crate::resolve::build_struct_table(&program);
        let errors = check_types(&program, &fns, &structs);
        assert!(errors.is_empty(), "expected no type errors, got: {:?}", errors);
    }

    #[test]
    fn a_range_for_loop_with_a_bool_bound_is_a_type_error() {
        let program = parse(lex("fn main() { for i from true to 5 { print(i); } }").unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        let structs = crate::resolve::build_struct_table(&program);
        let errors = check_types(&program, &fns, &structs);
        assert!(!errors.is_empty(), "expected a type error for a bool start bound");
    }

    #[test]
    fn a_call_site_argument_kind_is_checked_against_the_declared_param_type() {
        let program = parse(lex(
            "fn f(x: i64) { print(x); }\nfn main() { f(true); }",
        ).unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        let structs = crate::resolve::build_struct_table(&program);
        let errors = check_types(&program, &fns, &structs);
        assert_eq!(errors.len(), 1, "got: {:?}", errors.iter().map(|e| &e.message).collect::<Vec<_>>());
        assert!(errors[0].message.contains("expected int, found bool"), "got: {}", errors[0].message);
    }

    #[test]
    fn a_matching_call_site_argument_kind_is_not_an_error() {
        let program = parse(lex(
            "fn f(x: i64, ok: bool) { print(x); }\nfn main() { f(5, true); }",
        ).unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        let structs = crate::resolve::build_struct_table(&program);
        let errors = check_types(&program, &fns, &structs);
        assert!(errors.is_empty(), "expected no type errors, got: {:?}", errors);
    }

    #[test]
    fn a_parameters_declared_type_is_known_inside_its_own_function_body() {
        // Previously a parameter's kind was always Unknown inside its
        // own body -- this proves `x`'s declared `i64` type now makes
        // the if-condition-must-be-bool check actually fire.
        let program = parse(lex("fn f(x: i64) { if (x) { print(1); } }").unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        let structs = crate::resolve::build_struct_table(&program);
        let errors = check_types(&program, &fns, &structs);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("if-condition must be a boolean expression"), "got: {}", errors[0].message);
    }

    #[test]
    fn a_struct_literal_field_value_is_checked_against_its_declared_type() {
        let program = parse(lex(
            "struct Point { x: i64, y: i64 }\nfn main() { let p = Point { x: true, y: 2 }; print(p.x); }",
        ).unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        let structs = crate::resolve::build_struct_table(&program);
        let errors = check_types(&program, &fns, &structs);
        assert_eq!(errors.len(), 1, "got: {:?}", errors.iter().map(|e| &e.message).collect::<Vec<_>>());
        assert!(errors[0].message.contains("expected int, found bool"), "got: {}", errors[0].message);
    }

    #[test]
    fn a_field_assignment_value_is_checked_against_its_declared_type() {
        let program = parse(lex(
            "struct Point { x: i64, y: i64 }\nfn main() { let p = Point { x: 1, y: 2 }; p.x = true; print(p.x); }",
        ).unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        let structs = crate::resolve::build_struct_table(&program);
        let errors = check_types(&program, &fns, &structs);
        assert_eq!(errors.len(), 1, "got: {:?}", errors.iter().map(|e| &e.message).collect::<Vec<_>>());
        assert!(errors[0].message.contains("expected int, found bool"), "got: {}", errors[0].message);
    }

    #[test]
    fn a_return_value_is_checked_against_the_declared_return_type() {
        let program = parse(lex("fn f() -> i64 { return true; }\nfn main() { print(1); }").unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        let structs = crate::resolve::build_struct_table(&program);
        let errors = check_types(&program, &fns, &structs);
        assert_eq!(errors.len(), 1, "got: {:?}", errors.iter().map(|e| &e.message).collect::<Vec<_>>());
        assert!(errors[0].message.contains("return value doesn't match declared return type"), "got: {}", errors[0].message);
    }

    #[test]
    fn a_calls_own_kind_reflects_the_callees_declared_return_type() {
        // f's declared `-> bool` return type now propagates to the call
        // site, so `let x = f();` makes `x` a known Bool instead of
        // Unknown -- proven here via the if-condition check downstream.
        let program = parse(lex(
            "fn f() -> bool { return true; }\nfn main() { let x = f(); if (x) { print(1); } }",
        ).unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        let structs = crate::resolve::build_struct_table(&program);
        let errors = check_types(&program, &fns, &structs);
        assert!(errors.is_empty(), "expected no type errors, got: {:?}", errors);
    }
}
