# Structs (records) for Kestrel — design

## Status

Approved scope, not yet implemented. This is step 1 toward
`kestrel-DESIGN.md`'s idea #3 ("layout polymorphism") — structs
existing at all is the prerequisite that idea is blocked on.

## Scope decision: `kestrelc` only

`kestrel.js` (`run()`/`runFast()`) is frozen as of this design — no new
language features are ported there going forward. It stays in the repo,
unmaintained, rather than being deleted; it can be revived or removed
later if it's clearly dead weight. Reasoning: `kestrelc --wasm` already
covers the "runs in the browser/mobile" use case with better
performance and a real compiled artifact, making the JS interpreter
backends mostly redundant for that purpose. Every feature this session
was ported to three places (two JS backends + kestrelc); freezing two
of them removes two-thirds of the "fix it three times" tax on every
future change.

This design is `kestrelc`-only: both the native backend (`codegen.rs`)
and the WASM backend (`wasm_codegen.rs`), since both are part of
`kestrelc`.

## Syntax

```
struct Point {
    x: i64,
    y: i64,
}

fn main() {
    let p = Point { x: 1, y: 2 };
    print(p.x, p.y);
}
```

- A struct is declared at the top level, alongside `fn` declarations.
- Construction uses a named-field literal (`Point { x: 1, y: 2 }`), not
  positional — field order in the literal doesn't need to match
  declaration order.
- Field access is `.field` (new postfix operator).
- Struct names are used in type position exactly like any other type
  name today (`Type::Named(Symbol)` already covers this — no new type
  grammar needed, just new semantic meaning when the name resolves to a
  declared struct instead of a built-in scalar type).

## Explicit scope limits (v1)

- **Immutable only.** No field assignment (`p.x = 5`) — construct once,
  read fields. Matches how arrays already behave in Kestrel (index-read
  only, no `arr[i] = x` either). Mutable fields are real future work,
  not attempted here.
- **Scalar fields only.** No array-typed fields, no nested structs. This
  lets a struct value flatten to N plain `i64` values everywhere (locals,
  function parameters, call arguments) — no pointer/memory involved at
  all, unlike arrays. Nested structs and array fields are a clean,
  separate follow-up once this works.
- **No struct-returning functions.** `kestrelc`'s calling convention
  currently assumes a single `i64` return value throughout codegen
  (recursive calls, the memoization epilogue, etc.). Supporting a
  multi-value return is a real ABI expansion, deferred rather than
  bundled into this design. A function may take a struct *parameter*
  and read its fields, but may not construct-and-return one.
- **Not eligible for memoization or `parallel_map` in v1.** Same "not
  yet, not an error" posture used throughout this session's other
  scoping decisions — a function with a struct parameter simply isn't
  memo-eligible yet, and a struct can't be a `parallel_map` array
  element type yet.
- **One documented parser edge case, not fixed:** a `where` clause is
  not parenthesized, so `fn f(x: i64) where SomeStructName { ... }`
  could misparse (attempting to read the function body as a
  struct-literal field list) if a `where` clause bare-referenced a name
  that happens to collide with a declared struct name. This is
  vanishingly unlikely in practice (the program wouldn't type-check
  either way — a struct isn't a boolean), and the failure mode is a
  confusing parse error, not a miscompile. Not worth the parser
  complexity of avoiding it in v1.

## Architecture

### AST (`ast.rs`)

- `Program` stops being a bare `Vec<Fn>`. Becomes:
  ```rust
  pub struct Program {
      pub fns: Vec<Fn>,
      pub structs: Vec<StructDecl>,
  }
  ```
- New declaration:
  ```rust
  pub struct StructDecl {
      pub name: Symbol,
      pub fields: Vec<Param>, // reuses the existing Param{name, ty} shape
      pub span: Span,
  }
  ```
- New `ExprKind` variants:
  ```rust
  StructLit { name: Symbol, fields: Vec<(Symbol, Expr)> },
  Field { target: Box<Expr>, field: Symbol },
  ```

### Parser (`parser.rs`)

- Top-level `parse_program` recognizes `struct` as a second item kind
  alongside `fn`, populating `Program.structs`.
- `parse_postfix` gains a `.` case: after parsing a primary/postfix
  expression, if the next token is `.` followed by an identifier,
  extend the chain with `ExprKind::Field`.
- `parse_primary`'s `Ident` case: if the identifier is immediately
  followed by `{`, parse a struct literal (`ExprKind::StructLit`)
  instead of a bare `ExprKind::Ident`. This is where the documented
  `where`-clause edge case above comes from.

### Every existing pass that iterates `program`

Every file currently doing `for f in program` (or `program.iter()`
expecting `&Fn`) needs a one-line update to `for f in &program.fns`:
`main.rs`, `lib.rs`, `purity.rs`, `typecheck.rs`, `resolve.rs`,
`fusion.rs`, `inline.rs`, `codegen.rs`, `wasm_codegen.rs`. This is
mechanical (same shape of change as tonight's `Expr` → `ExprKind`
refactor) but touches nearly every file in the crate — the single
biggest source of diff size in this feature, not the struct logic
itself.

### `resolve.rs`

New checks, genuinely useful (not just plumbing):
- Struct literal names a declared struct (`Unknown struct 'Foo'`).
- Struct literal's fields exactly match the declaration — no missing
  fields, no unknown fields (`Missing field 'y' in 'Point' literal`,
  `'Point' has no field 'z'`).
- `.field` access: target must resolve to a value of *some* struct
  type, and that struct must have the named field. This needs
  `resolve.rs` to track, per local, which struct type (if any) it holds
  — a small addition to the existing `HashSet<Symbol>` locals-tracking,
  becoming a `HashMap<Symbol, Option<Symbol>>` (local name → struct type
  name, if it's a struct-typed local) so field access can be validated.
- `resolve::build_fn_table`'s sibling: a new `build_struct_table(&Program) -> HashMap<Symbol, &StructDecl>`, built once and threaded through the same way `fns` already is.

### `purity.rs` / `typecheck.rs`

- `purity.rs`: `StructLit`/`Field` are pure operations (no I/O) — new
  match arms in the existing expression walker, same treatment as
  `ArrayLit`/`Index` already get. No new purity *rules*.
- `typecheck.rs`: new `Kind::Struct(Symbol)` variant. A struct literal
  infers `Kind::Struct(name)`; `.field` access on a non-`Kind::Struct`
  value, or a struct that doesn't have that field, is a type error. The
  field's own resulting value kind is `Kind::Unknown` after that — this
  matches the type checker's existing, already-documented limitation
  that declared types don't carry kind information yet (a function
  parameter's kind is `Unknown` inside its own body for the same
  reason).

### Codegen (`codegen.rs`, native)

- New `Slot::Struct(Vec<Variable>)` variant alongside today's
  `Slot::Scalar`/`Slot::Array` — one Cranelift `Variable` per field, in
  declaration order. Because fields are scalar-only, a struct value
  never needs stack memory or a pointer at all; it's purely N SSA
  values, simpler than arrays in that respect.
- Field access (`p.x`) is a direct `builder.use_var(...)` — no load
  instruction, no bounds check, nothing array-indexing needs.
- Struct construction (`Point { x: 1, y: 2 }`) evaluates each field
  expression (in *declaration* order, not literal-written order — needs
  a lookup into the struct's own field order) and defines each
  `Variable`.
- Passing a struct as a function argument or parameter: flattens to N
  consecutive `i64` ABI slots, exactly the same trick `fn_signature`
  already uses for an array parameter's (pointer, length) pair, just
  without the pointer.
- `Type::Array`/`Type::Named` handling in `fn_signature`,
  `collect_slots`, etc. gains a third case for a `Type::Named` that
  resolves to a declared struct name (vs. a built-in scalar name) — the
  struct table built during `compile_program` is what makes that
  distinction.

### Codegen (`wasm_codegen.rs`)

Same shape of change as native: `VarLoc::Struct(Vec<u32>)` (wasm local
indices instead of Cranelift `Variable`s), same flattening story for
parameters, same direct-local-read for field access.

## Testing plan

Same two-tier pattern as everything else this session:
- **Unit tests** per pass: `parser.rs` (struct decl + literal + field
  access parse correctly), `resolve.rs` (unknown struct, unknown field,
  missing field in literal all rejected with clear messages),
  `purity.rs`/`typecheck.rs` (struct operations don't break purity,
  field-access type errors caught).
- **Integration tests** (`tests/integration.rs`) through real compiled
  binaries: construct a struct, read its fields, pass it to a function,
  a `pure fn` reading struct fields, both native and `--wasm` variants
  of each — matching the existing `wasm_backend_*` pairing convention.

## Explicitly out of scope (future work, not this design)

- Mutable fields.
- Nested structs, array-typed fields.
- Struct-returning functions (multi-value return ABI support).
- Structs as `parallel_map` element types or memoization parameters.
- Layout polymorphism itself (`kestrel-DESIGN.md` idea #3) — this
  design is only the prerequisite, not the optimization.
- Any `kestrel.js` / web-editor changes (frozen per the scope decision
  above).
