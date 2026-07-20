# Kestrel — Language Design Notes

*A working name. A kestrel is a small falcon that hovers motionless in
mid-air while scanning the ground — fast, precise, and economical with
energy. That's the goal for this language: no wasted motion.*

This document collects the design ideas discussed so far. It's a living
draft, not a spec — some of these ideas are standard practice (borrow
checking, AOT compilation), and some are original combinations/extensions
that haven't been tried together in one language. Both are marked below.

---

## 1. Persistent cross-run optimization cache
**Status: novel combination**

Most languages force a choice:
- **AOT (ahead-of-time) compilation** — compile once, run fast, but the
  compiler has to guess how the program will actually be used.
- **JIT (just-in-time) compilation** — watch the program run and optimize
  hot paths on the fly, but re-learn everything from scratch every time
  the program starts (JVM/V8-style "warmup").

**Kestrel's approach:** after every run, the runtime writes a small
profile file next to the binary — which functions were hot, what shapes
of data they saw, which branches were taken. The next run reads that
file before executing a single instruction and starts pre-specialized.
Over repeated runs on the same machine, the program keeps getting faster
and plateaus at what a JIT would eventually reach, but without ever
paying the warmup cost more than once per machine.

Trade-off to be honest about: this only helps for programs run
repeatedly on the same machine (servers, CLIs, dev loops) — a one-shot
script gets no benefit.

## 2. Effect-tracked purity
**Status: extension of known ideas (Haskell's purity + Rust's ownership)**

Rust's borrow checker proves memory-safety at compile time so there's no
need for a garbage collector. Kestrel extends the same idea to a
function's *effects*: does it allocate, do I/O, or mutate something
another part of the program can see?

A function marked `pure` is checked by the compiler to guarantee none of
that happens. Once proven pure, the compiler is free to:
- run it early or speculatively
- run multiple calls in parallel with zero risk of data races
- cache ("memoize") results automatically
- reorder it relative to other code

This is Haskell's purity guarantee, but layered on top of Rust-style
ownership instead of replacing it — you get *both* deterministic memory
management *and* algebraic freedom for the optimizer.

## 3. Layout polymorphism
**Status: novel extension of data-oriented design**

CPUs read contiguous memory much faster than scattered memory. Game
engines exploit this with "struct-of-arrays" layouts instead of the more
natural "array-of-structs," but it's tedious and easy to get wrong by
hand.

**Kestrel's approach:** the programmer writes normal, "array of structs"
-looking code. The compiler tracks how each field is actually accessed
across the hot paths of the program and is free to silently pick a
different physical memory layout per call site — recompiling a function
against a struct-of-arrays layout if that's what the access pattern
calls for. The logical shape of your data and its physical layout become
fully decoupled.

## 4. Proof-carrying optimization
**Status: extension of known ideas (dependent types / Idris / Agda)**

Even Rust keeps some runtime safety nets (e.g. array bounds checks)
because proving they're unnecessary in general is hard. Fully
dependently-typed languages let you write proofs inline that make
certain classes of bugs structurally impossible — but they're research
tools, not practical languages.

**Kestrel's approach:** a *lightweight* proof system — not full dependent
types — focused specifically on the checks that are expensive at
runtime: bounds checks, overflow checks, aliasing checks. Example:

```
fn get_safe(arr: [i32; N], i: usize) -> i32
    where i < N
{
    arr[i]   // compiler proves this in bounds at every call site,
             // so no runtime check is emitted at all
}
```

If the compiler can't prove the `where` clause at a call site, it's a
compile error, not a runtime check — you fix the call site, or fall back
to an explicitly-checked variant of the function.

## 5. Fearless parallelism, powered by purity

**Status: extension of known ideas (Rust's Rayon/fork-join, auto-vectorization)**

Most languages that let you spread work across CPU cores make you prove
it's safe by hand — audit the code for shared mutable state, add locks,
hope you didn't miss a case. Rust's borrow checker gets partway there at
compile time; most languages don't get there at all.

**Kestrel's approach:** this is mostly already paid for by idea #2. A
function the compiler has proven `pure` — no I/O, no mutation outside
its own locals — is, by that same proof, provably safe to run many times
over in parallel with zero risk of a data race, because there's nothing
shared for two calls to race over. So: calling a `pure` function once
per element of a large array or collection is a natural place for the
compiler to automatically split the work across available CPU cores,
no `unsafe`, no manual thread-safety audit, no opt-in required beyond
having written `pure` in the first place (which you'd want to do anyway,
for the reasons in idea #2).

Two further-out extensions of the same idea, roughly in order of how
soon they're realistic:
- **SIMD** — the same kind of straightforward numeric `pure` function
  applied across an array is also a natural candidate for doing the
  operation to several array elements in a single CPU instruction,
  not just spreading elements across cores.
- **GPU compute** — for the largest embarrassingly-parallel numeric
  workloads (physics, voxel/grid processing, and similar), eventually
  targeting a GPU backend for `pure` functions applied over big
  collections. This is a substantially bigger, more specialized
  undertaking than CPU-side parallelism (different execution model,
  real data-transfer overhead between CPU and GPU memory) and is a
  longer-term goal, not a near-term one.

**Trade-off to be honest about:** this only helps code that's both
`pure` and operating over genuinely independent chunks of work.
Sequentially-dependent code (naive recursive Fibonacci is the running
example throughout this doc) has nothing to split up and sees no
benefit. This also isn't a free unlock the moment the native backend
exists — it's real additional engineering (a work-stealing scheduler,
heuristics for when splitting the work is actually worth its own
overhead, eventually a GPU backend) that comes after the native
compiler, not alongside it.

---

## What's implemented so far (this prototype)

Two backends share the same front end (lexer, parser, purity checker,
bounds-proof notes) and are semantics-identical — every example program
produces the same output, and every error case throws the same
`KestrelError` message, on either one:

- **`Kestrel.run`** — a tree-walking interpreter directly over the AST.
  This is what let the language's *semantics* get tested and iterated on
  before investing in a faster backend.
- **`Kestrel.runFast`** — compiles each function to a flat bytecode
  instruction list first, then executes it on a stack-based VM where
  variables are array-index slots instead of name-keyed object
  properties (see `docs/SYNTAX.md` for how it's built).

**A third backend now exists:** `kestrelc/`, a real native compiler
using [Cranelift](https://cranelift.dev/) that emits an actual
standalone executable (via `cranelift-object` + the system linker) —
no VM, no interpreter loop at runtime at all. It's a separate Rust
program from `kestrel.js`, not something that runs in the browser
editor. It now supports arrays (literals, indexing, array-typed
parameters — always bounds-checked, never silently trusted) alongside
integers, functions/recursion, and control flow. See `kestrelc/README.md`
for the exact supported subset and the full benchmark methodology behind
the numbers below.

Both `run`/`runFast` support:
- variables, arithmetic, `if`/`else`, `while`
- functions, including `pure fn` with a real (if simplified) purity
  checker: a pure function is rejected at compile time if it calls an
  impure function, does I/O, or mutates anything outside its own locals
- fixed-size arrays with `where i < N`-style bounds proofs, checked
  statically where possible and falling back to a runtime check
  otherwise (with a warning), rather than silently trusting the
  programmer
- a `print` builtin

**Honest performance status:** `runFast` is not uniformly faster than
`run`. Measured with `node`, best-of-N, on this machine:

| Workload | `run` (tree-walk) | `runFast` (bytecode VM) |
|---|---|---|
| Tight loop, 20M iterations, arithmetic only | 5150 ms | 2600 ms (~**98% faster**) |
| Tight loop, 3M iterations, array indexing | 865 ms | 352 ms (~**146% faster**) |
| `fib(30)`, naive recursion (2.7M calls) | 384 ms | 420 ms (~**9% slower**) |

The loop/array wins are exactly the "array slots beat dictionary-mode
property lookups" argument this design doc has always made. The
recursion column used to be worse — an initial version of `runFast`
was ~28% *slower* than `run` on `fib(30)`, because every Kestrel
function call was a real recursive JavaScript call, and profiling
(`node --prof`) showed that call/return bookkeeping as the dominant
cost, worse than any single instruction. The fix: `execute()` no longer
recurses in JS at all. It's one flat loop over a shared stack; a Kestrel
call/return just swaps which function's code/base/instruction-pointer
the loop is currently reading, saving/restoring the caller's own on a
hand-managed call stack (three parallel arrays + an index, not an array
of objects — allocating one object per call was exactly the mistake
being fixed). That closed most of the gap (28% slower → 9% slower) but
not all of it; the remaining cost is believed to be inherent to still
using a real (if now shallow) JS function-call boundary for `execute`
itself plus the per-call stack bookkeeping, and would need either a
JIT-style specialization for hot call sites or inlining small functions
at compile time to close fully.

For scale, the same workloads in Rust (`rustc -O`) and C++ (`g++ -O2`)
run in low single-digit milliseconds each — roughly 100-800x faster
than either Kestrel JS backend, depending on the workload. That gap is
expected, not a sign of an unusually slow implementation: `run`/`runFast`
are interpreters running *on top of* JavaScript, while Rust/C++ compile
directly to native instructions with no interpreter loop underneath at
all.

**`kestrelc` closes almost all of that gap, on its very first working
version, with zero custom optimizations beyond Cranelift's own defaults**
(no inlining, no bounds-check elimination, no persistent cache — none of
the ideas above are wired in yet). Measured the same way as the Rust/C++
numbers above (process-external, best-of-N, this machine):

| Workload | `run` | `runFast` | **`kestrelc` (native)** | Rust | C++ |
|---|---|---|---|---|---|
| `fib(30)`, naive recursion | 384 ms | 420 ms | **8.5 ms** | 2.9-3.4 ms | 1.65-1.7 ms |
| Arithmetic loop, 20M iterations | 5150 ms | 2600 ms | **15.0 ms** | ~8 ms | ~14.5 ms |

That's **kestrelc beating `runFast` by roughly 45-175x**, and landing
within **roughly 2-5x of hand-written Rust/C++** — on the loop workload,
it's already within the same margin as the C++ number. See
`kestrelc/README.md` for exactly how these were measured (external
process timing was used for every column so the comparison is apples to
apples; an in-process/compute-only estimate is also given there and is
even closer).

The honest caveats: `kestrelc` only supports integers, `if`/`while`,
functions/recursion, and `print` so far — no arrays, no strings as
general values, no bounds proofs enforced yet (see `kestrelc/README.md`
for the exact scope and why). And the *interesting* ideas in this
document — the persistent cache, proof-carrying bounds elimination,
layout polymorphism — aren't implemented in `kestrelc` at all yet; this
is "compile straight to machine code with a mature, off-the-shelf
optimizing backend," not yet "compile *smarter* than a normal compiler
would." That's the honest ceiling of what's measured here, and also
exactly where the next work goes.

Not yet implemented (future work, roughly in priority order):
1. Proof-based bounds-check *elision* in `kestrelc` — **the design
   doc's own `get_safe` example now works exactly as originally
   specified**: `where i < N` is proven at every call site (a literal
   index against a literal-length array), an unprovable call site is a
   *compile error* per the doc's own stated rule ("if the compiler can't
   prove the where clause... it's a compile error, not a runtime
   check"), and the check inside the function body is fully elided —
   zero runtime cost, not just a faster check. Still narrow: the prover
   only handles a literal index and a literal-length array argument at
   the call site (not, say, an index derived from another proven-safe
   variable) — see `kestrelc/README.md`. Also still missing: a friendlier
   failure than a bare trap on the runtime-check fallback for genuinely
   dynamic accesses.
2. **Getting `kestrelc` runnable in the browser editor.** `kestrelc`
   now has a real WASM backend (`--wasm`) — a separate code path from
   the native one, since Cranelift only targets real CPUs — verified
   end to end (compiles `fibonacci.kes`, runs correctly in Node's
   `WebAssembly` API, output identical to the other two backends) and
   fast: `fib(30)` runs in ~6.5ms, in the same ballpark as native and
   ~64x faster than `runFast`. What's still missing: `kestrelc` itself
   (the compiler) is a native program — to actually use this from
   `kestrel-editor.html`, the compiler's front end needs to *also* be
   built as a WASM module, so the browser can compile Kestrel source
   to `.wasm` client-side. That's the next concrete step.
3. Closing the remaining ~9% call-overhead gap in `runFast` on
   recursion-heavy code — lower priority now that `kestrelc` exists and
   already dwarfs any remaining VM-tuning gains
4. The persistent cross-run optimization cache, built on top of `kestrelc`
5. Layout polymorphism
6. A more general proof system beyond simple bounds checks
7. CPU-side parallelism for `pure` functions over collections (idea #5)
   — now unblocked, since `kestrelc` generates real machine code
8. SIMD, then (much further out) a GPU backend — both extensions of (7)

## Naming

"Kestrel" is a placeholder. Happy to rename — the interpreter and file
extension (`.kes`) can change with a find-and-replace once a name is
picked.
