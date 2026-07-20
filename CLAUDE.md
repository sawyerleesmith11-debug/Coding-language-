# Kestrel — Coding Language

## Project Summary
Kestrel is a compiled programming language with compile-time purity checking, bounds proof verification, and multi-backend compilation (JS, WASM, native).

## Core Concepts
- **Purity System**: Functions marked `@pure` enable fearless parallelism
- **Bounds Proofs**: Compile-time verification of array bounds (eliminates runtime checks)
- **Type System**: Static typing with constraint-based reasoning
- **Multiple Backends**: 
  - JS interpreter (kestrel.js)
  - Bytecode VM (kestrelc)
  - Native Rust compiler (via Cranelift)
  - WASM (browser-based editor)

## Directory Structure
```
kestrelc/           - Rust compiler (primary implementation)
kestrelc-web/       - WASM build for browser
kestrel.js          - JavaScript interpreter/backend
kestrel-editor.html - Browser-based IDE
test/               - Test suite
examples/           - Example programs
docs/               - Documentation
```

## Key Entry Points
- **Compiler**: `kestrelc/main.rs` or `kestrelc/lib.rs`
- **JS Backend**: `kestrel.js`
- **Web Editor**: `kestrel-editor.html` + WASM compilation
- **Design Doc**: `kestrel-DESIGN.md` (for architectural details)

## Development Commands
```bash
cargo build          # Build compiler
cargo test           # Run tests
npm run build        # Build JS backend
```

## Common Mistakes
(Add mistakes you encounter frequently here)

## Recent Sessions
(Sessions archived in `.claude/completions/`)
