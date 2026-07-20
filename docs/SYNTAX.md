# Kestrel Syntax Reference

This documents exactly what the current implementation (`kestrel.js`)
accepts — not the aspirational features in `kestrel-DESIGN.md` (layout
polymorphism, the optimization cache, general proof system). If it's
not in a code example below, it isn't implemented yet.

Everything here applies identically to `Kestrel.run` (tree-walking
interpreter) and `Kestrel.runFast` (bytecode compiler + stack VM). They
share the same lexer, parser, purity checker, and bounds-proof pass, and
are semantics-identical: same output, same errors, same messages.
`runFast` is not uniformly faster — see the benchmark table in
`kestrel-DESIGN.md` before assuming it's the one to reach for.

There's also a third, separate implementation: `kestrelc/`, a native
compiler (Rust + Cranelift) that emits a real standalone executable
instead of running on either JS backend. Its front end accepts this
same full grammar, but its code generator currently only supports a
subset — integers, arrays (literals, indexing, array-typed parameters,
always bounds-checked), functions/recursion, `if`/`while`, `print` with
string-literal arguments — and gives a clear compile error (never a
silent miscompile) for anything outside that, like floats or strings as
general values. See `kestrelc/README.md` for the exact scope and real,
measured performance numbers.

## Comments

```
// a line comment — runs to end of line, nothing else is supported
```

## Literals

| Kind    | Syntax             | Notes                                   |
|---------|--------------------|------------------------------------------|
| Number  | `42`, `3.14`       | All numbers are JS floats under the hood; there's no distinct int/float type at runtime, only in annotations. |
| String  | `"hello"`          | No escape sequences — a `"` cannot appear inside a string literal. |
| Boolean | `true`, `false`    | |
| Array   | `[1, 2, 3]`        | Fixed at the literal site; elements can be any expression. |

## Types

Declared type *names* (`i32`, `usize`, etc.) are still not checked
against each other — they're accepted as arbitrary identifiers, with no
overflow checking and no enforcement that a call site's argument
actually matches a parameter's declared name. What **is** checked now
(every backend): each expression's inferred value *kind* — integer or
boolean, inferred purely from literals and operators, never from
declared names — so `5 + true`, `!5`, and a plain number used directly
as an `if`/`while` condition are compile errors, along with a
function-call argument *count* mismatch. See "Type checking" below for
the exact rules. `[i32; N]` array types exist for the bounds-proof
mechanism and as documentation for the real backend.

```
name: i32
name: usize
name: [i32; N]     // fixed-size array type; N is a symbolic or literal bound
```

## Variables

```
let x = 5;
x = x + 1;          // assignment to an already-declared local
```

There's no `const`/`mut` distinction and no block scoping — `let`
introduces a binding into the enclosing function's flat environment.
Assigning to a name that was never `let`-declared is a runtime error.

## Functions

```
fn name(param: type, ...) -> returnType {
    ...
}
```

- `pure fn` — see [Purity](#purity) below.
- `-> returnType` is optional; omit it for a function that returns
  nothing meaningful (interpreter still allows `return;` or falling off
  the end, both yielding `null`).
- `where <expr>` is optional — see [Bounds proofs](#arrays--bounds-proofs).
- Every program needs exactly one `fn main()`, taking no arguments; it's
  the entry point.

```
fn add(a: i32, b: i32) -> i32 {
    return a + b;
}

fn main() {
    print(add(2, 3));
}
```

## Purity

```
pure fn square(x: i32) -> i32 {
    return x * x;
}
```

A `pure fn` is checked at compile time (before anything runs) and is
rejected if it:

- calls `print` (I/O), directly or transitively,
- calls a non-`pure` function, or a `pure` function that itself turns
  out to be impure,
- assigns to any name that isn't one of its own locals (params or its
  own `let` bindings).

```
// Rejected — 'oops' is marked pure but calls print:
pure fn oops(x: i32) -> i32 {
    print("side effect!");
    return x;
}
```

This produces a compile-time `KestrelError` naming every offending
function, before the program executes at all — not a runtime warning.

## Type checking

First honest version — see `kestrel-DESIGN.md`'s roadmap for the full
rationale. Each expression's value *kind* (integer or boolean) is
inferred purely from literals and operators — `true`/`false`,
comparisons, `&&`/`||`/`!` are boolean; everything else is numeric —
and mixing them is a compile error:

```
print(5 + true);   // Rejected: '+' needs two numbers, found int and bool
print(!5);         // Rejected: '!' needs a boolean, found int
if (5) { ... }      // Rejected: if-condition must be a boolean expression, found int
```

A function call with the wrong number of arguments is also rejected:

```
fn add(x: i32, y: i32) -> i32 { return x + y; }
add(1, 2, 3);   // Rejected: 'add' expects 2 arguments, got 3
```

**What this deliberately doesn't do yet:** check a call site's argument
*kinds* against the callee's declared parameter type names (`foo(x:
i32)` called as `foo(some_bool)` isn't caught) — that needs a real
decision about what Kestrel's built-in types actually are first, since
today `i32`/`usize`/anything else are just arbitrary identifiers (see
"Types" above). A function parameter's kind is always treated as
unknown inside its own body for the same reason. Every rule only fires
when it's *sure* — it never guesses, so a program that would otherwise
run correctly is never rejected.

## Arrays & bounds proofs

```
fn get_safe(arr: [i32; N], i: usize) -> i32 where i < N {
    return arr[i];
}
```

The `where i < N` clause documents the precondition under which
`arr[i]` is safe. **Current status:** the interpreter records a note
that a function has a where-clause and always performs the runtime
bounds check on `arr[i]` regardless (out-of-range access throws
`KestrelError: Index N out of bounds for array of length M`). Compile-time
proof/elision of the check — the actual point of the feature per the
design doc — is not implemented yet; see `kestrel-DESIGN.md`.

## Control flow

```
if (cond) {
    ...
} else {
    ...
}

while (cond) {
    ...
}
```

- `else` is optional.
- There's no `for`, no `break`/`continue`, no `match`/`switch`.
- `if`/`while` conditions are plain expressions — no parens required
  around sub-expressions, but the outer parens around the condition
  itself are mandatory (`if (x) { }`, not `if x { }`).

## `print`

```
print(expr, expr, ...);
```

Evaluates each argument and joins them with a single space, then emits
one line. It's a statement, not an expression — `print(...)` cannot be
used inside another expression.

## `parallel_map`

```
let out = parallel_map(f, arr);
```

A reserved builtin call name, not a keyword — `f` and `arr` aren't
special syntax, this is an ordinary function call whose name the
compiler recognizes. `f` must be a bare function name (not a call, not
an expression) naming a `pure fn` that takes exactly one scalar
parameter; `arr` is any array expression. Applies `f` to every element
of `arr`, producing a new array of the same length: `out[i] == f(arr[i])`
for every `i`. Misusing it — `f` not pure, wrong parameter count, `f`'s
parameter is an array, an unknown function, or a non-identifier first
argument — is a compile error in every backend, checked unconditionally
(not just inside `pure fn` bodies).

Purity is what makes this safe to run in any order, or concurrently:
a `pure fn` can't observe or be affected by any other call to itself,
so there's nothing for two calls to race over. `run`/`runFast` (single-
threaded JS) and the WASM backend apply `f` sequentially; `kestrelc`'s
native backend is the only one that actually parallelizes it across
real OS threads (above a size threshold — see `kestrelc/README.md`). See
`kestrel-DESIGN.md` idea #5 for the full design rationale and current
scope (`arr` must currently be a fixed-size array literal, not a
parameter).

## Operators (highest to lowest precedence)

| Precedence | Operators           | Associativity |
|-----------|----------------------|----------------|
| 1 (unary)  | `-x`, `!x`          | right (prefix) |
| 2          | `*` `/` `%`         | left |
| 3          | `+` `-`             | left |
| 4          | `==` `!=` `<` `>` `<=` `>=` | left |
| 5          | `&&` `\|\|`         | left |

Array indexing `a[i]` and function calls `f(a, b)` bind tighter than
any operator (postfix, left-to-right, chainable: `a[i][j]`, `f()(x)` is
*not* valid since calls only apply to a bare identifier, not to an
arbitrary expression).

Parentheses `(expr)` can always be used to override precedence.

## Full grammar

```
program    := item*
item       := fnDecl
fnDecl     := 'pure'? 'fn' IDENT '(' params ')' ('->' type)? ('where' expr)? block
params     := (param (',' param)*)?
param      := IDENT ':' type
type       := IDENT | '[' type ';' IDENT ']'
block      := '{' stmt* '}'
stmt       := letStmt | ifStmt | whileStmt | printStmt | returnStmt | assignStmt | exprStmt
letStmt    := 'let' IDENT '=' expr ';'
assignStmt := IDENT '=' expr ';'
ifStmt     := 'if' '(' expr ')' block ('else' block)?
whileStmt  := 'while' '(' expr ')' block
printStmt  := 'print' '(' args ')' ';'
returnStmt := 'return' expr? ';'
exprStmt   := expr ';'
args       := (expr (',' expr)*)?
expr       := comparison (('&&'|'||') comparison)*
comparison := additive (('=='|'!='|'<'|'>'|'<='|'>=') additive)*
additive   := term (('+'|'-') term)*
term       := unary (('*'|'/'|'%') unary)*
unary      := ('-'|'!')? postfix
postfix    := primary ('[' expr ']' | '(' args ')')*
primary    := NUMBER | STRING | 'true' | 'false' | IDENT | '(' expr ')' | arrayLit
arrayLit   := '[' args ']'
```

## Known gaps (not bugs — just not built yet)

- No structs/records, no user-defined types beyond arrays.
- No string operations beyond literals (no concatenation operator,
  no indexing into strings).
- No `for`, `break`, `continue`, `match`.
- No modules/imports — a program is a flat list of functions in one file.
- No int overflow, no float/int distinction at runtime.
- `where` clauses are advisory only (see [Bounds proofs](#arrays--bounds-proofs)) —
  they don't yet eliminate the runtime check or turn unprovable call
  sites into compile errors, both of which are the design's actual goal.

See `kestrel-DESIGN.md` for what's planned beyond this.
