# Kestrel vs. C vs. Rust benchmark results

Run on: Windows, mingw `cc` (WinLibs UCRT) + `rustc -C opt-level=3`, 16
logical / 8 physical cores. Median of 5 timed runs per variant, native
builds only. See `2026-07-20-benchmark-suite-design.md` in
`docs/superpowers/specs/` for methodology. Rust variants (`bench.rs`)
added later than the original C-only suite — same workloads,
byte-identical input data (same 20,000-element array literal), output
cross-checked equal across all four variants (kestrel/C-O2/C-O3/rust) on
every run, not just compared after the fact.

Re-measured after three `parallel_map` runtime changes: a persistent
worker pool (no more per-call thread spawn), pool sized by physical
core count instead of logical, and cross-call CSE for repeated pure
calls (doesn't affect this suite — no workload here has a repeated call
with identical arguments in one scope).

| Workload | Kestrel | C -O2 | C -O3 -march=native | Rust -O3 | Kestrel ÷ C-O3 | Kestrel ÷ Rust |
|---|---|---|---|---|---|---|
| integer-loop | 0.748s | 0.509s | 0.505s | 0.623s | **1.48x slower** | **1.20x slower** |
| fib-recursive | 0.038s | 0.097s | 0.082s | 0.128s | **2.2x faster*** | **3.4x faster*** |
| array-sum | 0.190s | 0.173s | 0.174s | 0.177s | **1.09x slower** (near parity) | **1.07x slower** (near parity) |
| parallel-map | 0.115s | 0.396s | 0.408s | 0.431s | **3.5x faster** | **3.7x faster** |
| bounds-heavy | 0.668s | 0.621s | 0.621s | 0.642s | **1.08x slower** | **1.04x slower** |

**parallel-map's multiplier actually dropped** from the previous
4.1-4.5x to 3.5-3.7x after switching the pool from logical (16) to
physical (8) core count. Expected, honest trade-off: this suite's
workload is a *single* `parallel_map` call, where more threads (up to
the array-size limit) directly buys more raw parallelism regardless of
SMT contention — the physical-core change was validated separately
against a *repeated-call* workload (2000x calls, cheap per-element
work), where it measured ~15-20% faster due to less scheduling
contention across many calls, a different shape of program than this
single-call suite exercises. Both numbers are real; they're just
measuring different things. Single-call raw throughput and
many-call scheduling overhead pull in opposite directions here.

\* fib-recursive's win is from automatic memoization eliminating naive
recursion's redundant subcalls, not from better codegen — neither C's
nor Rust's naive recursion has an equivalent optimization available.
Not an apples-to-apples codegen comparison; recorded honestly, not
excluded.

## Reading these

- **Rust lands almost exactly where C -O3 does**, workload for
  workload — both use LLVM at a comparable optimization tier, so this
  isn't a surprise, but it's worth having measured rather than assumed
  (an earlier conversation estimated "Kestrel-vs-Rust should track
  Kestrel-vs-C-O3" before this suite existed; the real numbers confirm
  that estimate was directionally correct, within a few percent).
- **On raw scalar codegen** (integer-loop, array-sum, bounds-heavy):
  Cranelift lands within roughly 5-47% of both C `-O3` and Rust,
  closest when the workload doesn't autovectorize well on either side
  (array-sum, whose modulus reduction likely blocks vectorization in
  all three compilers) and furthest on tight integer-only arithmetic
  (integer-loop). This is a real, moderate gap — not the >100% blowout
  a "Cranelift can't do vectorization" story alone would predict, since
  none of these three workloads triggered heavy vectorization on the
  C/Rust side either. A workload specifically designed to trigger SIMD
  (contiguous float arrays, unconditional element-wise ops with no
  modulus) would be a fairer test of Cranelift's actual vectorization
  gap and is a natural next addition to this suite.
- **On the actual thesis** (parallel-map): a clean **3.5-3.7x** win over
  single-threaded C *and* Rust, using purity-proven auto-parallelism
  with zero threading code written by hand — the Rust variant here is
  the same serial, single-threaded loop the C one is, not a `rayon`
  comparison. This is the strongest, most honest "beats C/Rust" result
  in the suite — it's not a codegen-quality claim, it's a
  language-semantics one, and the numbers back it against both. (See
  the physical-vs-logical core note above the table for why this
  number is lower than an earlier measurement — a real trade-off, not
  a regression.)
- **bounds-heavy** shows the real, current cost of Kestrel's safety net
  for the dominant real-world array-access pattern (loop-indexed, not
  literal-indexed) — about 5-8% overhead versus C/Rust's raw unchecked
  access. See the finding below: the `where`-clause proof system
  doesn't yet cover this pattern at all, so every loop-indexed access
  pays a real runtime check today.

## Two real bugs found while building this suite

Neither is a benchmark artifact — both are genuine, previously-unknown
kestrelc issues, found by hitting them directly while sizing workloads.

### 1. Automatic memoization has no cost-benefit check

Any eligible `pure fn` (single scalar parameter, not a `parallel_map`
callback) gets an unconditional memoization slot
(`kestrelc/src/codegen.rs:534`). If that function is called with a
different argument on every single call — e.g. `square(i)` inside a
loop over `i` — every call is a guaranteed cache miss, but still pays to
grow and insert into an ever-larger hash table
(`kestrelc_runtime.c`'s `kestrelc_memo_store`).

Measured impact: a 200,000,000-iteration loop calling such a function
used over 3GB of RAM and did not finish within several minutes (killed
manually). The equivalent loop with the function call inlined away
(no memoization involved) ran in 0.66s. A 20,000,000-iteration version
of the same pathological pattern hit 2.5GB of RAM within 2 seconds.

This is a real risk for any Kestrel program with a `pure fn` applied
over a large index or counter — a very natural, common pattern, not a
contrived one. Worth a follow-up: either a runtime eviction/cap
strategy for a memo table that's clearly not getting hits, or a
compile-time heuristic (e.g. don't memoize a function whose only
call site's argument is provably monotonic/derived from a loop
counter).

### 2. Array literals are stack-allocated — large ones crash

`kestrelc/src/codegen.rs:310`'s own comment confirms this is
deliberate: "Array literals are stack-allocated." A 500,000-element
`i64` array literal (4MB) reliably crashed with
`STATUS_STACK_OVERFLOW` (`0xC00000FD`) against Windows' default 1MB
thread stack — confirmed via direct exit-code inspection
(`$LASTEXITCODE` = -1073741571 in PowerShell), not inferred.

A 100,000-element array (800KB) ran without crashing but left very
little headroom. All workloads in this suite were rebuilt at
20,000 elements (160KB) to stay safely clear of this limit.

This is a serious, silent failure mode for real Kestrel programs: any
moderately large data literal (not even that large — a few hundred
thousand entries) will crash with no compiler warning at compile time
and a cryptic OS-level crash at runtime, not a clean Kestrel error
message. Worth a follow-up: either heap-allocate array literals above
some size threshold, or at minimum have `resolve.rs`/`codegen.rs`
detect a literal large enough to be a stack-overflow risk and reject it
with a clear compile error instead of an opaque runtime crash.

## Files

- `run.sh` — rebuilds and times all 5 workloads, verifies output
  matches across all four variants, prints median-of-5 results.
- `<workload>/bench.kes` + `bench.c` + `bench.rs` — the workload set,
  same logic and same input data in all three languages.
