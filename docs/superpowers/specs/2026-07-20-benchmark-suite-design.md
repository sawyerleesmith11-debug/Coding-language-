# Kestrel-vs-C benchmark suite — design

## Status

Approved scope, not yet implemented.

## Problem

The project's stated goal is for Kestrel to be "the fastest language," and
to be strong on parallelism specifically. There is currently zero data on
how kestrelc's actual compiled output performs against C on real
workloads. Before deciding whether to invest in a second codegen backend
(LLVM, to replace or supplement Cranelift), the project needs real
numbers, not a guess.

This also settled a scoping question along the way: "fast" is two
unrelated things —
1. **Runtime execution speed** (how fast compiled Kestrel code actually
   runs) — what this benchmark suite measures, and what "fastest
   language" competitively means.
2. **Dev-loop / iteration speed** (`kestrelc watch`'s ~30-100ms
   recompile-and-rerun cycle) — a tooling concern, unrelated to language
   speed, already addressed separately (cache.rs's linked-binary cache;
   a future `cranelift-jit`-based watch mode is a distinct, later
   project). This suite has nothing to do with #2.

## Scope

A `benchmarks/` directory in the repo: one workload per subdirectory, each
with a `.kes` source and an equivalent `.c` source, a runner script that
compiles and times both, and a committed results table. Rerunnable, so it
doubles as a regression suite after any future codegen change.

## Workloads

Five, each isolating one dimension:

1. **integer-loop** — tight scalar loop, modular arithmetic (same shape
   as `examples/bench_loop.kes`). Tests raw codegen/register-allocation
   quality, Cranelift's actual design target.
2. **array-sum** — transform/reduce over a large `i64` array. Tests
   memory throughput and autovectorization — Cranelift's known weak
   spot relative to LLVM.
3. **fib-recursive** — naive `fib(38)`. Tests call overhead and
   inlining, and exercises kestrelc's existing profile-guided inliner
   (`inline.rs`).
4. **parallel-map** — an expensive `pure fn` mapped over a large array via
   `parallel_map`. The actual differentiator: Kestrel can legally beat
   *single-threaded* C here because purity lets the compiler
   auto-parallelize without the programmer writing any threading code —
   this isn't a codegen-quality question, it's a language-semantics one.
5. **bounds-heavy** — an array loop with a `where` clause proving every
   access safe, vs. C's raw unchecked access. Expected to be near-parity
   (a sanity check that bounds-check elimination is actually working, not
   a discovery).

## Baselines

- **C `-O2`** — a fair, typical-real-world-code fight.
- **C `-O3 -march=native`** — the ceiling. `-march=native` lets the
  compiler autovectorize using the host CPU's full instruction set.
- **No hand-written assembly.** Considered and rejected: without real
  profiling tools (perf/VTune) to iterate against, hand-asm written here
  would be closer to a guess than an expert result, and could easily
  land *below* `-O3` — which would corrupt the ceiling number instead of
  clarifying it. `-O3 -march=native` is a low-risk, honest ceiling
  instead.
- **No Rust baseline.** Rust `--release` goes through LLVM, same as C
  `-O3` — it would very likely land within noise of the C ceiling and
  isn't worth the added toolchain dependency for this first pass.

## Method

- Each workload is sized to run multi-second, so the ~30ms Windows
  process-spawn floor (measured directly: a bare `cc`-compiled C
  hello-world takes ~30ms warm, ~85ms cold) is under 1% noise — this
  suite measures execution time, not process-launch overhead.
- Kestrel's profile-guided inliner needs a warm-up run before it reflects
  steady state (the *first* run of any program is always unoptimized —
  this is a known, deliberate tradeoff, not a bug). Each Kestrel workload
  is run twice to warm the profile, then measured on the third run.
- Median of 5 timed runs per workload/baseline combination.
- **Outputs are verified to match** across Kestrel and both C variants for
  every workload — a fast wrong answer is disqualified, not counted.
- Results reported as a table: workload × {Kestrel, C `-O2`, C `-O3`}
  wall-clock time, plus the ratios `Kestrel ÷ C-O2` and `Kestrel ÷ C-O3`.

## What the results mean (decision tree, not yet executed)

- Small gap (roughly <20-30%) on integer-loop/fib-recursive/bounds-heavy,
  wherever the gap shows up → Cranelift is good enough; don't touch the
  backend. Lean on the parallel-map result as the actual "beats C" story.
- Large gap specifically on array-sum → the vectorization weakness is
  real; the case for a second, LLVM-based codegen backend (parallel to
  `codegen.rs`/`wasm_codegen.rs`, same architectural shape, not a
  rewrite) becomes concrete and scoped.
- Large gap everywhere, not just array-sum → a bigger, separate
  investigation is needed before any backend decision; that finding
  alone would be worth surfacing on its own.

No backend commitment is made by this design — only the measurement.

## Explicitly out of scope

- Any codegen changes (this is measurement only).
- The dev-loop/JIT work (a separate, later project).
- Hand-written assembly baselines.
- A Rust baseline (redundant with C `-O3` per the reasoning above).
- Workloads beyond the five listed (string handling, allocation-heavy
  code, etc. are real future questions, not this pass).

## Testing plan

The suite itself *is* the deliverable — "testing" it means: confirm each
workload's Kestrel and C variants produce identical output, confirm the
runner script's timing methodology (multi-second workloads, warm-up runs,
median of 5) actually produces stable, low-variance numbers on a rerun
(two consecutive full runs of the suite should agree within a few
percent), and commit the first real results table to the repo.
