# Type System Foundation — Design

## Context

Kestrel's future optimization architecture is meant to look like:

```
Source → Type System → Typed IR → Purity/Effect Analysis
       → Bounds/Alias Analysis → Proof Information → Optimization → Cranelift
```

Before building more of that stack, the type system needs review on two
separate axes: (1) is the compiler's *internal representation* of types
memory/perf-efficient, and (2) is the *language-level* type checker
complete and consistent enough, and designed so it can grow toward the
future guarantee-type work (`NonZero`, `Range<i64,0,100>`,
`Array<T,N>`, etc.) without a rewrite.

Both were investigated by reading the actual implementation
(`kestrelc/src/typecheck.rs`, `ast.rs`, `interner.rs`) before proposing
anything — no speculative redesign of parts that are already fine.

## A. Internal representation — audit result: already efficient

- `Kind` (the type checker's inferred-type enum) is `#[derive(Clone, Copy,
  PartialEq, Eq)]`. Five zero-sized variants (`Int`, `Bool`, `Array`,
  `Str`, `Unknown`) plus `Struct(Symbol)` (one 4-byte payload). Total
  size ~8 bytes. Equality is a register compare. `.clone()`/`.copied()`
  never allocates.
- `Type` (the declared-type AST node: `Named(Symbol)` /
  `Array{elem: Box<Type>, size: Symbol}`) is `Clone` but not `Copy`
  (the `Box` blocks that). Cloning `Named` is a plain copy; cloning
  `Array` heap-allocates once. Arrays can't nest (struct fields must be
  scalar), so this recursion is shallow (depth ≤ 2) in practice — not a
  hot spot.
- The interner (`interner.rs`) already has its "Tier 0" optimizations
  shipped: `intern()` hashes once (FNV-1a, not SipHash) instead of
  hashing on both lookup and insert; well-known identifiers (`main`,
  `parallel_map`, `map`) are pre-interned as thread-local `Symbol`
  constants and compared directly (`Symbol == Symbol`), not via
  `.resolve()` string comparison.
- Struct field types are never duplicated — always looked up from the
  canonical `StructDecl` via `struct_table`, never re-stored per call
  site.

**One small leftover inefficiency**, same pattern as the well-known
identifiers but not yet applied: `typecheck.rs`'s `type_to_kind`
(lines 73-88) still resolves `bool`/`str`/`string` type names to a
`&str` and string-matches them, instead of comparing against
pre-interned `Symbol` constants the way `main`/`parallel_map` already
do. Trivial fix, same mechanism, already proven elsewhere in the file.

**Separately, not part of type representation:** `cse.rs`, `fusion.rs`,
and `inline.rs` each deep-clone the *entire* `Program` once per
optimizer pass per compile, even when the pass only rewrites a handful
of call sites. This is real allocation churn on the `watch` JIT hot
path. Flagged for visibility; out of scope for this doc (it's AST-clone
cost in the optimizer, not type representation) — worth its own pass
later if the JIT recompile latency budget needs it.

## B. Completeness gaps — real, found by reading `typecheck.rs` end to end

1. **`ExprKind::Field` always infers `Kind::Unknown`**
   (`typecheck.rs:269-279`). `p.x` used in an arithmetic op, passed as a
   function argument, or used as an `if`/`while` condition gets *zero*
   type checking today — even though the field's declared type is
   already looked up and checked for `StructLit` and `FieldAssign`.
   Struct field *writes* are fully checked; struct field *reads* are
   completely untyped. This is the largest gap found.
2. **`Kind::Array` carries no element type.** `type_to_kind` maps every
   array to a bare `Kind::Array`; `ExprKind::Index`'s inferred result is
   hardcoded to `Kind::Int` with the comment `// Kestrel arrays are
   integer-valued so far`. True only because no other element type
   exists yet in the language — nothing actually tracks it, so it will
   silently be wrong the moment array-of-bool or array-of-struct is
   ever added.
3. **`Let`/`Assign` inference is sticky-`Unknown`.** `locals.entry(name)
   .or_insert(k)` means a local's first-seen kind, if `Unknown`, stays
   `Unknown` for the rest of the function even if later information
   would resolve it. Minor precision gap, not a correctness bug (never
   causes a false rejection, just a missed check).

## C. Extensibility mechanism for future guarantee types

Do **not** grow `Kind` itself into a large lattice with one variant per
future guarantee (`NonZero`, `Range<min,max>`, `FixedLen(n)`, ...).
That would make `Kind` bigger, slower to compare, and would force every
existing `match k { ... }` site to grow a wildcard or an explicit arm
for each new guarantee — the same "every new variant touches N
exhaustive matches" pain already observed repeatedly this project with
`Stmt` variants.

Instead: keep `Kind` exactly as it is (base type identity only — same
size, same `Copy`, same cheap equality), and add a **separate, optional
refinement** wherever a `Kind` is currently stored (locals maps,
inferred expression results):

```rust
enum Refinement {
    NonZero,
    Range { min: i64, max: i64 },
    FixedLen(u64),
}
```

stored as `Option<Refinement>` alongside the existing `Kind`. This is
purely additive:
- `None` by default — same "never guess, never reject a valid program"
  posture the checker already has for `Kind::Unknown` everywhere.
- Existing checks (`is_numeric`, `is_boolean`, equality-of-`Kind`
  comparisons) don't change at all — they only ever look at `Kind`.
- A future bounds-check-elision pass reads `Refinement::Range` /
  `FixedLen` where present and falls back to a runtime check where
  absent — no new architecture, an additive lookup.

**Prerequisite, worth doing now regardless of whether the guarantee
types ever get built:** fix gap B1 (Field kind) and B2 (Array element
kind — change `Kind::Array` to `Kind::Array(Box<Kind>)`). Both are real
completeness wins on their own (catch real bugs today: a struct field
used as the wrong type, a hypothetical future array-of-bool misuse) and
are also the exact prerequisite `Array<T,N>` would need later — same
order is correct either way.

## D. Ranked future guarantee types (staged — not a commitment to build all)

1. **`Array<T,N>` (fixed-length arrays)** — cheapest of the three once
   B2 lands (reuses its element-kind tracking), and highest immediate
   payoff: directly targets the 16% bounds-check overhead the
   `bounds-heavy` benchmark already measured for loop-indexed array
   access.
2. **`NonZero`** — narrower usefulness (mainly div-by-zero-check
   elision), moderate effort — needs a small fact-propagation step
   (e.g. "this value came from `x + 1` where `x >= 0`"), not just an
   annotation.
3. **`Range<i64, min, max>`** — highest payoff (loop-bound proofs,
   vectorization enablement, the biggest remaining piece of the
   `where`-clause proof system's gaps) but also the highest effort and
   risk — real range-propagation through arithmetic. Same risk class
   already flagged in the prior optimization-brainstorm plan
   (`docs/superpowers/plans/shimmering-churning-hickey.md`, item #3): a
   bug in a soundness proof is a memory-safety bug, not just a missed
   optimization.

## E. Typed IR — explicitly out of scope for this doc

The architecture diagram's "Typed IR" stage is not designed here.
Today's checker and codegen both walk the AST directly; nothing in
sections B/C forecloses introducing a separate typed IR later — it's
a large, independent architectural change (new data structure, every
pass rewritten to consume it) that deserves its own dedicated
brainstorm once the type system itself is solid, not bundled into this
pass.

## Immediate next steps (the only part actually planned for implementation now)

1. Fix the `bool`/`str`/`string` well-known-Symbol gap (section A).
2. Fix gap B1: give `ExprKind::Field` a real inferred `Kind`, looked up
   from the struct's declared field type (mirrors the lookup
   `StructLit`/`FieldAssign` already do).
3. Fix gap B2: `Kind::Array` → `Kind::Array(Box<Kind>)`, threading
   element-kind through `ArrayLit`/`Index`/`type_to_kind`.

Sections C/D (the `Refinement` mechanism and the guarantee types
themselves) are **not** tasked yet — future work, staged per section D,
picked up only when actually pursued.
