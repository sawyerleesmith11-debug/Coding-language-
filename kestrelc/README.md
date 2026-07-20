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
native backend, including compile-time-proven bounds elision for literal
indices into literal-length arrays — see "Scope" below for the one real
difference (no cross-function `where`-clause elision yet in this
backend; those accesses still get a runtime check).

`kestrelc <file.kes>` (no `--wasm`) compiles and links `<file>.kes` into
a native executable named after the file (in the current directory),
using the
system `cc` as the linker. Requires a working C toolchain (`cc`) on
`PATH` — nothing else; Cranelift itself is a pure-Rust dependency with
no system requirements beyond that.

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
See `src/cache.rs` for the implementation, and
`kestrel-DESIGN.md`'s idea #1 for how this relates to (and how it's
scoped down from) the fuller runtime-profile-guided cache described
there — this is "skip redundant recompilation," not (yet) branch/shape
profiling or speculative pre-specialization.

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
  runtime check, same as `run`/`runFast`; a failing runtime check
  **traps the process (`SIGILL`) immediately** rather than printing a
  message and exiting cleanly like the other two backends do — a real,
  known difference, not yet fixed. The WASM backend (below) has the same
  scope, except it doesn't (yet) elide the check *inside* a `where`-guarded
  function body from its call sites' proofs — that fast path is native-only
  for now, so those accesses still get a runtime check there (and trap via
  WASM's `unreachable` instruction instead of `SIGILL`).

**Not supported yet — a clear compile error, never a silent miscompile:**
- Proving a `where` clause from anything other than a literal index and
  a literal-length array at the call site (e.g. chaining one proven-safe
  call's guarantee into another).
- Strings as general values (only as literal print arguments)
- Floats with real fractional semantics
- Indexing/passing anything other than a plain array variable (e.g. the
  result of a function call, or a nested array expression)

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
