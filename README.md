# Kestrel

A toy programming language focused on speed: compile-time purity
checking, compile-time bounds proofs, and (per the design doc) a
persistent cross-run optimization cache and layout polymorphism still
to come. "Kestrel" is a placeholder name.

See [`kestrel-DESIGN.md`](./kestrel-DESIGN.md) for the full design
rationale and status of each idea, and [`docs/SYNTAX.md`](./docs/SYNTAX.md)
for the syntax reference and grammar.

## Structure

- `kestrel.js` — lexer, parser, purity checker, bounds-proof notes, and
  two backends: `Kestrel.run` (tree-walking interpreter) and
  `Kestrel.runFast` (bytecode compiler + stack VM). Zero dependencies;
  runs unmodified in Node or as a browser `<script>`.
- `kestrelc/` — a real native compiler (Rust + Cranelift) that emits a
  standalone executable, no VM at runtime at all. Separate program from
  `kestrel.js`; supports a subset of the language so far (see
  `kestrelc/README.md`) but already lands within a few multiples of
  hand-written Rust/C++ on what it does support. Also has a WASM backend
  (`kestrelc --wasm`).
- `kestrelc-web/` — `kestrelc` itself, compiled to WASM, so it can run
  *inside* the browser editor and compile Kestrel source to a runnable
  `.wasm` module client-side — no server, no native binary. See
  `kestrelc-web/README.md`.
- `kestrel-editor.html` — a single-file mobile code editor/IDE (embeds
  `kestrel.js` inline; add to iPhone home screen via Safari for an
  app-like experience). Its engine picker offers all three backends,
  including "native (wasm)" for near-native speed via `kestrelc-web`.
  Auto-deployed to GitHub Pages on every push to `main` (see
  `.github/workflows/pages.yml`, which also builds `kestrelc-web` and
  publishes it alongside the editor).
- `docs/SYNTAX.md` — syntax reference and full grammar.
- `examples/` — runnable example programs:
  - `basics.kes` — `pure fn`, arrays, `where`-bounded access.
  - `fibonacci.kes` — recursion.
  - `purity_violation.kes` — a program that's *meant* to fail the
    purity check, for testing the checker itself.
- `test/` — automated test suite (Node's built-in `node:test`, no
  dependencies).

## Running

```sh
node -e 'require("./kestrel.js").run(require("fs").readFileSync("examples/basics.kes", "utf8"))'
```

Swap `.run(` for `.runFast(` to use the bytecode VM instead — same
output, same errors, and faster on every workload measured so far (see
Status below). The editor defaults to it for this reason.

## Testing

```sh
npm test
```

## Status

Four ways to run Kestrel now exist. `run` (tree-walking) and `runFast`
(bytecode VM) are semantics-identical and both cover the full language;
`runFast` is faster than `run` on every workload measured so far
(59-89% faster, depending on the workload — see `kestrel-DESIGN.md` for
the numbers and methodology). `kestrelc` (native, via Cranelift)
compiles a subset of the language straight to a real executable and is
**~50-175x faster than `runFast`**, landing within a few multiples of
hand-written Rust/C++ on its first working version — see
`kestrelc/README.md` for its exact scope and `kestrel-DESIGN.md` for the
full benchmark writeup and methodology. And `kestrelc` itself now also
compiles to WASM (both as a `kestrelc --wasm` output target, and as
`kestrelc-web` — the compiler itself running in the browser) so that
same near-native speed is available directly in `kestrel-editor.html`,
no server or native binary required — pick "engine: native (wasm)", now
with array support there too. `kestrelc` also has a persistent,
cross-invocation compile cache now (skips redundant recompilation of
unchanged source — see `kestrelc/README.md`), though not yet the fuller
runtime-profile-guided version `kestrel-DESIGN.md` describes. Next up,
in priority order: that fuller profile-guided cache, layout
polymorphism, a more general proof system, and CPU parallelism for
`pure` functions.
