# Kestrel

A toy programming language focused on speed: compile-time purity
checking, compile-time bounds proofs, and (per the design doc) a
persistent cross-run optimization cache and layout polymorphism still
to come. "Kestrel" is a placeholder name.

See [`kestrel-DESIGN.md`](./kestrel-DESIGN.md) for the full design
rationale and status of each idea.

## Structure

- `kestrel.js` — lexer, parser, purity checker, bounds-proof notes, and
  a tree-walking interpreter. Zero dependencies; runs unmodified in
  Node or as a browser `<script>`.
- `examples/basics.kes` — a small example program exercising `pure fn`,
  arrays, and `where`-bounded access.
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
