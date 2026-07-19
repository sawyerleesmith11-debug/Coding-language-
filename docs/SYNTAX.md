# Kestrel Syntax Reference

This documents exactly what the current interpreter (`kestrel.js`)
accepts — not the aspirational features in `kestrel-DESIGN.md` (layout
polymorphism, the optimization cache, general proof system). If it's
not in a code example below, it isn't implemented yet.

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

Types are written but **not checked** by the interpreter — `i32`,
`usize`, etc. are accepted as arbitrary identifiers with no semantic
enforcement (no overflow checking, no int/float distinction at
runtime). They exist for the bounds-proof mechanism and as documentation
for the eventual real backend.

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
