# kestrelc

A native compiler for Kestrel, using [Cranelift](https://cranelift.dev/)
to emit a real standalone executable — no VM, no interpreter loop at
runtime at all. This is a separate Rust program from `kestrel.js`; it
doesn't run in the browser editor.

**Status: working, for a deliberately scoped subset of the language.**
It compiles and correctly runs real Kestrel programs (verified against
`Kestrel.run`/`Kestrel.runFast`'s output — see `tests/integration.rs`),
and its first, unoptimized numbers are already within single-digit
multiples of hand-written Rust/C++. See "Scope" and "Benchmarks" below
for the honest details.

## Building and running

```sh
cd kestrelc
cargo build --release
./target/release/kestrelc ../examples/fibonacci.kes
./fibonacci
```

There's also a WASM backend, a completely separate code path from the
native one below (Cranelift's codegen only targets real CPUs, not WASM —
this uses `wasm-encoder` to build a `.wasm` module directly):

```sh
./target/release/kestrelc --wasm ../examples/fibonacci.kes   # writes fibonacci.wasm
```

The resulting `.wasm` runs in any WASM host (a browser, Node's
`WebAssembly` API) that supplies two host-import functions the module
calls for output, since WASM has no I/O of its own — see
`tests/integration.rs`'s wasm tests for a minimal working host
(`env.print_i64(value, is_last)` and `env.print_str(ptr, len, is_last)`).
**This is now wired into `kestrel-editor.html`** — pick "engine: native
(wasm)" in the editor. `kestrelc` itself is also compiled to WASM
(`kestrelc-web/`), so the whole pipeline (compile Kestrel source to a
`.wasm` module, then run it) happens client-side, no server involved.
See `kestrelc-web/README.md`. **Arrays are supported**, same as the
native backend, including both proof-carrying bounds-elision fast paths
— literal indices into literal-length arrays, and cross-function
`where`-clause elision (see "Scope" below) — both backends run the
exact same `WhereInfo` proof (`kestrelc/src/where_info.rs`), not two
separately-maintained copies.

`kestrelc <file.kes>` (no `--wasm`) compiles and links `<file>.kes` into
a native executable named after the file (in the current directory),
using the
system `cc` as the linker. Requires a working C toolchain (`cc`) on
`PATH` — nothing else; Cranelift itself is a pure-Rust dependency with
no system requirements beyond that.

**Windows works too**, as of a real cross-platform pass — three
platform-specific bugs are fixed, not worked around: `kestrelc_runtime.c`
used `sysconf(_SC_NPROCESSORS_ONLN)` for processor-count detection, a
POSIX/glibc extension MinGW-w64's UCRT doesn't implement (now falls back
to `GetSystemInfo` under `#ifdef _WIN32`); every generated function
signature (Kestrel functions, `printf`, the `parallel_map` runtime shim)
was hardcoded to `CallConv::SystemV` instead of asking the target ISA
for its actual native convention (Windows x64 uses a different one —
this silently passed arguments in the wrong registers); and Cranelift's
`enable_probestack` defaults to *off*, which is a guaranteed
`STATUS_ACCESS_VIOLATION` crash the moment a stack-allocated array
literal (see "Scope" below) crosses one 4KB page, since Windows relies
on each stack page being touched in order to grow the stack — now
enabled with the `inline` strategy. On Windows, `cc` needs to resolve to
a real GCC (e.g. via a MinGW-w64 toolchain on `PATH`) — MSVC's `cl.exe`
is not a drop-in `cc` and hasn't been tested.

## Error diagnostics

Lex, parse, purity-check, `parallel_map()`-misuse, and type-check errors
are all reported as `file:line:col: message`, followed by the offending
source line and a `^` span underneath it, e.g.:

```
kestrelc: fib.kes:3:12: Unexpected token 'RParen'
  return x +;
           ^
```

`format_diagnostic` (`src/lib.rs`) is the single formatter behind this,
shared by the CLI (`main.rs`) and `compile_to_wasm_bytes` (and so
`kestrelc-web`/`kestrel-editor.html`'s "native (wasm)" engine). Every
`Stmt` in `src/ast.rs` carries the line/col of its first token (set by
the parser); `purity::check_purity`, `purity::check_parallel_map`, and
`typecheck::check_types` return `CheckError { message, line, col }`
values pinned to the statement they were found in, instead of bare
strings. Honest scope: statement granularity, not full per-expression —
`let x = f(a) + g(b);` points at the start of the `let`, not at
whichever of `f(a)`/`g(b)` was actually the problem.

The native backend's own codegen errors (`codegen.rs`'s "kestrelc only
supports X so far" / "Unknown identifier" / etc. — scope errors, not
syntax errors) now carry a position too, but only a bare `line:col:`
prefix, e.g. `kestrelc: 3:5: 'a' is an array — it can only be indexed...`
— not the full `file:line:col:` + caret treatment above, since
`codegen.rs` never has the original source text or filename threaded
through it. Still open: the same for `wasm_codegen.rs`'s own errors, and
every backend's runtime errors (unknown identifier, out-of-bounds
index, etc.), which still don't carry a source position at all. See
`kestrel-DESIGN.md`'s roadmap for what's left.

## Compile cache

`kestrelc` caches its output across invocations, keyed by a content hash
of the source text (plus which backend — native and `--wasm` cache
separately). Compiling the exact same `.kes` file again — the common
case during a dev loop, or every time the browser editor's native engine
runs unchanged code — skips lexing/parsing/purity-checking/codegen
entirely and reuses the cached artifact; the output line says
`(cached)` when this happens. Any edit to the source is a different
hash, so it's always a correctness-safe cache: a hit only ever happens
for byte-identical input.

Cache location: `$KESTRELC_CACHE_DIR`, else `$XDG_CACHE_HOME/kestrelc`,
else `$HOME/.cache/kestrelc`, else caching is silently skipped (compiles
still work, just without the speedup). Delete the directory to clear it.
See `src/cache.rs` for the implementation.

## Runtime call-count profiling and inlining (native only)

The native backend goes one step further than "skip redundant
recompilation": every compiled binary counts how many times each of its
own functions actually ran, and writes those counts to a small profile
file next to its cache entry when it exits (`src/profile.rs`,
`runtime/kestrelc_runtime.c`'s `kestrelc_profile_record`). The *next*
`kestrelc` compile of that same source reads the file back and inlines
small, pure, non-recursive functions with only scalar parameters that
were called at least 5 times in a previous run (`src/inline.rs`) —
saving real call overhead at whichever sites turned out to matter, based
on how the program actually ran, not a static guess. Because the
compiled artifact can now legitimately differ for byte-identical source
depending on what profile data exists, the compile-cache key for the
native backend folds in a fingerprint of the current profile (see
`cache::artifact_key`); the profile file's own path stays stable
(`cache::key`) so it isn't lost across that.

Recorded counts are a running historical maximum, not each run's raw
number — necessary for the loop to settle. Once a function is inlined,
its own compiled body has no call sites left calling it, so a fresh
binary's raw count for it would read back as 0; recording that naively
would make the next compiler run conclude "not hot anymore," un-inline
it, make it hot again next run, forever. Keeping the highest count ever
observed avoids that flip-flop.

Honest scope: call-count-driven inlining only, not the fuller
branch/shape-profiling and speculative pre-specialization
`kestrel-DESIGN.md`'s idea #1 describes as the end goal — and not
transitive (if hot function A's body calls hot function B, A's inlined
copy still calls B as a real function). WASM has no persistent profile
at all (no filesystem in a browser); this is native-only.

## Scope

kestrelc's front end (lexer, parser, purity checker) is a complete port
of `kestrel.js`'s and accepts the full grammar in `docs/SYNTAX.md`.
Codegen, however, currently supports a subset:

**Supported:**
- Every scalar runtime value is a 64-bit integer (numbers, and
  comparison/`true`/`false` results as 0/1) — no floats yet; a literal
  like `3.14` is a clean compile-time error ("kestrelc only supports
  integer literals so far"), not silently truncated
- Functions, including recursion, and `pure fn` (checked, same rules and
  error messages as `kestrel.js`)
- A first, honestly-scoped **type checker**: infers each expression's
  value kind (integer vs. boolean) from literals and operators, and
  rejects mixing them (`5 + true`, `if (5) {...}`) or calling a
  function with the wrong argument count. Doesn't yet check declared
  parameter type names against call-site arguments — see
  `docs/SYNTAX.md`'s "Type checking" section.
- `let`, assignment, `if`/`else`, `while`
- Arithmetic (`+ - * / %`), comparisons, `&&`/`||` (not short-circuiting,
  matching the other two backends)
- `print`, with string literals **only as direct print() arguments**
  (`print("x =", x)` works; storing a string in a variable or passing
  one to a function does not)
- **Arrays**: literals (`let a = [1, 2, 3];`), array-typed parameters
  (`fn f(a: [i32; N])`), and indexing (`a[i]`) — represented at runtime as
  a (pointer, length) pair. Array values can only be indexed or passed to
  a function so far — not returned, not stored inside another array, not
  aliased with a second `let`.
- **Proof-carrying bounds elision, including across function calls**:
  a literal index into a `let`-literal array (`a[2]`, `a = [1,2,3]`) is
  proven safe/unsafe entirely at compile time, no runtime check either
  way. And the design doc's own `get_safe(arr: [i32;N], i) where i < N`
  example now works as originally specified: every call site must prove
  the clause (literal index, literal-length array argument) or it's a
  **compile error** — never a silent trust — and once proven, the check
  inside `get_safe`'s own body is fully elided too. Anything less static
  (a variable index, or proving one call's safety from another's) isn't
  provable yet and is rejected at compile time rather than silently
  falling back to a runtime check for `where`-guarded calls specifically.
  Indexing *without* a `where` clause still falls back to an ordinary
  runtime check, same as `run`/`runFast`; a failing runtime check now
  prints `kestrelc: Index N out of bounds for array of length M` before
  halting, in both backends — the native backend calls a small runtime
  function (`kestrelc_bounds_fail` in `runtime/kestrelc_runtime.c`)
  that writes to stderr and exits(1) instead of a bare trap (still a
  trap right after, purely to satisfy Cranelift's "every block needs a
  terminator" — unreachable in practice); the WASM backend prints the
  same message through the same host `print_i64`/`print_str` imports
  the program's own `print()` calls use, then traps via `unreachable`.
  The WASM backend (below) has the identical elision scope too,
  including eliding the check *inside* a `where`-guarded function body
  from its call sites' proofs — both backends share one `WhereInfo`
  analysis in `kestrelc/src/where_info.rs`.
- **`parallel_map(f, arr)`**, with real OS-thread parallelism — see
  "Parallel map" below. `f` must be a `pure fn` taking exactly one
  scalar parameter; `arr` must be a fixed-size array *literal*
  (`let x = [...]`), not a parameter, since the output array's size has
  to be known at compile time (it's a plain stack allocation). The WASM
  backend accepts the same programs but runs them sequentially — see
  below for why.

**Not supported yet — a clear compile error, never a silent miscompile:**
- Proving a `where` clause from anything other than a literal index and
  a literal-length array at the call site (e.g. chaining one proven-safe
  call's guarantee into another).
- Strings as general values (only as literal print arguments)
- Floats with real fractional semantics
- Indexing/passing anything other than a plain array variable (e.g. the
  result of a function call, or a nested array expression)

## Parallel map

```
pure fn square(x: i32) -> i32 { return x * x; }
fn main() {
    let nums = [1, 2, 3, 4, 5];
    let squares = parallel_map(square, nums);
    print(squares[0]);
}
```

See `kestrel-DESIGN.md` idea #5 ("fearless parallelism, powered by
purity") for the full rationale. In short: a `pure fn` can't observe or
be affected by any other call to itself, so applying it once per array
element has nothing to race over no matter what order (or how much
overlap) those calls happen in — purity alone is the safety proof, no
`unsafe`, no manual audit.

`kestrelc`'s native backend is the only one that actually runs this in
parallel. It's implemented as a small C shim
(`runtime/kestrelc_runtime.c`, ~100 lines, linked into every native
build automatically) rather than hand-rolled Cranelift IR — Cranelift
has no pthread-aware primitives, and there's no benefit to re-deriving
`pthread_create`'s calling convention by hand when `cc` already knows
how to compile straightforward C and link it against libpthread.
Generated code just calls one function, `kestrelc_parallel_map_i64`,
the same way it already calls libc's `printf`. That function spawns one
thread per available CPU core (`sysconf(_SC_NPROCESSORS_ONLN)` on
POSIX, `GetSystemInfo` on Windows), splits the array into contiguous
chunks, and calls straight back into the
Cranelift-compiled `pure fn` (via its own address, obtained with
Cranelift's `func_addr`) from each thread. Below 10,000 elements or on
a single-core machine, it runs inline instead — thread setup/teardown
would cost more than it saves.

**Measured** on this machine (4 logical CPUs): a CPU-heavy `pure fn`
applied to a 20,000-element array via `parallel_map`, external-process/
best-of-N timed against an equivalent hand-written sequential C loop
doing the identical work — **~2.1x faster**, consistent across a light
and a 40x-heavier per-element workload. Honestly below the ideal ~4x
for 4 cores (thread overhead, memory bandwidth, and likely some
virtualization/container overhead on this particular machine all play
a part) — take the multiplier as this-machine-specific, not universal.

`run`/`runFast` (single-threaded JS) and the WASM backend accept the
exact same `parallel_map` programs and produce identical results, but
apply `f` sequentially — kestrel.js's sequential version is what the
native backend's real threaded output is checked against for
correctness (see `tests/integration.rs`'s large-array test, which
generates a 20,000-element array to force the real thread-pool path,
not just the small-array inline fallback).

## Loop fusion

A chain of `let a = parallel_map(f, arr); let b = parallel_map(g, a);`
(with `a` used nowhere else) fuses into one `parallel_map` over `arr`
with a synthesized pure fn computing `g(f(x))` — one pass instead of
two, no intermediate array. `kestrelc/src/fusion.rs`'s `fuse_loops` is
a direct Rust port of kestrel.js's `fuseLoops` (same matching rules —
see `kestrel-DESIGN.md`), wired into both the native and `--wasm`
backends right before codegen. One real difference from the JS
version: this backend's codegen requires a `parallel_map` array
argument to always be a plain identifier bound via a literal-length
`let`, never an inline array literal, so the fused output
re-introduces a `let` binding for the source array where the JS
version would just inline it.

## Memoization (native only)

A `pure fn` called repeatedly with the same arguments returns the
cached result instead of recomputing — safe because purity means the
call can't observe or be affected by any other call to itself. Eligible
functions: `pure`, not `main`, only scalar (no array) parameters, and
**never passed as `parallel_map`'s callback argument anywhere in the
program**. That last rule is the whole safety story: a memoized
function's own cache (`kestrelc_runtime.c`'s `kestrelc_memo_lookup`/
`kestrelc_memo_store`) has no locking at all, which is only sound
because the exclusion guarantees it's never touched from more than one
OS thread — `parallel_map`'s worker threads are the only way a Kestrel
function ever runs concurrently, and any function that could be called
that way is compiled unmemoized instead. Eligible functions get a
compile-time-assigned "slot" (capped at 64 per program — a function
past the cap just compiles unmemoized, not an error); each slot is a
plain growable array of `(args, result)` entries, linear-scanned, since
these are small toy programs, not a scale target for a hash table yet.
Recursive functions memoize correctly too (each recursive call is a
normal lookup-or-compute-and-store against the same slot).

## Benchmarks

Measured on this machine, comparing `kestrelc`-compiled binaries against
the JS backends (`node`, in-process timing) and against the Rust/C++
reference implementations from `kestrel-DESIGN.md` (external process
timing, best-of-N, `-O`/`-O2`, forced to actually execute rather than be
constant-folded away — see that doc for the methodology).

To keep the `kestrelc` numbers comparable to the *self-timed* Rust/C++
numbers (which exclude process startup), a no-op Kestrel program's pure
process-startup cost was measured separately (~2.4 ms on this machine —
dynamic linking + a PIE executable) and is reported as both the raw
external number and a startup-subtracted estimate:

| Workload | `kestrelc`, external | `kestrelc`, compute-only estimate | Rust (self-timed) | C++ (self-timed) |
|---|---|---|---|---|
| `fib(30)`, naive recursion | 8.5 ms | ~6.1 ms | 2.9-3.4 ms | 1.65-1.7 ms |
| Arithmetic loop, 20M iterations | 15.0 ms | ~12.6 ms | ~8 ms (forced, see note) | ~14.5 ms (forced, see note) |

*Note on the loop numbers:* an optimizing compiler can sometimes prove a
loop's entire result mathematically and skip running it — this actually
happened to the first, unguarded Rust version of this benchmark (it
measured 0.000 ms). The Rust/C++ numbers above use a compiler barrier to
force the loop to genuinely execute, same as `kestrelc`'s codegen (which
doesn't do this kind of whole-loop elimination) is forced to by
construction. Full detail in `kestrel-DESIGN.md`.

**Against the JS backends** (in-process timed, from `kestrel-DESIGN.md`):

| Workload | `run` | `runFast` | `kestrelc` (external) | Speedup vs `runFast` |
|---|---|---|---|---|
| `fib(30)` | 384 ms | 420 ms | 8.5 ms | **~50x** |
| Arithmetic loop, 20M | 5150 ms | 2600 ms | 15.0 ms | **~173x** |

**Reading this honestly:** this is `kestrelc`'s first working version,
with zero custom optimization passes — just Cranelift's built-in
`opt_level=speed` and a straightforward, unoptimized AST-to-IR
translation (no inlining, no bounds-check elimination since there are no
bounds checks yet, no persistent cache). Landing within single digits of
Rust/C++ this early is a good sign for the overall "compile straight to
machine code" strategy in `kestrel-DESIGN.md`, but it isn't evidence of
anything beyond that yet — none of the design doc's actually-novel ideas
(persistent cache, proof-carrying optimization, layout polymorphism) are
implemented here at all.

## Design notes

- **Locals are Cranelift `Variable`s**, one per distinct name a function
  binds (params, then each `let` in first-occurrence order — including
  inside nested `if`/`while` bodies), matching the same flat,
  non-block-scoped semantics as `kestrel.js`'s interpreter and bytecode
  VM. `cranelift-frontend`'s `FunctionBuilder` handles SSA construction
  (phi nodes at merge points) automatically from this.
- **`print` calls `libc`'s `printf`** directly, once per argument, with a
  fixed (non-variadic) Cranelift call signature. This is safe on the
  System V x86-64 ABI specifically because Kestrel has no floating-point
  values yet — the varargs calling-convention wrinkle that a fixed-arity
  call site would otherwise violate only applies to *floating-point*
  variadic arguments.
- **Every function returns `i64`**, even ones with no `-> type` in the
  source (returning 0 in that case) — avoids modeling void functions
  separately for now.
- **Arrays are (pointer, length) pairs** — two Cranelift `Variable`s per
  array-typed name instead of one, since a Variable is a single SSA
  value. A `let x = [1, 2, 3];` stack-allocates the exact byte size
  (`create_sized_stack_slot`) and stores each element; an array-typed
  parameter gets two `AbiParam`s in the Cranelift signature instead of
  one, and the caller passes both values at the call site. Indexing
  computes `ptr + index * 8` and does the bounds compare inline before
  the load — no separate bounds-checking function to call.
