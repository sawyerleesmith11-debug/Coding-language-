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
**Status: novel combination — first (scoped-down) step implemented**

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

**What's actually implemented is a smaller, real first step, not this
full vision:** `kestrelc` now has a persistent, on-disk, cross-invocation
*compile* cache (`kestrelc/src/cache.rs`) — if the exact same source text
has compiled successfully before, a later `kestrelc` invocation skips
lexing/parsing/purity-checking/codegen entirely and reuses the cached
artifact (object file for the native backend, `.wasm` bytes for the WASM
one), keyed by a content hash of the source. This is the boring, honest
80%: "don't redo work you've already done," not runtime branch/shape
profiling or speculative pre-specialization — no execution feedback loop
exists yet. `kestrel-editor.html`'s native engine has the same idea at a
smaller, session-only scale: an in-memory `Map` keyed by source text, so
clicking Run repeatedly on unchanged code skips recompilation without
needing a filesystem (the browser's `kestrelc-web` has none). Real,
measured win either way — e.g. a cached `kestrelc-web` compile in the
editor dropped from ~22ms to ~0.6ms — but the actual profile-guided
pre-specialization described above is still future work.

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

**Status: extension of known ideas (Rust's Rayon/fork-join, auto-vectorization) — first real version implemented**

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

**What's implemented:** `parallel_map(f, arr)` — a reserved builtin call
name (like `print`), not a new keyword, so it needed zero grammar
changes. `f` must be a `pure fn` taking exactly one scalar parameter;
misuse (non-pure, wrong arity, an array parameter, an unknown function,
or a non-identifier first argument) is a compile error in every
backend, not a runtime surprise. Every backend accepts the same
programs — `run`/`runFast` (JS, single-threaded) and the WASM backend
apply `f` sequentially, since neither has real threads available
(kestrel.js's are a correctness oracle: proof of the *right answer* to
check the real implementation against); **`kestrelc`'s native backend
is the one that actually parallelizes**, via a small C runtime shim
(`kestrelc/runtime/kestrelc_runtime.c`, linked into every native build)
that spins up real OS threads with `pthread_create`, one chunk of the
array per available CPU core, calling straight back into the
Cranelift-compiled `pure fn` from each thread. Below a size threshold
(currently 10,000 elements) or on a single-core machine, it runs inline
instead — thread setup/teardown would cost more than it saves, a real
instance of the "heuristics for when splitting is worth it" trade-off
named below, not a hypothetical one anymore.

Measured on this machine (4 logical CPUs), a CPU-heavy `pure fn` applied
to a 20,000-element array via `parallel_map`, external-process/best-of-N
timed against an equivalent hand-written sequential C loop doing the
identical work: **~2.1x faster**, consistently across both a light and
a 40x-heavier per-element workload. That's real speedup, honestly
below the ideal ~4x for 4 cores — some combination of thread
overhead, memory bandwidth, and this being a shared/virtualized
container rather than dedicated hardware, most likely; take the exact
multiplier as this-machine-specific, not a universal claim.

Current scope, honestly: the array being mapped over must be a
fixed-size array *literal* (`let x = [...]`), not a parameter — the
output array's size has to be known at compile time since it's a plain
stack allocation, same restriction the bounds-elision proof (idea #4)
already has for its literal-length fast path. See `kestrelc/README.md`
for the exact rules and error messages.

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
benefit. The current implementation is also still narrow — one thread
pool, one fixed size threshold, no work-stealing, only over an array
literal — real additional engineering beyond this first version, not a
finished scheduler.

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
no VM, no interpreter loop at runtime at all. It supports arrays
(literals, indexing, array-typed parameters — always bounds-checked,
never silently trusted) alongside integers, functions/recursion, and
control flow.

**And it now runs in the browser editor.** `kestrelc` also has a WASM
backend (a separate code path from the native one, since Cranelift only
targets real CPUs), and — the actual point of building that — `kestrelc`
itself is *also* compiled to WASM (`kestrelc-web/`), so
`kestrel-editor.html` can compile Kestrel source to a runnable `.wasm`
module entirely client-side: no server, no native binary, just the
"native" option in the editor's engine picker. Verified end to end in a
real headless-browser run (not just Node): correct output, correct
compile-error reporting, `fib(30)` compiling and running in
milliseconds. See `kestrelc-web/README.md` for the no-wasm-bindgen,
zero-JS-dependency interface this uses. See `kestrelc/README.md`
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

**Honest performance status:** `runFast` is now faster than `run` on
every workload measured, including recursion. Measured externally
(fresh `node` process per run, best-of-5), on this machine:

| Workload | `run` (tree-walk) | `runFast` (bytecode VM) |
|---|---|---|
| Tight loop, 20M iterations, arithmetic only | 5219 ms | 2977 ms (~**75% faster**) |
| Tight loop, 3M iterations, array indexing | 779 ms | 412 ms (~**89% faster**) |
| `fib(30)`, naive recursion (2.7M calls) | 426 ms | 268 ms (~**59% faster**) |

The loop/array wins are exactly the "array slots beat dictionary-mode
property lookups" argument this design doc has always made. The
recursion column has a longer history: an initial version of `runFast`
was ~28% *slower* than `run` on `fib(30)`, because every Kestrel
function call was a real recursive JavaScript call, and profiling
(`node --prof`) showed that call/return bookkeeping as the dominant
cost, worse than any single instruction. Rewriting `execute()` to not
recurse in JS at all — one flat loop over a shared stack, a Kestrel
call/return just swapping which function's code/base/instruction-pointer
the loop is currently reading, with a hand-managed call stack (three
parallel arrays + an index) — closed most of that gap (28% slower → 9%
slower) but left a real remainder, which for a while was believed to be
an inherent cost of the VM's own function-call boundary.

It wasn't. The actual cause: the VM used the *operand/locals stack's own
`.length`* as its stack pointer (`stack.push`/`stack.pop`/
`stack.length = ...`), so every single call and return — 2.7M of them,
for `fib(30)` — mutated a real JS array's length, which pushes V8 to redo
bookkeeping (capacity checks, possible reallocation) on the hottest path
in the interpreter. Decoupling the logical stack pointer from the
backing array's length — a plain `sp` integer, with the array
preallocated and only ever grown (via manual copy), never shrunk — turned
that per-call cost into an integer increment/decrement. Confirmed with
`node --prof` on a fresh (unwarmed) process, the realistic case for a
short-lived script: before the fix, cold `fib(30)` measured ~452 ms on
`runFast` vs. ~410 ms on `run` (`runFast` slower, matching the
previously-reported 9%); after, ~268 ms vs. ~426 ms — `runFast` now
**faster** on recursion too, not just at parity. All 61 JS tests still
pass; `run` and `runFast` remain semantics-identical, verified by the
equivalence tests, not just the timing.

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

The honest caveats: `kestrelc` (both the native and WASM backends,
including `kestrelc-web`) only supports integers, `if`/`while`,
functions/recursion, `print`, and arrays so far — no strings as general
values, and cross-function `where`-clause bounds elision is native-only
for now (see `kestrelc/README.md` for the exact scope and why). And most
of the *interesting* ideas in this document — layout polymorphism, a
more general proof system beyond array bounds — aren't implemented in
`kestrelc` at all yet (the persistent cache has a real first step now,
see idea #1 above, but not its full runtime-profile-guided vision); this
is still mostly "compile straight to machine code with a mature,
off-the-shelf optimizing backend," not yet "compile *smarter* than a
normal compiler would." That's the honest ceiling of what's measured
here, and also exactly where the next work goes.

**A real type checker now exists — a first, honestly-scoped version.**
Types were previously written but not checked at all (see
`docs/SYNTAX.md`'s Types section) — `i32`, `usize`, etc. were arbitrary
identifiers with no semantic enforcement, so `foo(true, "hello")`
compiled even if `foo` declared `(x: i32, y: i32)`. Rather than
inventing a full built-in type system in one step, `check_types`
(`kestrelc/src/typecheck.rs` and kestrel.js's `checkTypes`, wired into
every backend) infers each expression's value *kind* (integer vs.
boolean) purely from literals and operators — `true`/`false`/
comparisons/`&&`/`||`/`!` are boolean, everything else numeric — and
rejects mixing them (`5 + true`, `!5`, a literal number used directly
as an `if`/`while` condition), plus a plain function-call argument
*count* mismatch. Does **not** yet check declared parameter type
*names* against call-site arguments (`foo(x: i32)` called as
`foo(some_bool)` isn't caught yet) — that needs a real decision about
what Kestrel's built-in types actually are first, a bigger design step
than this. See `docs/SYNTAX.md`'s "Type checking" section for the exact
rules and worked examples.

**Compile error locations now include line, column, and a length — a
first, honestly-scoped version.** Both front ends (`kestrel.js`'s
lexer/parser and `kestrelc`'s) tracked only a line number before; now
every token carries its starting column and character length, and a
shared formatter (`formatKestrelError` in `kestrel.js`,
`format_diagnostic` in `kestrelc/src/lib.rs`) renders `file:line:col:
message` followed by the offending source line and a `^` span
underneath it — exactly the `filename:14:7` example this item used to
describe as future work. `kestrelc`'s CLI and `kestrelc-web` (and so
`kestrel-editor.html`'s "native (wasm)" engine) use it for real; the
`run`/`runFast` engines in the editor use it too when a `KestrelError`
reaches the Run button's error handler. Purity-check and type-check
errors (both JS backends) now get the full `file:line:col:` + caret
treatment too, not just lex/parse errors — `checkPurity` and
`checkTypes` return `{message, line, col, len}` objects (instead of
plain strings) pinned to the statement that triggered them, and
`run`/`runFast` render each one through `formatKestrelError` before
throwing, exactly like a lex/parse error. **Scope, honestly:** the
position is still statement-granularity, not full per-expression — `let
x = f(a) + g(b);` points the caret at the start of the `let`, not at
whichever of `f(a)`/`g(b)` was actually the problem, since that needs a
span on every expression node, not just statements, which is still
real future work. `kestrelc` (native, via Cranelift) doesn't have any
of this — its own `purity.rs`/`typecheck.rs` still report
message-only errors; runtime errors (unknown identifier,
out-of-bounds index, etc.) in every backend are also still
message-only, since they're not part of the AST-walking checkers this
change touched.

**Memoization, shipped (JS backends only):** both `run` and `runFast`
now cache a `pure fn`'s result by argument value, scoped to a single
`run`/`runFast` call — a repeated call with identical arguments returns
the cached value instead of re-executing. This is always safe per the
idea #2/#4 purity proof: a `pure fn` can't observe or be affected by
any other call to itself, so caching changes nothing observable. Cache
key is a canonicalized `JSON.stringify` of the argument list; the one
correctness wrinkle was that `JSON.stringify(NaN) === JSON.stringify(null)`,
and Kestrel can produce a real runtime `null` via a bare `return;` even
with a declared return type — so `NaN` is swapped for a sentinel string
before stringifying to keep those cases from colliding on the same key.
**Scope, honestly:** `kestrelc` (native, via Cranelift) does not memoize
at all yet — this is JS-backends-first, matching the project's usual
pattern of new semantics landing there before the native compiler.

**Loop fusion, shipped (JS backends only, narrow shape):** both `run`
and `runFast` now fuse a chain of `let a = parallel_map(f, arr); let b
= parallel_map(g, a);` — with `a` used nowhere else in the function —
into one `parallel_map` over `arr` with a synthesized pure fn computing
`g(f(x))`, one pass and no intermediate array instead of two. Runs as
an AST-to-AST pass (`fuseLoops`) after purity/type/parallel_map checks
pass on the original program, so both backends share one
implementation instead of duplicating the optimization. Safe by
construction: `f` and `g` are already proven pure, and the synthesized
function is a trivial composition of two already-pure functions, not a
new proof. Chains fuse transitively (a 3-deep chain collapses to one
function), and it also fires inside `if`/`while` bodies, not just at
top level. **Scope, honestly:** deliberately narrow — only this exact
adjacent-`let` shape triggers it. A chain split across other
statements, an intermediate array referenced more than once, or a
source that isn't a bare `parallel_map` call are all left unfused
rather than guessed at. `kestrelc` does not fuse anything (it doesn't
even have `parallel_map` chains as a concept yet at the IR level).
General loop fusion beyond `parallel_map` chains specifically (e.g. a
plain `while`-loop calling multiple pure fns per iteration) is still
unaddressed.

Not yet implemented (future work, roughly in priority order):
1. Full per-expression position tracking — purity/type errors now get a
   real `file:line:col:` + caret (see above), but pinned to the
   *statement*, not the exact sub-expression; going finer needs a span
   on every AST node, not just statements. Also: bringing any of this
   to `kestrelc`'s own checkers, and to runtime errors in every backend.
2. Bringing memoization and `parallel_map`-chain fusion (both shipped
   in the JS backends, see above) to `kestrelc`. Also: generalizing
   fusion beyond the current narrow adjacent-`let` shape.
3. Proof-based bounds-check *elision* in `kestrelc` — **the design
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
   dynamic accesses, and this specific cross-function elision fast path
   is native-only so far — the WASM backend (`kestrelc --wasm` /
   `kestrelc-web`) has array support too, but still runtime-checks
   `where`-guarded accesses rather than eliding them.
4. The full runtime-profile-guided version of the persistent cache (idea
   #1) — the on-disk/in-memory *compile-result* cache is done; branch/
   shape profiling and pre-specialization from it are not
5. Layout polymorphism — blocked on structs/records existing at all
   (Kestrel doesn't have them yet — see `docs/SYNTAX.md`), a
   prerequisite bigger than the layout-choice optimization itself
6. A more general proof system beyond simple bounds checks
7. SIMD, then (much further out) a GPU backend — both extensions of
   idea #5's CPU-parallelism work, which now has a first real version
   (`parallel_map`, native backend only — see idea #5 above); a
   general-purpose work-stealing scheduler beyond the current one
   thread-pool/one-threshold implementation is also still open

(`runFast`'s recursion overhead, formerly listed here, is resolved — see
the benchmark table above.)

## Naming

"Kestrel" is a placeholder. Happy to rename — the interpreter and file
extension (`.kes`) can change with a find-and-replace once a name is
picked.
