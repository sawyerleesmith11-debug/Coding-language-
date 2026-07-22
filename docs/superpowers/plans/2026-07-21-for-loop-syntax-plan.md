# For-loop Syntax (Range-for + General-for) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add two new Kestrel loop forms — `for i from 0 to n { }` (range-for, restricted to step-1 ascending count) and `for i = 0, i < n, i = i + 2 { }` (general-for, unrestricted three-clause) — across the lexer, parser, resolve, purity, typecheck, all three codegen backends (native AOT, WASM, JIT), and both AST-to-AST optimization passes (fusion, CSE).

**Architecture:** General-for is pure parser-level sugar — it desugars immediately into the existing `Stmt::Let` + `Stmt::While` + a trailing `Stmt::Assign` for the step, so no other file needs to know it ever existed. Range-for gets one new first-class AST node, `Stmt::RangeFor`, kept distinct all the way to codegen (not desugared) so a future SIMD pass can recognize it directly instead of re-deriving a safety proof from a generic loop.

**Tech Stack:** Rust, Cranelift (native + JIT codegen), `wasm_encoder` (WASM codegen).

## Global Constraints

- Range-for's end bound is **exclusive** (`for i from 0 to n` runs `i = 0, 1, ..., n-1`).
- Range-for's step is always exactly `+1`, never configurable.
- Range-for's `start`/`end` are evaluated exactly once, at loop entry — never re-evaluated per iteration.
- If `start >= end`, range-for runs zero times (not an error).
- General-for's step clause must target the exact same identifier its init clause declared — a parse error otherwise.
- Both forms must work correctly on all three codegen backends: native AOT (`codegen.rs`), WASM (`wasm_codegen.rs`), and JIT (`jit_codegen.rs`), and must keep participating in the `fusion.rs`/`cse.rs` optimization passes when a `RangeFor` body contains a fusable/CSE-able pattern.
- No SIMD/vectorization work of any kind belongs in this plan — that's a separate, future plan.

---

## Task 1: Lexer keywords + `Stmt::RangeFor` AST node

**Files:**
- Modify: `kestrelc/src/lexer.rs:9-52` (add `Tok` variants), `kestrelc/src/lexer.rs:114-128` (add keyword matches)
- Modify: `kestrelc/src/ast.rs:93-101` (add `Stmt::RangeFor` variant)
- Test: `kestrelc/src/lexer.rs` (inline `#[cfg(test)]` module — check if one exists first; if not, add one at the end of the file matching this repo's existing per-file test module convention)

**Interfaces:**
- Produces: `Tok::For`, `Tok::From`, `Tok::To` variants on the existing `Tok` enum (`kestrelc/src/lexer.rs`). `Stmt::RangeFor { var: Symbol, start: Expr, end: Expr, body: Vec<Stmt>, span: Span }` variant on the existing `Stmt` enum (`kestrelc/src/ast.rs`) — every later task in this plan constructs/matches this exact shape, exact field names, exact order.

- [ ] **Step 1: Add the three new `Tok` variants**

In `kestrelc/src/lexer.rs`, in the `Tok` enum (starts at line 9), add three variants right after `While`:

```rust
    While,
    For,
    From,
    To,
    Where,
```

- [ ] **Step 2: Add the three keyword matches in the lexer's word-matching arm**

In `kestrelc/src/lexer.rs`, in the `match word.as_str()` block (starts at line 114), add three arms right after `"while" => Tok::While,`:

```rust
                "while" => Tok::While,
                "for" => Tok::For,
                "from" => Tok::From,
                "to" => Tok::To,
                "where" => Tok::Where,
```

- [ ] **Step 3: Write a failing test for keyword lexing**

Check whether `kestrelc/src/lexer.rs` already has a `#[cfg(test)] mod tests` block at the end of the file (run `grep -n "mod tests" kestrelc/src/lexer.rs` from the `kestrelc/` directory). If it exists, add this test inside it; if not, add a new module at the end of the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_for_from_to_as_keywords_not_identifiers() {
        let tokens = lex("for i from 0 to n {}").unwrap();
        let kinds: Vec<&Tok> = tokens.iter().map(|t| &t.tok).collect();
        assert!(matches!(kinds[0], Tok::For));
        assert!(matches!(kinds[1], Tok::Ident(_)));
        assert!(matches!(kinds[2], Tok::From));
        assert!(matches!(kinds[3], Tok::Number(0)));
        assert!(matches!(kinds[4], Tok::To));
        assert!(matches!(kinds[5], Tok::Ident(_)));
    }
}
```

- [ ] **Step 4: Run the test to verify it fails before Steps 1-2's edits are applied**

This step only makes sense if you write the test *before* editing `Tok`/the keyword match. If you already applied Steps 1-2, skip straight to Step 5's verification instead — the important thing is that the test exists and passes once both are done, not the literal order of edits vs. test-writing for this particular step (a codebase-wide `Tok`/lexer change is small and mechanical enough that TDD's usual red-first ceremony doesn't add real value here; just make sure it passes).

- [ ] **Step 5: Add the `RangeFor` variant to `Stmt`**

In `kestrelc/src/ast.rs`, in the `Stmt` enum (starts at line 93), add a new variant right after `While`:

```rust
pub enum Stmt {
    Let { name: Symbol, value: Expr, span: Span },
    Assign { name: Symbol, value: Expr, span: Span },
    If { cond: Expr, then_block: Vec<Stmt>, else_block: Option<Vec<Stmt>>, span: Span },
    While { cond: Expr, body: Vec<Stmt>, span: Span },
    RangeFor { var: Symbol, start: Expr, end: Expr, body: Vec<Stmt>, span: Span },
    Print { args: Vec<Expr>, span: Span },
    Return { value: Option<Expr>, span: Span },
    ExprStmt { expr: Expr, span: Span },
}
```

- [ ] **Step 6: Run `cargo build` from `kestrelc/` and confirm it fails**

Run: `cargo build 2>&1 | grep "error\[E0004\]" ` (or just `cargo build`)
Expected: several "non-exhaustive match" errors (`E0004`) across `resolve.rs`, `purity.rs`, `typecheck.rs`, `codegen.rs`, `wasm_codegen.rs`, `jit_codegen.rs`, `fusion.rs`, `cse.rs` — every existing `match s { Stmt::Let { .. } => ..., ... }` that doesn't yet have a `RangeFor` arm. This is expected and confirms the new variant is wired into the type; Tasks 2-7 add the missing arms one file at a time.

- [ ] **Step 7: Run the lexer test**

Run: `cargo test --lib lexes_for_from_to_as_keywords_not_identifiers`
Expected: PASS (the lexer/`Tok` change itself compiles and works standalone even though the whole crate doesn't build yet — `cargo test --lib` still fails to build the *crate*, so actually run `cargo build --lib 2>&1 | tail -5` first to confirm the *only* errors are the non-exhaustive matches from Step 6, not something in lexer.rs itself; then proceed — the lexer test will only actually run once Task 3 makes the crate compile again. Note the expected failure mode in your task report so the next task's implementer isn't confused by it.)

- [ ] **Step 8: Commit**

```bash
git add kestrelc/src/lexer.rs kestrelc/src/ast.rs
git commit -m "Add for/from/to keywords and Stmt::RangeFor AST node"
```

---

## Task 2: Parser — both loop forms

**Files:**
- Modify: `kestrelc/src/parser.rs:390-449` (`parse_stmt` — change return type, add both loop forms)
- Modify: `kestrelc/src/parser.rs:380-388` (`parse_block` — adapt to `parse_stmt`'s new `Vec<Stmt>` return)
- Test: `kestrelc/src/parser.rs` (inline `#[cfg(test)] mod tests`, already exists starting at line 489)

**Interfaces:**
- Consumes: `Tok::For`/`Tok::From`/`Tok::To` (Task 1), `Stmt::RangeFor { var, start, end, body, span }` (Task 1).
- Produces: `parse_stmt(&mut self) -> PResult<Vec<Stmt>>` (changed from `PResult<Stmt>`) — every later reader of the parser's output only ever sees `Program`/`Fn`/`Vec<Stmt>` (this was already true), so this signature change is invisible outside `parser.rs` itself.

- [ ] **Step 1: Change `parse_block` to collect a `Vec<Stmt>` per statement**

In `kestrelc/src/parser.rs`, replace the `parse_block` function (lines 380-388):

```rust
    fn parse_block(&mut self) -> PResult<Vec<Stmt>> {
        self.expect(Tok::LBrace)?;
        let mut stmts = Vec::new();
        while !self.at(&Tok::RBrace) {
            stmts.extend(self.parse_stmt()?);
        }
        self.expect(Tok::RBrace)?;
        Ok(stmts)
    }
```

- [ ] **Step 2: Rewrite `parse_stmt` — change its return type and wrap every existing branch, then add both loop forms**

Replace the entire `parse_stmt` function (lines 390-449) with:

```rust
    fn parse_stmt(&mut self) -> PResult<Vec<Stmt>> {
        let span = self.peek().span;
        if self.at(&Tok::Let) {
            self.advance();
            let name = self.expect_ident()?;
            self.expect(Tok::Eq)?;
            let value = self.parse_expr()?;
            self.expect(Tok::Semi)?;
            return Ok(vec![Stmt::Let { name, value, span }]);
        }
        if self.at(&Tok::If) {
            self.advance();
            self.expect(Tok::LParen)?;
            let cond = self.parse_expr()?;
            self.expect(Tok::RParen)?;
            let then_block = self.parse_block()?;
            let else_block = if self.at(&Tok::Else) {
                self.advance();
                Some(self.parse_block()?)
            } else {
                None
            };
            return Ok(vec![Stmt::If { cond, then_block, else_block, span }]);
        }
        if self.at(&Tok::While) {
            self.advance();
            self.expect(Tok::LParen)?;
            let cond = self.parse_expr()?;
            self.expect(Tok::RParen)?;
            let body = self.parse_block()?;
            return Ok(vec![Stmt::While { cond, body, span }]);
        }
        if self.at(&Tok::For) {
            self.advance();
            let var = self.expect_ident()?;
            if self.at(&Tok::From) {
                self.advance();
                let start = self.parse_expr()?;
                self.expect(Tok::To)?;
                let end = self.parse_expr()?;
                let body = self.parse_block()?;
                return Ok(vec![Stmt::RangeFor { var, start, end, body, span }]);
            }
            // General-for: `for i = <init>, <cond>, i = <step> { body }` --
            // desugars directly into `let i = <init>; while (<cond>) {
            // body...; i = <step>; }`, so every downstream pass (resolve,
            // purity, typecheck, all three codegens, fusion, CSE) handles
            // it automatically via the existing Let/While/Assign arms it
            // already has -- no new AST node, no new arm anywhere else in
            // the compiler.
            self.expect(Tok::Eq)?;
            let init_value = self.parse_expr()?;
            self.expect(Tok::Comma)?;
            let cond = self.parse_expr()?;
            self.expect(Tok::Comma)?;
            let step_span = self.peek().span;
            let step_name = self.expect_ident()?;
            if step_name != var {
                return Err(KestrelcError::new(
                    ErrorKind::Parse,
                    format!(
                        "for-loop step must update the same loop variable '{}', found '{}'",
                        var, step_name
                    ),
                    step_span,
                ));
            }
            self.expect(Tok::Eq)?;
            let step_value = self.parse_expr()?;
            let mut body = self.parse_block()?;
            body.push(Stmt::Assign { name: var, value: step_value, span: step_span });
            return Ok(vec![
                Stmt::Let { name: var, value: init_value, span },
                Stmt::While { cond, body, span },
            ]);
        }
        if self.at(&Tok::Print) {
            self.advance();
            self.expect(Tok::LParen)?;
            let args = self.parse_args()?;
            self.expect(Tok::RParen)?;
            self.expect(Tok::Semi)?;
            return Ok(vec![Stmt::Print { args, span }]);
        }
        if self.at(&Tok::Return) {
            self.advance();
            let value = if self.at(&Tok::Semi) { None } else { Some(self.parse_expr()?) };
            self.expect(Tok::Semi)?;
            return Ok(vec![Stmt::Return { value, span }]);
        }
        if let Tok::Ident(name) = &self.peek().tok {
            let name = name.clone();
            if self.tokens[self.pos + 1].tok == Tok::Eq {
                self.advance();
                self.advance();
                let value = self.parse_expr()?;
                self.expect(Tok::Semi)?;
                return Ok(vec![Stmt::Assign { name, value, span }]);
            }
        }
        let expr = self.parse_expr()?;
        self.expect(Tok::Semi)?;
        Ok(vec![Stmt::ExprStmt { expr, span }])
    }
```

Note `Symbol` needs `PartialEq` for the `step_name != var` comparison — check it already derives that (`grep -n "struct Symbol" -A3 kestrelc/src/interner.rs`); if it doesn't, this step also needs `#[derive(PartialEq)]` added there. (It almost certainly already does, since `Symbol == Symbol` comparisons are used throughout the rest of the compiler, e.g. `cse.rs`'s `f_name == *name` — this is a "confirm, don't assume" check, not an expected real change.)

- [ ] **Step 3: Write failing tests for both loop forms**

Add to the existing `#[cfg(test)] mod tests` block in `kestrelc/src/parser.rs` (starts at line 489):

```rust
    #[test]
    fn parses_range_for_into_a_rangefor_node() {
        let program = crate::parser::parse(crate::lexer::lex(
            "fn main() { for i from 0 to 5 { print(i); } }"
        ).unwrap()).unwrap();
        let main_fn = &program.fns[0];
        assert_eq!(main_fn.body.len(), 1);
        let Stmt::RangeFor { var, start, end, body, .. } = &main_fn.body[0] else {
            panic!("expected RangeFor, got {:?}", main_fn.body[0]);
        };
        assert_eq!(var.resolve().as_ref(), "i");
        assert!(matches!(start.kind, ExprKind::Num(0)));
        assert!(matches!(end.kind, ExprKind::Num(5)));
        assert_eq!(body.len(), 1);
    }

    #[test]
    fn parses_general_for_as_a_let_followed_by_a_while() {
        let program = crate::parser::parse(crate::lexer::lex(
            "fn main() { for i = 0, i < 5, i = i + 2 { print(i); } }"
        ).unwrap()).unwrap();
        let main_fn = &program.fns[0];
        assert_eq!(main_fn.body.len(), 2, "general-for must desugar to exactly two top-level statements");
        assert!(matches!(&main_fn.body[0], Stmt::Let { .. }));
        let Stmt::While { body, .. } = &main_fn.body[1] else {
            panic!("expected While as the second desugared statement, got {:?}", main_fn.body[1]);
        };
        // original print(i) plus the appended step assignment
        assert_eq!(body.len(), 2);
        assert!(matches!(&body[1], Stmt::Assign { .. }));
    }

    #[test]
    fn general_for_step_targeting_a_different_variable_is_a_parse_error() {
        let result = crate::parser::parse(crate::lexer::lex(
            "fn main() { for i = 0, i < 5, j = i + 1 { print(i); } }"
        ).unwrap());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("same loop variable"), "got: {}", err.message);
    }
```

(These tests assume `Stmt`/`ExprKind` are already imported at the top of the test module via `use super::*;` plus `use crate::ast::*;` — check the existing test module's imports at the top of the `#[cfg(test)] mod tests` block and add `use crate::ast::*;` if it isn't already there.)

- [ ] **Step 4: Run the crate build**

Run: `cargo build 2>&1 | tail -30`
Expected: still fails with non-exhaustive-match errors in `resolve.rs`/`purity.rs`/`typecheck.rs`/`codegen.rs`/`wasm_codegen.rs`/`jit_codegen.rs`/`fusion.rs`/`cse.rs` (Task 1's Step 6 list, unchanged) — `parser.rs` itself should now compile cleanly on its own. Confirm no errors are reported *in* `parser.rs`.

- [ ] **Step 5: Commit**

```bash
git add kestrelc/src/parser.rs
git commit -m "Parse range-for and general-for statements"
```

---

## Task 3: Resolve, purity, and typecheck support for `RangeFor`

**Files:**
- Modify: `kestrelc/src/resolve.rs:427-487` (`resolve_stmt`)
- Modify: `kestrelc/src/purity.rs:82-132` (`visit_stmt` inside `check_purity`) and `kestrelc/src/purity.rs:240-274` (`visit_stmt` inside `check_parallel_map`)
- Modify: `kestrelc/src/typecheck.rs:205-270ish` (`visit_stmt`)
- Test: `kestrelc/src/resolve.rs`, `kestrelc/src/purity.rs`, `kestrelc/src/typecheck.rs` (each already has an inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Stmt::RangeFor { var, start, end, body, span }` (Task 1).
- Produces: no new public interface — this task's job is purely "don't reject/miscompile a program using `RangeFor`," verified by tests.

- [ ] **Step 1: Add the `RangeFor` arm to `resolve_stmt`**

In `kestrelc/src/resolve.rs`, in `resolve_stmt` (starts at line 427), add a new match arm right after the existing `Stmt::While` arm (line 469-474):

```rust
        Stmt::While { cond, body, span } => {
            resolve_expr(cond, locals, struct_locals, fns, structs, *span, errors);
            for st in body {
                resolve_stmt(st, locals, struct_locals, fns, structs, errors);
            }
        }
        Stmt::RangeFor { var, start, end, body, span } => {
            resolve_expr(start, locals, struct_locals, fns, structs, *span, errors);
            resolve_expr(end, locals, struct_locals, fns, structs, *span, errors);
            locals.insert(*var);
            for st in body {
                resolve_stmt(st, locals, struct_locals, fns, structs, errors);
            }
        }
```

- [ ] **Step 2: Write a failing resolve test**

Add to `kestrelc/src/resolve.rs`'s existing `#[cfg(test)] mod tests` block (starts at line 489):

```rust
    #[test]
    fn a_range_for_loop_variable_is_visible_inside_its_body() {
        let errors = resolve_src(
            "fn main() { for i from 0 to 5 { print(i); } }"
        );
        assert!(errors.is_empty(), "expected no resolve errors, got: {:?}", errors);
    }

    #[test]
    fn a_range_for_loop_variable_used_after_the_loop_still_resolves() {
        // This codebase has no real block scoping (locals is one flat
        // HashSet for the whole function, confirmed by reading
        // resolve_stmt) -- a range-for's `var` behaves exactly like a
        // `let` in that same flat sense: visible afterward too, matching
        // how a `while` loop's own internal `let` would behave. This test
        // documents that existing behavior extends correctly to
        // RangeFor's `var`, not that it's a *desirable* semantic on its
        // own.
        let errors = resolve_src(
            "fn main() { for i from 0 to 5 { } print(i); }"
        );
        assert!(errors.is_empty(), "expected no resolve errors, got: {:?}", errors);
    }
```

- [ ] **Step 3: Add the `RangeFor` arm to `purity.rs`'s `check_purity` walk**

In `kestrelc/src/purity.rs`, in the `visit_stmt` function used by `check_purity` (starts at line 82), add a new arm right after the existing `Stmt::While` arm (lines 116-121):

```rust
                Stmt::While { cond, body, .. } => {
                    visit_expr(cond, fns, cache, stack, impure);
                    for st in body {
                        visit_stmt(st, fns, cache, stack, locals, impure);
                    }
                }
                Stmt::RangeFor { start, end, body, var, .. } => {
                    visit_expr(start, fns, cache, stack, impure);
                    visit_expr(end, fns, cache, stack, impure);
                    locals.insert(*var);
                    for st in body {
                        visit_stmt(st, fns, cache, stack, locals, impure);
                    }
                }
```

- [ ] **Step 4: Add the `RangeFor` arm to `purity.rs`'s `check_parallel_map` walk**

In the same file, in the *other* `visit_stmt` function used by `check_parallel_map` (starts at line 240 — note this one has a different, shorter signature than Step 3's, `(s, fns, errors)` not `(s, fns, cache, stack, locals, impure)`), add a new arm right after its `Stmt::While` arm (lines 256-261):

```rust
            Stmt::While { cond, body, span } => {
                visit_expr(cond, fns, *span, errors);
                for st in body {
                    visit_stmt(st, fns, errors);
                }
            }
            Stmt::RangeFor { start, end, body, span, .. } => {
                visit_expr(start, fns, *span, errors);
                visit_expr(end, fns, *span, errors);
                for st in body {
                    visit_stmt(st, fns, errors);
                }
            }
```

- [ ] **Step 5: Write failing purity tests**

Add to `kestrelc/src/purity.rs`'s existing `#[cfg(test)] mod tests` block:

`purity.rs`'s test module has no `_src`-style helper (unlike `resolve.rs`) — its existing tests inline `parse`/`build_fn_table`/`check_purity` directly (see `a_pure_fn_constructing_and_reading_a_struct_is_still_pure`, line 291). Match that exact pattern:

```rust
    #[test]
    fn a_pure_fn_containing_only_a_range_for_loop_is_still_pure() {
        let program = parse(lex(
            "pure fn sum_to(n: i64) -> i64 { let total = 0; for i from 0 to n { total = total + i; } return total; }",
        ).unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        assert!(check_purity(&program, &fns).is_empty());
    }

    #[test]
    fn a_range_for_loop_containing_print_makes_its_enclosing_fn_impure() {
        let program = parse(lex(
            "pure fn bad(n: i64) -> i64 { for i from 0 to n { print(i); } return 0; }",
        ).unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        assert!(!check_purity(&program, &fns).is_empty(), "expected a purity error for print() inside a pure fn's range-for body");
    }
```

- [ ] **Step 6: Add the `RangeFor` arm to `typecheck.rs`'s `visit_stmt`**

In `kestrelc/src/typecheck.rs`, in `visit_stmt` (starts at line 205), add a new arm right after the existing `Stmt::While` arm (lines 245-257):

```rust
            Stmt::While { cond, body, .. } => {
                let k = infer_expr(cond, locals, fns, errors);
                if k != Kind::Unknown && k != Kind::Bool {
                    errors.push(KestrelcError::new(
                        ErrorKind::Type,
                        format!("while-condition must be a boolean expression, found {}", k.name()),
                        cond.span,
                    ));
                }
                for st in body {
                    visit_stmt(st, locals, fns, errors);
                }
            }
            Stmt::RangeFor { var, start, end, body, .. } => {
                let sk = infer_expr(start, locals, fns, errors);
                if sk != Kind::Unknown && sk != Kind::Int {
                    errors.push(KestrelcError::new(
                        ErrorKind::Type,
                        format!("for-loop start must be an integer expression, found {}", sk.name()),
                        start.span,
                    ));
                }
                let ek = infer_expr(end, locals, fns, errors);
                if ek != Kind::Unknown && ek != Kind::Int {
                    errors.push(KestrelcError::new(
                        ErrorKind::Type,
                        format!("for-loop end must be an integer expression, found {}", ek.name()),
                        end.span,
                    ));
                }
                locals.entry(*var).or_insert(Kind::Int);
                for st in body {
                    visit_stmt(st, locals, fns, errors);
                }
            }
```

- [ ] **Step 7: Write failing typecheck tests**

`typecheck.rs`'s test module also has no `_src` helper (see `a_struct_literal_infers_a_struct_kind`, line 294) — inline `parse`/`build_fn_table`/`check_types` the same way:

```rust
    #[test]
    fn a_range_for_loop_with_integer_bounds_type_checks() {
        let program = parse(lex("fn main() { for i from 0 to 5 { print(i); } }").unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        let errors = check_types(&program, &fns);
        assert!(errors.is_empty(), "expected no type errors, got: {:?}", errors);
    }

    #[test]
    fn a_range_for_loop_with_a_bool_bound_is_a_type_error() {
        let program = parse(lex("fn main() { for i from true to 5 { print(i); } }").unwrap()).unwrap();
        let fns = crate::resolve::build_fn_table(&program);
        let errors = check_types(&program, &fns);
        assert!(!errors.is_empty(), "expected a type error for a bool start bound");
    }
```

- [ ] **Step 8: Run the crate build**

Run: `cargo build 2>&1 | tail -30`
Expected: still fails with non-exhaustive-match errors, now only in `codegen.rs`/`wasm_codegen.rs`/`jit_codegen.rs`/`fusion.rs`/`cse.rs`. Confirm `resolve.rs`, `purity.rs`, `typecheck.rs` compile cleanly.

- [ ] **Step 9: Commit**

```bash
git add kestrelc/src/resolve.rs kestrelc/src/purity.rs kestrelc/src/typecheck.rs
git commit -m "Add RangeFor support to resolve, purity, and typecheck passes"
```

---

## Task 4: Native AOT codegen (`codegen.rs`)

**Files:**
- Modify: `kestrelc/src/codegen.rs` — four touch points (see below)
- Test: `kestrelc/tests/integration.rs` (this repo's existing convention for native-backend end-to-end tests — compiles and runs a real `.kes` program via the built `kestrelc` binary)

**Interfaces:**
- Consumes: `Stmt::RangeFor { var, start, end, body, span }` (Task 1). `Slot::Scalar(Variable)` / `SlotKind::Scalar` (existing, `codegen.rs:921-937`). `self.vars: HashMap<Symbol, Slot>` (existing).
- Produces: no new public interface — a `RangeFor`-using native binary compiles and runs correctly.

- [ ] **Step 1: Add the `RangeFor` arm to the where-clause proof pass's statement walker**

This is the `visit_stmts` closure inside whatever function contains it (around line 220-246 — the where-clause bounds-proof collection pass). Add a new arm right after its existing `Stmt::While` arm:

```rust
                Stmt::While { cond, body, .. } => {
                    visit_expr(cond, known_lens, array_positions, proofs);
                    visit_stmts(body, known_lens, array_positions, proofs);
                }
                Stmt::RangeFor { start, end, body, .. } => {
                    visit_expr(start, known_lens, array_positions, proofs);
                    visit_expr(end, known_lens, array_positions, proofs);
                    visit_stmts(body, known_lens, array_positions, proofs);
                }
```

(A `RangeFor`'s own loop variable is a fresh scalar the where-clause proof system has no special knowledge of — no `known_lens` entry needed for it, matching how this same pass doesn't special-case a `while` loop's own internal scalar `let`s either.)

- [ ] **Step 2: Add the `RangeFor` arm to `walk_slots`**

In `walk_slots` (starts at line 990), add a new arm right after the existing `Stmt::While` arm (line 1011):

```rust
            Stmt::While { body, .. } => walk_slots(body, slots, seen, known_lens),
            Stmt::RangeFor { var, body, .. } => {
                add_slot(*var, SlotKind::Scalar, slots, seen);
                walk_slots(body, slots, seen, known_lens);
            }
```

- [ ] **Step 3: Add `RangeFor` to the span-setting match in `gen_stmt`**

In `gen_stmt` (starts at line 1250), add `Stmt::RangeFor { span, .. }` to the existing span-union pattern (lines 1252-1258):

```rust
        self.cur_span = match s {
            Stmt::Let { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::If { span, .. }
            | Stmt::While { span, .. }
            | Stmt::RangeFor { span, .. }
            | Stmt::Print { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::ExprStmt { span, .. } => *span,
        };
```

- [ ] **Step 4: Add the `RangeFor` codegen arm**

In the same `gen_stmt` function, add a new match arm right after the existing `Stmt::While` arm (lines 1308-1331):

```rust
            Stmt::RangeFor { var, start, end, body, .. } => {
                // var = start
                let start_v = self.gen_expr(start)?;
                let Slot::Scalar(var_v) = self.vars[var] else {
                    return Err(self.err(format!(
                        "internal error: range-for loop variable '{var}' wasn't declared as a scalar slot"
                    )));
                };
                self.builder.def_var(var_v, start_v);

                // end is evaluated exactly once, here, before the loop --
                // its Cranelift Value dominates every block below (this
                // block, header_blk, body_blk), so referencing it inside
                // the loop's condition on every iteration is sound and
                // never re-evaluates the `end` expression.
                let end_v = self.gen_expr(end)?;

                let header_blk = self.builder.create_block();
                let body_blk = self.builder.create_block();
                let after_blk = self.builder.create_block();

                self.builder.ins().jump(header_blk, &[]);

                self.builder.switch_to_block(header_blk);
                let cur = self.builder.use_var(var_v);
                let c = self.builder.ins().icmp(IntCC::SignedLessThan, cur, end_v);
                self.builder.ins().brif(c, body_blk, &[], after_blk, &[]);

                self.builder.switch_to_block(body_blk);
                let body_term = self.gen_block(body)?;
                if !body_term {
                    let cur = self.builder.use_var(var_v);
                    let next = self.builder.ins().iadd_imm(cur, 1);
                    self.builder.def_var(var_v, next);
                    self.builder.ins().jump(header_blk, &[]);
                }
                self.builder.seal_block(body_blk);
                self.builder.seal_block(header_blk);

                self.builder.switch_to_block(after_blk);
                self.builder.seal_block(after_blk);
                Ok(false)
            }
```

- [ ] **Step 5: Run the crate build**

Run: `cargo build 2>&1 | tail -30`
Expected: still fails with non-exhaustive-match errors, now only in `wasm_codegen.rs`/`jit_codegen.rs`/`fusion.rs`/`cse.rs`. Confirm `codegen.rs` compiles cleanly (this is the first point the native `kestrelc` binary itself can build — try `cargo build --release --bin kestrelc 2>&1 | tail -10` too, since `wasm_codegen.rs`/`jit_codegen.rs` are also linked into that same binary and will still block it; this is expected until Tasks 5-6 land).

- [ ] **Step 6: Write a failing end-to-end test**

Add to `kestrelc/tests/integration.rs`, following this file's existing pattern exactly (`scratch_dir`, write a `.kes` file, run `kestrelc_bin()`, run the compiled binary, assert on `native_stdout`):

```rust
#[test]
fn range_for_sums_an_array_correctly() {
    let scratch = scratch_dir("range_for_sum");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "fn main() {\n\
         \x20   let arr = [10, 20, 30, 40, 50];\n\
         \x20   let total = 0;\n\
         \x20   for i from 0 to 5 {\n\
         \x20       total = total + arr[i];\n\
         \x20   }\n\
         \x20   print(total);\n\
         }\n",
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let bin = scratch.join("prog");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(native_stdout(&run), "150\n");
}

#[test]
fn range_for_with_start_equal_to_end_runs_zero_times() {
    let scratch = scratch_dir("range_for_zero");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "fn main() {\n\
         \x20   let total = 0;\n\
         \x20   for i from 5 to 5 {\n\
         \x20       total = total + 1;\n\
         \x20   }\n\
         \x20   print(total);\n\
         }\n",
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let bin = scratch.join("prog");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(native_stdout(&run), "0\n");
}

#[test]
fn general_for_counts_down_by_two() {
    let scratch = scratch_dir("general_for_countdown");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "fn main() {\n\
         \x20   for i = 10, i > 0, i = i - 2 {\n\
         \x20       print(i);\n\
         \x20   }\n\
         }\n",
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let bin = scratch.join("prog");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(native_stdout(&run), "10\n8\n6\n4\n2\n");
}
```

- [ ] **Step 7: Run the new tests**

Run: `cargo test --release range_for_sums_an_array_correctly range_for_with_start_equal_to_end_runs_zero_times general_for_counts_down_by_two`
Expected: PASS, all three (this will build the whole crate, including `wasm_codegen.rs`/`jit_codegen.rs` — if Tasks 5-6 haven't landed yet, `cargo test` won't compile at all; if working through this plan in order, skip running this step for real until after Task 6, but the test code itself belongs in this task's commit since it's testing this task's codegen path specifically).

- [ ] **Step 8: Commit**

```bash
git add kestrelc/src/codegen.rs kestrelc/tests/integration.rs
git commit -m "Add native AOT codegen for RangeFor"
```

---

## Task 5: WASM codegen (`wasm_codegen.rs`)

**Files:**
- Modify: `kestrelc/src/wasm_codegen.rs` — three touch points, plus a new scratch-local allocation mechanism (see below)
- Test: `kestrelc/tests/integration.rs` (this repo's existing convention includes WASM-backend variants of native tests — check for an existing `wasm_backend_*` test near the native tests it mirrors, e.g. the `where_clause` tests around line 380, for the exact pattern: compile with `--wasm`, load and run via whatever WASM runtime the existing tests already use)

**Interfaces:**
- Consumes: `Stmt::RangeFor { var, start, end, body, span }` (Task 1).
- Produces: no new public interface — a `RangeFor`-using program compiles correctly to WASM and produces correct output when run.

**Design note before starting:** unlike native/JIT codegen (Cranelift SSA `Value`s can be referenced across blocks directly, by dominance, with no extra bookkeeping), WASM's structured stack machine has no equivalent — evaluating `end` once and reusing it inside the loop condition on every iteration requires storing it in a real WASM local, not just leaving it on the value stack. This needs one scratch i64 local *per `RangeFor` occurrence* (not one shared scratch reused across the whole function — a `RangeFor` nested inside another `RangeFor`'s body would otherwise have its outer loop's `end` value clobbered by the inner loop reusing the same scratch slot). The existing code already has a precedent for a per-function scratch local (the single scratch i32 local at `wasm_codegen.rs:319-325`, used for a different, single-value purpose) — this task adds an analogous but *per-occurrence* mechanism.

- [ ] **Step 1: Add the `RangeFor` arm to `walk_slots`**

In `walk_slots` (starts at line ~215-225 — the function containing the arm at line 244), add a new arm right after the existing `Stmt::While` arm:

```rust
            Stmt::While { body, .. } => walk_slots(body, slots, seen, known_lens),
            Stmt::RangeFor { var, body, .. } => {
                add_slot(*var, SlotKind::Scalar, slots, seen);
                walk_slots(body, slots, seen, known_lens);
            }
```

- [ ] **Step 2: Add a per-occurrence scratch-local counter for `RangeFor`'s `end` value**

Find the section around lines 276-325 where `next_local`/`extra_locals` are built up (the same place the existing single scratch i32 local is allocated). Add a small recursive counter function right before that section, and reserve one scratch i64 local per `RangeFor` found in the function body:

```rust
/// One scratch i64 local per `RangeFor` occurrence in `stmts` (recursing
/// into every nested block) -- `end` is evaluated once per range-for and
/// must survive across the loop's whole body, so unlike this file's other
/// single shared scratch local, a nested range-for needs its own slot or
/// an outer loop's `end` value would be clobbered by an inner one reusing
/// the same local.
fn count_range_fors(stmts: &[Stmt]) -> usize {
    let mut count = 0;
    for s in stmts {
        match s {
            Stmt::If { then_block, else_block, .. } => {
                count += count_range_fors(then_block);
                if let Some(eb) = else_block {
                    count += count_range_fors(eb);
                }
            }
            Stmt::While { body, .. } => count += count_range_fors(body),
            Stmt::RangeFor { body, .. } => {
                count += 1;
                count += count_range_fors(body);
            }
            _ => {}
        }
    }
    count
}
```

Then, in the `next_local`/`extra_locals` setup section, add (matching the existing style there — read the surrounding ~15 lines first to match exact variable names in context):

```rust
    let range_for_end_locals: Vec<u32> = (0..count_range_fors(&f.body))
        .map(|_| {
            let idx = next_local;
            next_local += 1;
            extra_locals.push((1, ValType::I64));
            idx
        })
        .collect();
```

- [ ] **Step 3: Thread `range_for_end_locals` and a cursor into `FnWasm`**

In `kestrelc/src/wasm_codegen.rs`, add two fields to the `FnWasm<'a>` struct (starts at line 345, right after the existing `scratch: u32,` field at line ~360):

```rust
struct FnWasm<'a> {
    func: &'a mut Function,
    vars: HashMap<Symbol, VarLoc>,
    fn_indices: &'a HashMap<Symbol, u32>,
    where_info: &'a HashMap<Symbol, WhereInfo>,
    my_where: Option<&'a WhereInfo>,
    struct_table: &'a HashMap<Symbol, &'a StructDecl>,
    data_bytes: &'a mut Vec<u8>,
    str_offsets: &'a mut HashMap<String, (u32, u32)>,
    scratch: u32,
    range_for_end_locals: Vec<u32>,
    range_for_cursor: usize,
    cur_span: Span,
}
```

(Read the actual full field list first via `sed -n '345,370p' kestrelc/src/wasm_codegen.rs` and insert the two new fields into it exactly as shown above, in whatever exact order the existing fields are already in — the block above is illustrative of the two new fields' types, not a claim about every other field's exact existing order.)

In `gen_fn` (the function that constructs `FnWasm`, starts at line 261), add `range_for_end_locals` (Step 2's computed `Vec<u32>`) and `range_for_cursor: 0` to the `FnWasm { ... }` literal at lines 326-337:

```rust
    let mut fc = FnWasm {
        func: &mut func,
        vars,
        fn_indices,
        where_info,
        my_where,
        struct_table,
        data_bytes,
        str_offsets,
        scratch,
        range_for_end_locals,
        range_for_cursor: 0,
        cur_span: f.span,
    };
```

- [ ] **Step 4: Add `RangeFor` to the span-setting match in `gen_stmt`**

In `gen_stmt` (starts at line 523), add `Stmt::RangeFor { span, .. }` to the existing span-union pattern (lines 525-531):

```rust
        self.cur_span = match s {
            Stmt::Let { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::If { span, .. }
            | Stmt::While { span, .. }
            | Stmt::RangeFor { span, .. }
            | Stmt::Print { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::ExprStmt { span, .. } => *span,
        };
```

- [ ] **Step 5: Add the `RangeFor` codegen arm**

In the same `gen_stmt` function, add a new arm right after the existing `Stmt::While` arm (lines 553-564):

```rust
            Stmt::RangeFor { var, start, end, body, .. } => {
                self.gen_binding(*var, start)?; // var = start

                // Evaluate `end` exactly once and stash it in this
                // occurrence's own scratch local -- see this task's
                // design note for why a shared scratch local is unsafe
                // for nested range-for loops.
                let end_local = self.range_for_end_locals[self.range_for_cursor];
                self.range_for_cursor += 1;
                self.gen_expr(end)?;
                self.func.instructions().local_set(end_local);

                self.func.instructions().block(wasm_encoder::BlockType::Empty);
                self.func.instructions().loop_(wasm_encoder::BlockType::Empty);

                let VarLoc::Scalar(var_local) = self.vars[var] else {
                    return Err(self.err(format!(
                        "internal error: range-for loop variable '{var}' wasn't declared as a scalar slot"
                    )));
                };
                self.func.instructions().local_get(var_local);
                self.func.instructions().local_get(end_local);
                self.func.instructions().i64_ge_s(); // var >= end -> exit
                self.func.instructions().br_if(1);

                self.gen_block(body)?;

                self.func.instructions().local_get(var_local);
                self.func.instructions().i64_const(1);
                self.func.instructions().i64_add();
                self.func.instructions().local_set(var_local);
                self.func.instructions().br(0);

                self.func.instructions().end(); // end loop
                self.func.instructions().end(); // end block
                Ok(())
            }
```

`VarLoc::Scalar(u32)` (confirmed at `wasm_codegen.rs:170-175`) is the exact variant used above — a bare WASM local index, matching how every other `VarLoc::Scalar` read/write in this file already works (see `gen_binding`'s `Slot::Scalar` handling for the exact same `local_get`/`local_set` pattern this arm reuses).

- [ ] **Step 6: Run the crate build**

Run: `cargo build 2>&1 | tail -30`
Expected: still fails with non-exhaustive-match errors, now only in `jit_codegen.rs`/`fusion.rs`/`cse.rs`.

- [ ] **Step 7: Write a failing WASM end-to-end test**

Mirrors the existing `wasm_backend_where_clause_call_site_proof_accepts_valid_literal_call` test's exact structure (`kestrelc/tests/integration.rs:380-413`) — `--wasm` flag, `run_wasm_via_node` helper, `.wasm` extension on the compiled output path:

```rust
#[test]
fn wasm_backend_range_for_sums_an_array_correctly() {
    // Mirrors range_for_sums_an_array_correctly (codegen.rs's native
    // path) -- same program, WASM backend instead.
    let scratch = scratch_dir("wasm_range_for_sum");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "fn main() {\n\
         \x20   let arr = [10, 20, 30, 40, 50];\n\
         \x20   let total = 0;\n\
         \x20   for i from 0 to 5 {\n\
         \x20       total = total + arr[i];\n\
         \x20   }\n\
         \x20   print(total);\n\
         }\n",
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg("--wasm")
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "kestrelc --wasm failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let wasm_path = scratch.join("prog.wasm");
    let run = run_wasm_via_node(&wasm_path);
    assert!(run.status.success(), "node failed to run the wasm module:\n{}", String::from_utf8_lossy(&run.stderr));
    assert_eq!(String::from_utf8_lossy(&run.stdout), "150\n");
}
```

- [ ] **Step 8: Run the new test**

Run: `cargo test --release wasm_backend_range_for_sums_an_array_correctly`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add kestrelc/src/wasm_codegen.rs kestrelc/tests/integration.rs
git commit -m "Add WASM codegen for RangeFor"
```

---

## Task 6: JIT codegen (`jit_codegen.rs`)

**Files:**
- Modify: `kestrelc/src/jit_codegen.rs` — `collect_let_names`, `check_stmts_supported`, `gen_stmt`'s span match, `gen_stmt`'s main lowering (four touch points), and its own `#[cfg(test)] mod tests` block (starts at line 984, already has a `jit_run` test helper)

**Interfaces:**
- Consumes: `Stmt::RangeFor { var, start, end, body, span }` (Task 1).
- Produces: no new public interface — a `RangeFor`-using program stays JIT-eligible (`check_jit_supported` doesn't reject it) and executes correctly under the JIT path.

- [ ] **Step 1: Add the `RangeFor` arm to `collect_let_names`**

In `collect_let_names`'s inner `walk` function (starts at line 582), add a new arm right after the existing `Stmt::While` arm (line 594):

```rust
                Stmt::While { body, .. } => walk(body, names),
                Stmt::RangeFor { var, body, .. } => {
                    names.push(*var);
                    walk(body, names);
                }
```

- [ ] **Step 2: Add `RangeFor` to `check_stmts_supported`**

In `check_stmts_supported` (starts at line 229), add a new arm right after the existing `Stmt::While` arm (lines 240-243):

```rust
            Stmt::While { cond, body, .. } => {
                check_expr_supported(cond, false)?;
                check_stmts_supported(body)?;
            }
            Stmt::RangeFor { start, end, body, .. } => {
                check_expr_supported(start, false)?;
                check_expr_supported(end, false)?;
                check_stmts_supported(body)?;
            }
```

This makes `RangeFor` JIT-eligible whenever its `start`/`end`/`body` are all otherwise JIT-supported (same rule `While` already follows) — a `RangeFor` program is never rejected by `check_jit_supported` purely for using this new loop form.

- [ ] **Step 3: Add `RangeFor` to the span-setting match in `gen_stmt`**

In `gen_stmt` (starts at line 633), add `Stmt::RangeFor { span, .. }` to the existing span-union pattern (lines 635-641):

```rust
        self.cur_span = match s {
            Stmt::Let { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::If { span, .. }
            | Stmt::While { span, .. }
            | Stmt::RangeFor { span, .. }
            | Stmt::Print { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::ExprStmt { span, .. } => *span,
        };
```

- [ ] **Step 4: Add the `RangeFor` codegen arm**

In the same `gen_stmt` function, add a new match arm right after the existing `Stmt::While` arm (matches the block starting at line 684 you read earlier — same shape as `codegen.rs`'s native arm from Task 4 Step 4, since this file's `self.vars: HashMap<Symbol, Variable>` already stores a bare `Variable` per scalar, simpler than native `codegen.rs`'s `Slot` enum since JIT v1 only ever has scalars):

```rust
            Stmt::RangeFor { var, start, end, body, .. } => {
                let start_v = self.gen_expr(start)?;
                let var_v = self.vars[var];
                self.builder.def_var(var_v, start_v);

                let end_v = self.gen_expr(end)?;

                let header_blk = self.builder.create_block();
                let body_blk = self.builder.create_block();
                let after_blk = self.builder.create_block();

                self.builder.ins().jump(header_blk, &[]);

                self.builder.switch_to_block(header_blk);
                let cur = self.builder.use_var(var_v);
                let c = self.builder.ins().icmp(IntCC::SignedLessThan, cur, end_v);
                self.builder.ins().brif(c, body_blk, &[], after_blk, &[]);

                self.builder.switch_to_block(body_blk);
                let body_term = self.gen_block(body)?;
                if !body_term {
                    let cur = self.builder.use_var(var_v);
                    let next = self.builder.ins().iadd_imm(cur, 1);
                    self.builder.def_var(var_v, next);
                    self.builder.ins().jump(header_blk, &[]);
                }
                self.builder.seal_block(body_blk);
                self.builder.seal_block(header_blk);

                self.builder.switch_to_block(after_blk);
                self.builder.seal_block(after_blk);
                Ok(false)
            }
```

Confirm `IntCC` is already imported at the top of `jit_codegen.rs` (it almost certainly is, given the file already uses Cranelift IR directly elsewhere) — if not, add the same import `codegen.rs` uses.

- [ ] **Step 5: Run the crate build**

Run: `cargo build 2>&1 | tail -30`
Expected: still fails with non-exhaustive-match errors, now only in `fusion.rs`/`cse.rs`.

- [ ] **Step 6: Write failing JIT end-to-end tests using this file's own existing `jit_run` test helper**

`jit_codegen.rs`'s own `#[cfg(test)] mod tests` block (starts at line 984) already has a `jit_run(src: &str) -> i64` helper that parses, checks JIT eligibility, compiles, and runs a program, returning `main`'s return value — use it directly rather than re-deriving the JIT invocation sequence. Add two tests to that same module:

```rust
    #[test]
    fn range_for_sums_zero_through_four_via_jit() {
        let result = jit_run(
            "fn main() {\n\
             \x20   let total = 0;\n\
             \x20   for i from 0 to 5 {\n\
             \x20       total = total + i;\n\
             \x20   }\n\
             \x20   return total;\n\
             }\n",
        );
        assert_eq!(result, 10);
    }

    #[test]
    fn general_for_counts_down_via_jit() {
        let result = jit_run(
            "fn main() {\n\
             \x20   let total = 0;\n\
             \x20   for i = 5, i > 0, i = i - 1 {\n\
             \x20       total = total + i;\n\
             \x20   }\n\
             \x20   return total;\n\
             }\n",
        );
        assert_eq!(result, 15);
    }
```

Note `jit_run` already calls `check_jit_supported(&program).expect(...)` internally — if Step 2's `check_stmts_supported` arm is missing or wrong, these tests fail at that `.expect()` with a clear panic message, not a silent skip, so this doubles as the eligibility-check test without needing a separate one.

- [ ] **Step 7: Run the new tests**

Run: `cargo test --release --lib range_for_sums_zero_through_four_via_jit general_for_counts_down_via_jit`
Expected: PASS, both.

- [ ] **Step 8: Commit**

```bash
git add kestrelc/src/jit_codegen.rs
git commit -m "Add JIT codegen and eligibility support for RangeFor"
```

---

## Task 7: Fusion, CSE, and hot-fn inlining support for `RangeFor` bodies

**Amendment note:** the original version of this task covered only `fusion.rs`/`cse.rs`. During Task 2's implementation, a third AST-to-AST pass, `inline.rs` (hot-fn inlining, gated on profile data — see `inline_hot_fns`), was discovered to also have two exhaustive `Stmt` matches with no `RangeFor` arm, missed during this plan's original research. Folded in here since it's the same category of pass (an AST-to-AST optimization walker recursing into loop bodies) as fusion/CSE, not because it was in scope from the start.

**Files:**
- Modify: `kestrelc/src/fusion.rs:257-269` (`fuse_body`'s nested-block recursion)
- Modify: `kestrelc/src/cse.rs:139-180ish` (`cse_block`'s statement match)
- Modify: `kestrelc/src/inline.rs:93-119` (`walk_stmts_exprs`) and `kestrelc/src/inline.rs:251-283` (`inline_stmts`)
- Test: `kestrelc/src/fusion.rs`, `kestrelc/src/cse.rs`, `kestrelc/src/inline.rs` (each already has an inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Stmt::RangeFor { var, start, end, body, span }` (Task 1).
- Produces: no new public interface — a `parallel_map` chain, a repeated pure call, or a hot inlinable call inside a `RangeFor` body gets the same optimization treatment it would get inside an equivalent `while` loop's body.

- [ ] **Step 0a: Add the `RangeFor` arm to `inline.rs`'s `walk_stmts_exprs`**

In `kestrelc/src/inline.rs`, in `walk_stmts_exprs` (starts at line 93), add a new arm right after the existing `Stmt::While` arm (lines 106-109):

```rust
            Stmt::While { cond, body, .. } => {
                on_expr(cond);
                walk_stmts_exprs(body, on_expr);
            }
            Stmt::RangeFor { start, end, body, .. } => {
                on_expr(start);
                on_expr(end);
                walk_stmts_exprs(body, on_expr);
            }
```

- [ ] **Step 0b: Add the `RangeFor` arm to `inline.rs`'s `inline_stmts`**

In the same file, in `inline_stmts` (starts at line 251), add a new arm right after the existing `Stmt::While` arm (lines 267-271):

```rust
            Stmt::While { cond, body, span } => Stmt::While {
                cond: inline_expr(cond, candidates),
                body: inline_stmts(body, candidates),
                span: *span,
            },
            Stmt::RangeFor { var, start, end, body, span } => Stmt::RangeFor {
                var: *var,
                start: inline_expr(start, candidates),
                end: inline_expr(end, candidates),
                body: inline_stmts(body, candidates),
                span: *span,
            },
```

- [ ] **Step 0c: Write a failing inlining test**

Check `kestrelc/src/inline.rs`'s existing `#[cfg(test)] mod tests` block for its test-helper pattern (likely calls `inline_hot_fns` directly with a constructed `profile: HashMap<String, u64>`), then add one test confirming a hot, small, expression-bodied pure function called inside a `RangeFor` body still gets inlined — mirror whatever existing test already proves this for a `while`-bodied call site (search for an existing test asserting inlining happens inside a loop body; if none exists for `while` either, write the `RangeFor` version following this file's general test pattern: construct a program with a `RangeFor` body containing a call to a small hot pure fn, call `inline_hot_fns` with that fn's name present with a high count in the profile map, assert the call site's AST no longer contains a `Call` to that function name, confirming substitution happened).

- [ ] **Step 1: Add the `RangeFor` arm to `fuse_body`'s nested-block recursion**

In `kestrelc/src/fusion.rs`, in `fuse_body` (the block starting at line 257 that recurses into `If`/`While`), add a new arm:

```rust
    for s in body.iter_mut() {
        match s {
            Stmt::If { then_block, else_block, .. } => {
                fuse_body(then_block, fns, extra_fns, counter);
                if let Some(eb) = else_block {
                    fuse_body(eb, fns, extra_fns, counter);
                }
            }
            Stmt::While { body: wbody, .. } => fuse_body(wbody, fns, extra_fns, counter),
            Stmt::RangeFor { body: rbody, .. } => fuse_body(rbody, fns, extra_fns, counter),
            _ => {}
        }
    }
```

Note this loop only recurses into nested bodies — it does *not* also need to scan `RangeFor`'s `start`/`end` expressions for fusable `parallel_map` chains, since `as_parallel_map_call` and `match_fusion` only ever look for `parallel_map` calls as the *entire value* of a `Let` statement (never inside an arbitrary sub-expression like a loop bound) — matching this file's existing, deliberately narrow scope (see the file's own top-of-file comment).

- [ ] **Step 2: Write a failing fusion test**

Add to `kestrelc/src/fusion.rs`'s existing `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn fuses_two_chained_parallel_map_calls_inside_a_range_for_body() {
        let program = parse_src(
            "
            pure fn square(x: i32) -> i32 { return x * x; }
            pure fn inc(x: i32) -> i32 { return x + 1; }
            fn main() {
                for i from 0 to 3 {
                    let a = parallel_map(square, [1, 2, 3, 4]);
                    let b = parallel_map(inc, a);
                    print(b[0]);
                }
            }
            ",
        );
        let fused = fuse_loops(&program);
        assert_eq!(fused.fns.len(), 4, "should have added exactly one fused function, same as the top-level-body version of this test");
    }
```

- [ ] **Step 3: Add the `RangeFor` arm to `cse_block`**

In `kestrelc/src/cse.rs`, in `cse_block` (starts at line 139), add a new arm right after the existing `Stmt::While` arm:

```rust
            Stmt::While { cond, body: wbody, .. } => {
                rewrite_expr(cond, fns, &available);
                cse_block(wbody, fns);
            }
            Stmt::RangeFor { start, end, body: rbody, .. } => {
                rewrite_expr(start, fns, &available);
                rewrite_expr(end, fns, &available);
                cse_block(rbody, fns);
            }
```

(Matches `While`'s existing treatment exactly: `start`/`end` are rewritten against the *current* available table — same as `While`'s `cond` — but the loop body gets a fresh, empty table via its own `cse_block` call, never carrying availability into or out of the loop, for the same reasoning already documented at the top of `cse.rs` for `While`'s body.)

- [ ] **Step 4: Write a failing CSE test**

Add to `kestrelc/src/cse.rs`'s existing `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn reuses_a_let_bound_call_result_inside_a_range_for_body() {
        let program = parse_src(
            "
            pure fn square(x: i32) -> i32 { return x * x; }
            fn main() {
                for i from 0 to 3 {
                    let a = square(5);
                    let total = 0;
                    total = total + square(5);
                    print(total, a);
                }
            }
            ",
        );
        let out = eliminate_common_calls(&program);
        let main_fn = out.fns.iter().find(|f| f.name.resolve().as_ref() == "main").unwrap();
        let Stmt::RangeFor { body, .. } = &main_fn.body[0] else { panic!("expected RangeFor") };
        let Stmt::Assign { value, .. } = &body[2] else { panic!("expected assign") };
        let ExprKind::Binop { right, .. } = &value.kind else { panic!("expected binop") };
        assert!(matches!(&right.kind, ExprKind::Ident(n) if n.resolve().as_ref() == "a"), "second call inside the range-for body should reuse `a`, got: {:?}", right.kind);
    }
```

- [ ] **Step 5: Run the crate build and full test suite**

Run: `cargo build --release 2>&1 | tail -30`
Expected: builds cleanly — this is the first point in the plan the whole crate compiles again (every non-exhaustive-match error from Task 1's Step 6 should now be resolved).

Run: `cargo test --release 2>&1 | tail -40`
Expected: every new test from Tasks 1-7 passes, plus the entire pre-existing suite still passes (aside from the known pre-existing flaky timing test, `an_array_param_fn_with_one_agreed_length_actually_gets_memoized`, unrelated to this work — see this repo's own notes on that test if it fails; it only fails under parallel test-suite load, always passes run alone).

- [ ] **Step 6: Commit**

```bash
git add kestrelc/src/fusion.rs kestrelc/src/cse.rs kestrelc/src/inline.rs
git commit -m "Add RangeFor support to fusion, CSE, and hot-fn inlining passes"
```

---

## Task 8: End-to-end coverage across all three backends together, plus edge cases

**Files:**
- Test: `kestrelc/tests/integration.rs`

**Interfaces:**
- Consumes: everything from Tasks 1-7. This task adds no new production code, only tests that exercise the whole feature as a user would, cross-checking behavior no single earlier task's narrower tests already covered.

- [ ] **Step 1: Write a test proving general-for's arbitrary step actually works end-to-end**

```rust
#[test]
fn general_for_supports_a_data_dependent_early_style_condition() {
    // General-for's condition can be arbitrary -- not just a bound check
    // against a fixed end value. This is the shape range-for can never
    // express (see the design spec's "why two forms" reasoning).
    let scratch = scratch_dir("general_for_data_dependent");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "fn main() {\n\
         \x20   let arr = [3, 5, 0, 9, 1];\n\
         \x20   let total = 0;\n\
         \x20   for i = 0, arr[i] != 0, i = i + 1 {\n\
         \x20       total = total + arr[i];\n\
         \x20   }\n\
         \x20   print(total);\n\
         }\n",
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let bin = scratch.join("prog");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(native_stdout(&run), "8\n"); // 3 + 5, stops before the 0
}
```

- [ ] **Step 2: Write a test proving `end` is evaluated exactly once, not per iteration**

```rust
#[test]
fn range_for_end_bound_is_evaluated_once_not_every_iteration() {
    // If `end` were re-evaluated every iteration, this loop would run
    // forever (or until some other limit) since `n` keeps growing inside
    // the body -- capping it here proves the compiler actually evaluated
    // `n`'s value once, at loop entry, matching this feature's documented
    // "start/end evaluated exactly once" contract.
    let scratch = scratch_dir("range_for_end_evaluated_once");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "fn main() {\n\
         \x20   let n = 3;\n\
         \x20   let count = 0;\n\
         \x20   for i from 0 to n {\n\
         \x20       n = n + 100;\n\
         \x20       count = count + 1;\n\
         \x20   }\n\
         \x20   print(count);\n\
         }\n",
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let bin = scratch.join("prog");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    assert_eq!(native_stdout(&run), "3\n");
}
```

- [ ] **Step 3: Write a test proving a range-for's start/end can be arbitrary expressions, not just literals**

```rust
#[test]
fn range_for_bounds_can_be_arbitrary_expressions() {
    let scratch = scratch_dir("range_for_expr_bounds");
    let src_path = scratch.join("prog.kes");
    fs::write(
        &src_path,
        "fn main() {\n\
         \x20   let a = 2;\n\
         \x20   let b = 7;\n\
         \x20   let total = 0;\n\
         \x20   for i from a + 1 to b - 1 {\n\
         \x20       total = total + i;\n\
         \x20   }\n\
         \x20   print(total);\n\
         }\n",
    )
    .unwrap();

    let out = Command::new(kestrelc_bin())
        .arg(&src_path)
        .current_dir(&scratch)
        .output()
        .expect("failed to run kestrelc");
    assert!(out.status.success(), "compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let bin = scratch.join("prog");
    let run = Command::new(&bin).output().expect("failed to run compiled binary");
    // for i from 3 to 6: 3+4+5 = 12
    assert_eq!(native_stdout(&run), "12\n");
}
```

- [ ] **Step 4: Run all new tests**

Run: `cargo test --release general_for_supports_a_data_dependent_early_style_condition range_for_end_bound_is_evaluated_once_not_every_iteration range_for_bounds_can_be_arbitrary_expressions`
Expected: PASS, all three.

- [ ] **Step 5: Run the full test suite one final time**

Run: `cargo test --release 2>&1 | tail -20`
Expected: full suite green (aside from the known pre-existing flaky test noted in Task 7 Step 5).

- [ ] **Step 6: Commit**

```bash
git add kestrelc/tests/integration.rs
git commit -m "Add end-to-end integration tests for for-loop syntax"
```

---

## Final: whole-branch verification

- [ ] Build the release binary and manually test both loop forms against `kestrelc-devtool` (the local dev server built earlier this project): `cargo build --release`, run `cargo run --release` from `kestrelc-devtool/`, paste in a range-for and a general-for program from Tasks 4/8 above, confirm both run correctly through the devtool's JIT/AOT auto-selection path.
- [ ] Confirm `benchmarks/run.sh` still passes (no regression to any existing workload) — none of the 5 benchmark `.kes` files use the new syntax, so this is a pure regression check, not a new-feature check.
