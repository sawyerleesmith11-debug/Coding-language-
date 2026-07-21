# JIT-backed `kestrelc watch` — design

## Status

Approved scope (by continuation authorization — user stepped away mid-session
with explicit "keep working" instruction; design decisions below are best-
judgment calls made in their absence, documented for review when they
return, not yet re-confirmed with them).

## Problem

`kestrelc watch <file.kes>` (shipped earlier this session) recompiles and
reruns on every save by spawning two real OS processes: self-invoking
`kestrelc` (which itself writes a `.o`, then spawns `cc` to link a `.exe`),
then spawning that `.exe` to run it. Each process spawn costs a real,
measured ~30ms floor on this machine (confirmed directly: a bare C
hello-world compiled with the same toolchain takes ~30ms warm). For a
trivial program, that floor dominates and the whole loop feels slow
(~60-100ms), even after `benchmarks/`-driven fixes eliminated every other
avoidable cost (caching the linked binary, fixing the Windows `.exe`-naming
bug that broke that cache).

The only way to beat a real OS process-spawn floor is to not spawn a
process — run the compiled code inside the watcher's own, already-running
process instead. This is exactly what the web editor already does with
WASM (`WebAssembly.instantiate` + a direct call, no process involved at
all).

## Scope: `kestrelc watch` only

This is Track 2 of the two-track speed goal established earlier this
session (Track 1 = runtime execution speed, addressed via the
Kestrel-vs-C benchmark suite; Track 2 = dev-loop iteration speed, this
design). The real, shipped `kestrelc file.kes` command (what actually gets
benchmarked and used to produce standalone binaries) is completely
untouched — it keeps using `cranelift-object` + `cc`, unconditionally.
Only `watch.rs`'s internal execution path changes.

## Approach: `cranelift-jit`

Kestrelc's `codegen.rs` already builds Cranelift IR for every function.
Today that IR goes to `cranelift-object`, which writes a `.o` file `cc`
then links. `cranelift-jit` is a different Cranelift backend: it maps
generated machine code directly into the *current process's* memory and
hands back a raw function pointer, callable immediately, no file, no
external linker, no second process.

`watch.rs`'s loop becomes: on save, lex/parse/resolve/purity/typecheck
(unchanged — this is all fast, pure-Rust work, not the bottleneck), then
JIT-compile with a new codegen path, then call the resulting `main`
function pointer directly, in-process.

## The real design problem: runtime imports

`codegen.rs` doesn't just emit self-contained machine code — every
compiled program depends on runtime support functions currently defined in
`kestrelc/runtime/kestrelc_runtime.c` and declared as `Linkage::Import`
symbols resolved by `cc` at link time: `printf` (real libc), plus
kestrelc's own `kestrelc_parallel_map_i64`, `kestrelc_bounds_fail`,
`kestrelc_profile_record`, `kestrelc_memo_lookup`, `kestrelc_memo_store`.

None of these currently exist inside `kestrelc.exe`'s own process — they
only get linked into the *output* programs `kestrelc` produces, never into
`kestrelc` itself. JIT-compiled code calling one of these symbols by name
needs `cranelift-jit`'s `JITBuilder::symbol()` to resolve to a real,
already-loaded function address in the *current* process — which means
these functions must be linked into `kestrelc.exe` itself, not just into
its output.

**Decision: compile `kestrelc_runtime.c` into a static library and link it
into `kestrelc` itself** (via a `build.rs` using the `cc` crate — a build-
time compile step, not a new runtime dependency), gated behind the
existing `native` Cargo feature (same as everything else JIT-related).
This makes every one of `kestrelc_runtime.c`'s functions resident in
`kestrelc.exe`'s own address space, reachable via `extern "C"` declarations
in Rust and registered with `JITBuilder::symbol()` by name, pointing at
their real addresses. `printf` is already resolvable by name (libc is
already linked into any Rust binary).

## v1 scope: what JIT mode actually supports

Given this is being designed and built without the user available to
weigh in on scope tradeoffs, v1 is deliberately narrow — support what
matters most for a fast edit-print-run loop, defer the rest as a
documented, disclosed gap rather than block on harder integration work:

- **Supported**: scalars (`let`/`=`), arithmetic and comparison operators,
  `if`/`while`, ordinary function calls (including recursion — this is
  just a normal Cranelift call, nothing JIT-specific needed), `print`/
  `print_str` (the dominant way a `kestrelc watch` session actually
  observes output).
- **Explicitly deferred, not supported in JIT mode (v1) — narrowed twice
  after a working spike confirmed the core mechanism, each time to keep
  the actual port achievable and reviewable in one continued session
  rather than attempting full parity with `codegen.rs` in one
  unsupervised pass**:
  - `parallel_map` — real pthread-based threading adds real complexity
    for an in-process JIT host (a worker thread calling back into JIT'd
    code raises real thread-safety questions about the JIT module's own
    internal state that deserve their own design).
  - Memoization and profile-guided inlining — arguably low-value in a
    rapid save-and-rerun loop anyway (a fresh JIT compile happens every
    save regardless, so cross-run profile persistence buys little).
  - **Structs** — a real, separate feature surface (`Slot::Struct`,
    field access, struct-typed parameters) already ported once this
    session (native to wasm); porting it a third time is real,
    separable follow-up work.
  - **Arrays** (both literals and indexing) — real complexity of its
    own (bounds-check codegen, the stack-vs-heap `alloc_array_buffer`
    threshold from this session's other fix, `resolve_array`'s pointer/
    length tracking). Deferring this alongside the above keeps v1 to a
    genuinely small, self-contained subset (scalars, control flow,
    calls, print) that's realistic to build correctly and get reviewed
    without the user present, rather than a large port with more
    surface area for an unsupervised mistake to hide in.

  A `.kes` program using any deferred construct still compiles and runs
  correctly via the normal `kestrelc file.kes` / non-JIT `kestrelc
  watch` fallback — JIT mode must detect the unsupported construct and
  report a clear, specific error (e.g. "arrays aren't supported under
  `kestrelc watch` yet") rather than attempting partial support or
  crashing, and `watch.rs` should fall back to the existing self-invoke
  path automatically when JIT mode reports "unsupported," not surface
  that as a hard failure to the user.

This narrowing is a judgment call made without the user present — flagged
clearly for their review, not presented as an unchangeable final scope.

## Architecture

- New file: `kestrelc/src/jit_codegen.rs` — mirrors `codegen.rs`'s
  structure (this session's structs/array work already established the
  "new codegen backend, same AST, same shape as the existing one" pattern
  twice now — native vs. wasm, and now native-AOT vs. native-JIT).
  Consumes the same fully-resolved, purity-checked, type-checked
  `Program` `codegen.rs` does; emits Cranelift IR via `cranelift-jit`'s
  `JITModule` instead of `cranelift-object`'s `ObjectModule`.
- `kestrelc/build.rs` (new): compiles `runtime/kestrelc_runtime.c` into a
  static library and links it into the `kestrelc` binary itself, gated
  behind the `native` feature.
- `kestrelc/src/watch.rs`: `compile_and_run`'s current
  `Command::new(exe).arg(path).status()` (self-invoke) +
  `Command::new(&bin_path).status()` (run the linked binary) pair is
  replaced by a call into `jit_codegen`, then a direct call through the
  returned function pointer. Compile errors (lex/parse/resolve/purity/
  typecheck) are reported exactly the same way as today — only the
  final codegen+execution step changes.
- `parallel_map` detection: `jit_codegen` rejects a program containing a
  `parallel_map` call with a clear message ("`parallel_map` isn't
  supported under `kestrelc watch` yet — compile normally with `kestrelc
  file.kes` to test it") rather than attempting unsafe partial support.

## Explicitly out of scope

- Any change to the real `kestrelc file.kes` / `kestrelc --wasm` paths.
- `parallel_map` and memoization support in JIT mode (v1) — see above.
- Hot-reload of a running program (unchanged from the existing watch
  design: always a fresh compile + fresh run per save).
- Any wasm-related work (JIT is native-only, same as the two bugs fixed
  earlier this session).

## Testing plan

- Integration tests mirroring the existing `watch_rejects_a_nonexistent_file`
  test's style, but exercising actual JIT execution: a simple `print`
  program compiled and run via `kestrelc watch` (this needs a way to
  invoke just the compile-and-run step without the interactive file-
  watching loop — likely a small `#[cfg(test)]`-only entry point in
  `jit_codegen`/`watch.rs` that skips the `notify` watcher, since the
  existing `tests/integration.rs` pattern already can't spawn/kill a
  real interactive watch session cleanly).
- A test proving `parallel_map` is rejected with a clear error message
  rather than crashing or silently misbehaving under JIT mode.
- A test proving a program using a struct is rejected with a clear
  error message under JIT mode.
- A test proving a program using an array is rejected with a clear
  error message under JIT mode.
- A test proving recursion (e.g. `fib`) works correctly under JIT
  execution — this is the one "supported" feature with any real
  subtlety (a self-call must resolve correctly within the same JIT
  compile), worth its own explicit test rather than assuming it's
  covered by the basic print/arithmetic tests.
- Manual verification: time an actual `kestrelc watch` save-and-rerun
  cycle for a trivial program, compare against this session's measured
  ~30-100ms AOT-based watch cycle — the whole point of this design is a
  real, felt speedup, not just passing tests.
