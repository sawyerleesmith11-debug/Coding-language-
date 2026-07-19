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
  hand-written Rust/C++ on what it does support.
- `kestrel-editor.html` — a single-file mobile code editor/IDE (embeds
  `kestrel.js` inline; add to iPhone home screen via Safari for an
  app-like experience). Auto-deployed to GitHub Pages on every push to
  `main` (see `.github/workflows/pages.yml`) — once Pages is enabled in
  repo Settings, it's served live at the repo's Pages URL.
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
output, same errors. It's not uniformly faster yet (see Status below),
so `run` is still the safer default.

## Testing

```sh
npm test
```

## Status

Three implementations now exist. `run` (tree-walking) and `runFast`
(bytecode VM) are semantics-identical and both cover the full language;
`runFast` is faster on loop/array-heavy code and currently slightly
slower on deep-recursion-heavy code. `kestrelc` (native, via Cranelift)
compiles a subset of the language straight to a real executable and is
**~50-175x faster than `runFast`**, landing within a few multiples of
hand-written Rust/C++ on its first working version — see
`kestrelc/README.md` for its exact scope and `kestrel-DESIGN.md` for the
full benchmark writeup and methodology. Next up, in priority order:
arrays and real bounds-check enforcement in `kestrelc`, the persistent
cross-run optimization cache, layout polymorphism, a more general proof
system, and CPU parallelism for `pure` functions.
