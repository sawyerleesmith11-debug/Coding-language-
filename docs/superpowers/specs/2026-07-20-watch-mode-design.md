# `kestrelc watch` — design

## Status

Approved scope, not yet implemented.

## Problem

Testing a `.kes` file today means manually running `kestrelc file.kes && ./file`
after every edit. There's no fast edit-compile-run loop, and the web editor
(`kestrel.js`) is frozen and no longer the intended testing surface.

## Scope

A new `kestrelc watch <file.kes>` subcommand. Native backend only (matches
how this session has been testing kestrelc all along). No wasm, no GUI, no
editor — the user edits in their existing editor of choice.

## Behavior

1. `kestrelc watch path/to/file.kes` starts watching that single file.
2. On every save (file-content change), it:
   - Clears the terminal screen.
   - Prints a short banner (e.g. the file path and a timestamp).
   - Compiles the file the same way `kestrelc file.kes` does today.
   - If compilation fails: prints the compiler's error output, does **not**
     crash the watcher, keeps waiting for the next save.
   - If compilation succeeds: runs the resulting binary, streams its
     stdout/stderr live, and prints its exit code when it finishes.
3. `Ctrl+C` exits the watcher cleanly.
4. If the program being watched runs forever (e.g. an infinite loop), the
   watcher does not kill it automatically — that's the user's job (Ctrl+C
   kills the whole watch process, including the child). Out of scope to
   solve process lifecycle management beyond this.

## Architecture

- New file: `kestrelc/src/watch.rs` — owns the watch loop.
- `kestrelc/src/main.rs` gains a subcommand dispatch: `kestrelc watch <path>`
  routes here instead of the default compile-and-exit path. Existing
  `kestrelc <path>` (no subcommand) behavior is unchanged.
- New dependency: `notify` crate (file-system watching, cross-platform,
  widely used, no other new dependencies needed).
- Compilation reuses the existing compile pipeline (`lib.rs`'s public
  compile function used by `main.rs` today) — no duplicated compiler logic.
- Running the compiled binary reuses `std::process::Command`, same as the
  existing test harness in `tests/integration.rs` already does.
- Debounce: file-watcher events can fire multiple times for one logical
  save (some editors write in multiple steps). Debounce with a short delay
  (e.g. 100ms) before triggering a recompile, coalescing rapid-fire events
  into one run.

## Explicitly out of scope

- Watching multiple files or a whole directory — one file at a time.
- wasm backend.
- Any GUI, editor, or syntax highlighting.
- Automatically killing a long-running/infinite-looping compiled program.
- Hot-reload of a running program — always a fresh compile + fresh run.

## Testing plan

- Unit test (if practical) for the debounce logic in isolation.
- Manual verification: run `kestrelc watch` against a sample `.kes` file,
  edit it, confirm the loop recompiles/reruns; introduce a syntax error,
  confirm it prints the error without crashing the watcher; fix it, confirm
  it recovers.
