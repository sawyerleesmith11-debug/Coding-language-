# `kestrelc-devtool` — a real, native-speed dev editor — design

## Status

Approved scope, not yet implemented.

## Problem

`kestrel-editor.html` (the existing browser playground) compiles and runs
Kestrel source entirely client-side, via a WASM build of `kestrelc`
(`kestrelc-web`) executing inside the browser's WASM sandbox. That's the
right architecture for a shareable, zero-server public demo, but it's the
wrong one for actually watching `kestrelc` work: it uses `wasm_codegen.rs`
(a separate, less-tuned backend) rather than `codegen.rs`'s native
Cranelift AOT path or `jit_codegen.rs`'s in-process JIT path — the two
backends that produced every real performance number from tonight's
session (the JIT watch mode's ~2ms recompiles, the `parallel_map`
benchmark wins). A WASM-in-browser editor would show different, likely
slower numbers than the real ones, misrepresenting what was actually
built. It also has no compile-time/run-time breakdown — both are lumped
into one elapsed number — and there's no way to compare native-speed
runs at all today outside a terminal.

## Goal

A real local application: run it, a browser tab opens showing an editor
UI, type/paste Kestrel source, click Run, see the actual native compiler
work — real compile time and real run time, reported separately, backed
by the same `kestrelc` code paths already proven tonight (in-process JIT
where supported, native AOT subprocess fallback otherwise) — not a
reimplementation, not a slower sandboxed stand-in.

## Explicitly out of scope

- A Stop button / interrupting a hung program mid-run. Confirmed not
  needed. A genuine infinite loop with no recursion/div-by-zero (nothing
  any existing guard catches) will hang the server's JIT-execution
  thread; recovery is restarting the server. Documented, not silently
  glossed over.
- Any WASM path. This tool exists specifically because the WASM path
  gives the wrong numbers for this purpose — `kestrel-editor.html`/
  `kestrelc-web` are untouched, kept as-is for their own (different)
  purpose.
- Multi-file projects, syntax highlighting, LSP-style features. A plain
  textarea + Run button + output pane, matching `kestrel-editor.html`'s
  existing visual style but none of its WASM machinery.
- Persisting/saving source between sessions — paste-and-run only, same
  as the existing web editor today.

## Architecture

- New crate: `kestrelc-devtool/` (binary), sibling to `kestrelc/` and
  `kestrelc-web/` at the repo root. Depends on `kestrelc` (path
  dependency, `native` feature — the same feature gate `kestrelc`'s own
  CLI binary already requires) and `tiny_http` (new dependency — a
  small, focused, no-async-runtime HTTP server; handles real HTTP edge
  cases, e.g. request body framing, that a hand-rolled parser would risk
  getting subtly wrong for uncertain benefit, given `kestrelc` already
  pulls in small focused crates like `cranelift`/`notify`/`wasm-encoder`
  rather than being zero-dependency as a hard rule).
- `main()`: starts a `tiny_http::Server` bound to `127.0.0.1:<port>`
  (a fixed port, e.g. 7420, chosen once — not user-configurable in v1),
  then opens the system's default browser pointed at that address (on
  Windows: `Command::new("cmd").args(["/c", "start", url])`, matching
  the "no extra dependency for something this small" pattern already
  used elsewhere in this project rather than pulling in an `open`-style
  crate).
- Two routes:
  - `GET /` — serves a single embedded HTML/JS/CSS page (a new file,
    `kestrelc-devtool/ui.html`, `include_str!`'d into the binary — no
    separate static-file-serving path needed, no risk of the binary and
    its UI drifting apart on disk). Visually modeled on
    `kestrel-editor.html`'s existing dark theme (same font choices,
    color palette) but written fresh — no WASM/`kestrel.js` loading
    code to strip out, since none of it is needed.
  - `POST /run` — body is the raw UTF-8 source text. Response is a JSON
    object (hand-encoded — the response shape is small and fixed, a
    full JSON crate dependency isn't warranted for 5 fields):
    ```json
    {
      "engine": "jit" | "aot",
      "ok": true | false,
      "compile_ms": 1.37,
      "run_ms": 0.02,
      "output": "hi 42\n",
      "error": null | "kestrelc: ...(full formatted diagnostic)..."
    }
    ```
- Compile/run orchestration (`kestrelc-devtool/src/main.rs` or a small
  `runner.rs` alongside it): mirrors `kestrelc/src/watch.rs`'s
  `try_jit`-then-fallback structure closely, reusing the exact same
  library calls `watch.rs` already makes (`lexer::lex`, `parser::parse`,
  `resolve::resolve`, `purity::check_purity`/`check_parallel_map`,
  `typecheck::check_types`, `jit_codegen::check_jit_supported`,
  `jit_codegen::JitCodegen`) — not duplicating compiler logic, just
  timing it differently:
  - **JIT path:** `compile_ms` = time from start of `lexer::lex` through
    the end of `JitCodegen::compile_program` (i.e. everything up to but
    not including running the compiled code). `run_ms` = time spent
    inside `finish_and_run`'s `main_fn()` call specifically. This is a
    real split `watch.rs` doesn't currently expose (it only reports one
    combined "finished in Xms") — the devtool's runner captures its own
    `Instant`s around the same calls rather than modifying `watch.rs`
    itself, so `kestrelc watch`'s existing behavior/output format is
    completely unaffected by this feature.
  - **AOT fallback path** (JIT-unsupported programs — arrays, structs,
    `parallel_map`, `main` with parameters): spawns the real
    `kestrelc.exe` as a subprocess to compile (same
    `Command::new(exe).arg(path)` pattern `watch.rs` already uses),
    `compile_ms` timed around that call; then spawns the produced binary
    as a second subprocess, `run_ms` timed around that call. Two real
    subprocess invocations, matching exactly what `kestrelc file.kes`
    followed by running the output does today — genuinely the same
    speed as the CLI, not a reimplementation.
  - Output capture: JIT path's `print`/`printf` output currently goes to
    the process's real stdout (see `jit_codegen.rs`) — for the devtool,
    this needs redirecting/capturing rather than letting it go to the
    devtool server's own console. AOT path's output is naturally
    captured already (`Command::output()` on the spawned binary).
    **Open implementation detail, not a design blocker**: the JIT path's
    stdout capture needs either (a) temporarily redirecting the
    process's stdout file descriptor around the `main_fn()` call
    (platform-specific, a real but bounded piece of work), or (b) a
    small addition to `jit_codegen.rs`'s registered `printf`/`fflush`
    symbols allowing an alternate output sink to be injected instead of
    real C `printf` — likely the cleaner fix, and worth resolving during
    implementation rather than in this design doc, since it's a
    contained, well-scoped question with a clear owner (`jit_codegen.rs`
    already owns exactly where `printf` gets wired up).

## No server-side state

Every `/run` request compiles from the submitted source fresh — no
caching, no persisted session — matching the same "always a fresh
compile" rule `JitCodegen`'s own doc comment already states. Keeps the
implementation simple and avoids any staleness bugs between requests.

## Testing plan

- Unit tests (if practical) for the JSON response encoding (a small,
  fixed-shape function — easy to test directly without a real HTTP
  round-trip).
- Manual verification (this is fundamentally a UI/integration feature,
  same posture `watch-mode-design.md` already took for `kestrelc
  watch`): launch the tool, confirm the browser opens automatically,
  paste a JIT-eligible program (e.g. the same "hi 42" print program used
  to verify JIT watch mode tonight), confirm `compile_ms`/`run_ms` are
  both real, small, separately-reported numbers, and confirm the
  program's actual output appears. Paste a JIT-ineligible program (an
  array literal), confirm it falls back to `"engine": "aot"` and still
  produces correct output/timing via the real subprocess path. Paste a
  program with a compile error, confirm `ok: false` and the real
  formatted diagnostic message comes through.
