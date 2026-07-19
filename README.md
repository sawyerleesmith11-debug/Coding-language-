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
  a tree-walking interpreter. Zero dependencies; runs unmodified in
  Node or as a browser `<script>`.
- `kestrel-editor.html` — a single-file mobile code editor/IDE (embeds
  `kestrel.js` inline; add to iPhone home screen via Safari for an
  app-like experience).
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

## Testing

```sh
npm test
```

## Status

The tree-walking interpreter is the only backend implemented so far.
Next up, in priority order (see the design doc): a real bytecode or
native (LLVM/Cranelift) backend, the persistent cross-run optimization
cache, layout polymorphism, and a more general proof system.
