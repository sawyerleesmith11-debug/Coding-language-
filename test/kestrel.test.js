// Test suite for kestrel.js — lexer, parser, purity checker, bounds
// notes, and the tree-walking interpreter. Run with `node --test` or
// `npm test`. No dependencies, matching the project's zero-dep goal.

const { test, describe } = require("node:test");
const assert = require("node:assert/strict");
const Kestrel = require("../kestrel.js");

function runCollect(src, opts = {}) {
  const output = [];
  const { result, boundsNotes } = Kestrel.run(src, {
    onPrint: (s) => output.push(s),
    ...opts,
  });
  return { result, boundsNotes, output };
}

describe("lexer", () => {
  test("tokenizes numbers, idents, keywords, strings, operators", () => {
    const tokens = Kestrel.lex('pure fn f(x: i32) -> i32 { return x + 1; }');
    const types = tokens.map((t) => t.type);
    assert.deepEqual(types, [
      "PURE", "FN", "IDENT", "(", "IDENT", ":", "IDENT", ")", "->", "IDENT",
      "{", "RETURN", "IDENT", "+", "NUMBER", ";", "}", "EOF",
    ]);
  });

  test("tracks line numbers across newlines", () => {
    const tokens = Kestrel.lex("fn\nmain\n(\n)");
    assert.equal(tokens[0].line, 1);
    assert.equal(tokens[1].line, 2);
    assert.equal(tokens[2].line, 3);
    assert.equal(tokens[3].line, 4);
  });

  test("skips line comments", () => {
    const tokens = Kestrel.lex("// hello\nfn");
    assert.equal(tokens[0].type, "FN");
  });

  test("lexes two-char operators distinctly from their prefixes", () => {
    const types = Kestrel.lex("a == b != c <= d >= e && f || g -> h").map((t) => t.type);
    assert.deepEqual(
      types.filter((t) => t !== "IDENT" && t !== "EOF"),
      ["==", "!=", "<=", ">=", "&&", "||", "->"]
    );
  });

  test("lexes string literals", () => {
    const tokens = Kestrel.lex('"hello world"');
    assert.equal(tokens[0].type, "STRING");
    assert.equal(tokens[0].value, "hello world");
  });

  test("throws KestrelError on unexpected character", () => {
    assert.throws(() => Kestrel.lex("$"), Kestrel.KestrelError);
  });
});

describe("parser", () => {
  test("parses a pure fn with return type and body", () => {
    const program = Kestrel.parse(Kestrel.lex(
      "pure fn square(x: i32) -> i32 { return x * x; }"
    ));
    assert.equal(program.length, 1);
    const fn = program[0];
    assert.equal(fn.kind, "fn");
    assert.equal(fn.name, "square");
    assert.equal(fn.pure, true);
    assert.deepEqual(fn.params, [{ name: "x", type: { kind: "named", name: "i32" } }]);
    assert.deepEqual(fn.returnType, { kind: "named", name: "i32" });
    assert.equal(fn.body.length, 1);
    assert.equal(fn.body[0].kind, "return");
  });

  test("parses array types and where clauses", () => {
    const program = Kestrel.parse(Kestrel.lex(
      "fn get_safe(arr: [i32; N], i: usize) -> i32 where i < N { return arr[i]; }"
    ));
    const fn = program[0];
    assert.deepEqual(fn.params[0].type, { kind: "array", elem: { kind: "named", name: "i32" }, size: "N" });
    assert.equal(fn.where.kind, "binop");
    assert.equal(fn.where.op, "<");
  });

  test("respects arithmetic operator precedence", () => {
    // 2 + 3 * 4 should parse as 2 + (3 * 4), not (2 + 3) * 4
    const program = Kestrel.parse(Kestrel.lex(
      "fn main() { return 2 + 3 * 4; }"
    ));
    const expr = program[0].body[0].value;
    assert.equal(expr.kind, "binop");
    assert.equal(expr.op, "+");
    assert.equal(expr.left.value, 2);
    assert.equal(expr.right.kind, "binop");
    assert.equal(expr.right.op, "*");
  });

  test("parses comparison and logical operators left-associatively", () => {
    const program = Kestrel.parse(Kestrel.lex(
      "fn main() { return a < b && c > d; }"
    ));
    const expr = program[0].body[0].value;
    assert.equal(expr.kind, "binop");
    assert.equal(expr.op, "&&");
  });

  test("parses array literals and indexing", () => {
    const program = Kestrel.parse(Kestrel.lex(
      "fn main() { let a = [1, 2, 3]; return a[1]; }"
    ));
    const letStmt = program[0].body[0];
    assert.equal(letStmt.value.kind, "array_lit");
    assert.equal(letStmt.value.elems.length, 3);
    const ret = program[0].body[1];
    assert.equal(ret.value.kind, "index");
  });

  test("parses if/else and while", () => {
    const program = Kestrel.parse(Kestrel.lex(
      "fn main() { if (a) { print(\"y\"); } else { print(\"n\"); } while (a) { a = a - 1; } }"
    ));
    const [ifStmt, whileStmt] = program[0].body;
    assert.equal(ifStmt.kind, "if");
    assert.ok(ifStmt.elseBlock);
    assert.equal(whileStmt.kind, "while");
  });

  test("parses unary minus and logical not", () => {
    const program = Kestrel.parse(Kestrel.lex("fn main() { return -x; }"));
    assert.equal(program[0].body[0].value.kind, "unary");
    assert.equal(program[0].body[0].value.op, "-");
  });

  test("throws KestrelError on malformed input", () => {
    assert.throws(() => Kestrel.parse(Kestrel.lex("fn main( { }")), Kestrel.KestrelError);
  });

  test("throws on missing 'main' at parse-adjacent stage (unknown token)", () => {
    assert.throws(() => Kestrel.parse(Kestrel.lex("fn foo() }")), Kestrel.KestrelError);
  });
});

describe("purity checker", () => {
  test("accepts a pure fn with only arithmetic and pure calls", () => {
    const program = Kestrel.parse(Kestrel.lex(`
      pure fn square(x: i32) -> i32 { return x * x; }
      pure fn sum_of_squares(a: i32, b: i32) -> i32 { return square(a) + square(b); }
    `));
    assert.deepEqual(Kestrel.checkPurity(program), []);
  });

  test("rejects a pure fn that calls print", () => {
    const program = Kestrel.parse(Kestrel.lex(`
      pure fn f(x: i32) -> i32 { print("x"); return x; }
    `));
    const errors = Kestrel.checkPurity(program);
    assert.equal(errors.length, 1);
    assert.match(errors[0], /'f' is marked pure/);
  });

  test("rejects a pure fn that calls an impure function", () => {
    const program = Kestrel.parse(Kestrel.lex(`
      fn impure_helper() { print("side effect"); }
      pure fn f() { impure_helper(); }
    `));
    const errors = Kestrel.checkPurity(program);
    assert.equal(errors.length, 1);
  });

  test("rejects a pure fn that assigns to a non-local", () => {
    // `total` is never declared with `let` inside f, so assigning to it
    // is treated as mutating something outside the function's locals.
    const program = Kestrel.parse(Kestrel.lex(`
      pure fn f(x: i32) -> i32 { total = x; return total; }
    `));
    const errors = Kestrel.checkPurity(program);
    assert.equal(errors.length, 1);
  });

  test("allows a pure fn to assign to its own locals", () => {
    const program = Kestrel.parse(Kestrel.lex(`
      pure fn f(x: i32) -> i32 { let y = x; y = y + 1; return y; }
    `));
    assert.deepEqual(Kestrel.checkPurity(program), []);
  });

  test("does not infinite-loop on recursive pure functions", () => {
    const program = Kestrel.parse(Kestrel.lex(`
      pure fn fact(n: i32) -> i32 { return n; }
    `));
    assert.doesNotThrow(() => Kestrel.checkPurity(program));
  });

  test("run() throws when a pure fn is impure", () => {
    assert.throws(
      () => Kestrel.run(`
        pure fn f() { print("boom"); }
        fn main() { f(); }
      `),
      Kestrel.KestrelError
    );
  });
});

describe("bounds proof notes", () => {
  test("emits a note for functions with a where clause", () => {
    const program = Kestrel.parse(Kestrel.lex(
      "fn get_safe(arr: [i32; N], i: usize) -> i32 where i < N { return arr[i]; }"
    ));
    const notes = Kestrel.checkBounds(program);
    assert.equal(notes.length, 1);
    assert.match(notes[0], /'get_safe'/);
  });

  test("emits no notes for functions without a where clause", () => {
    const program = Kestrel.parse(Kestrel.lex(
      "fn main() { print(\"hi\"); }"
    ));
    assert.deepEqual(Kestrel.checkBounds(program), []);
  });
});

describe("interpreter / run()", () => {
  test("runs the basics.kes example end to end", () => {
    const fs = require("node:fs");
    const path = require("node:path");
    const src = fs.readFileSync(path.join(__dirname, "../examples/basics.kes"), "utf8");
    const { output, boundsNotes } = runCollect(src);
    assert.deepEqual(output, [
      "square: 9",
      "square: 16",
      "square: 25",
      "square: 36",
      "sum of squares(3,4) = 25",
      "safe get nums[2] = 5",
    ]);
    assert.equal(boundsNotes.length, 1);
  });

  test("evaluates arithmetic with correct precedence", () => {
    const { result } = runCollect("fn main() { return 2 + 3 * 4; }");
    assert.equal(result, 14);
  });

  test("evaluates comparisons and booleans", () => {
    const { result } = runCollect("fn main() { return 3 < 4 && 5 > 4; }");
    assert.equal(result, true);
  });

  test("while loop mutates state via assignment", () => {
    const { output } = runCollect(`
      fn main() {
        let i = 0;
        while (i < 3) { print(i); i = i + 1; }
      }
    `);
    assert.deepEqual(output, ["0", "1", "2"]);
  });

  test("if/else picks the right branch", () => {
    const { output } = runCollect(`
      fn main() {
        let x = 5;
        if (x > 10) { print("big"); } else { print("small"); }
      }
    `);
    assert.deepEqual(output, ["small"]);
  });

  test("functions can call other functions and return values", () => {
    const { result } = runCollect(`
      fn double(x: i32) -> i32 { return x * 2; }
      fn main() { return double(21); }
    `);
    assert.equal(result, 42);
  });

  test("arrays support literals and indexing", () => {
    const { result } = runCollect(`
      fn main() {
        let a = [10, 20, 30];
        return a[1];
      }
    `);
    assert.equal(result, 20);
  });

  test("throws a KestrelError on out-of-bounds array access", () => {
    assert.throws(
      () => Kestrel.run(`
        fn main() {
          let a = [1, 2, 3];
          return a[5];
        }
      `),
      /out of bounds/
    );
  });

  test("throws a KestrelError on negative index", () => {
    assert.throws(
      () => Kestrel.run(`
        fn main() {
          let a = [1, 2, 3];
          return a[-1];
        }
      `),
      /out of bounds/
    );
  });

  test("throws a KestrelError when main is missing", () => {
    assert.throws(
      () => Kestrel.run("fn helper() { return 1; }"),
      /No 'main' function found/
    );
  });

  test("throws a KestrelError on unknown identifier", () => {
    assert.throws(
      () => Kestrel.run("fn main() { return unknown_var; }"),
      /Unknown identifier/
    );
  });

  test("throws a KestrelError on unknown function call", () => {
    assert.throws(
      () => Kestrel.run("fn main() { return unknown_fn(); }"),
      /Unknown function/
    );
  });

  test("throws a KestrelError on assignment to an undeclared variable", () => {
    assert.throws(
      () => Kestrel.run("fn main() { x = 5; }"),
      /Assignment to unknown variable/
    );
  });

  test("recursion works via ordinary function calls", () => {
    const { result } = runCollect(`
      fn fib(n: i32) -> i32 {
        if (n < 2) { return n; }
        return fib(n - 1) + fib(n - 2);
      }
      fn main() { return fib(10); }
    `);
    assert.equal(result, 55);
  });

  test("string concatenation-like print joins args with spaces", () => {
    const { output } = runCollect(`fn main() { print("a", 1, true); }`);
    assert.deepEqual(output, ["a 1 true"]);
  });
});

describe("parallel_map()", () => {
  test("applies a pure function to every array element, in order", () => {
    const { output } = runCollect(`
      pure fn square(x: i32) -> i32 { return x * x; }
      fn main() {
        let nums = [1, 2, 3, 4, 5];
        let squares = parallel_map(square, nums);
        print(squares[0], squares[1], squares[2], squares[3], squares[4]);
      }
    `);
    assert.deepEqual(output, ["1 4 9 16 25"]);
  });

  test("rejects a non-pure function", () => {
    assert.throws(
      () => Kestrel.run(`
        fn notpure(x: i32) -> i32 { print(x); return x; }
        fn main() { let a = [1, 2]; let b = parallel_map(notpure, a); print(b[0]); }
      `),
      /must be a 'pure fn'/
    );
  });

  test("rejects a function that doesn't take exactly one parameter", () => {
    assert.throws(
      () => Kestrel.run(`
        pure fn add(x: i32, y: i32) -> i32 { return x + y; }
        fn main() { let a = [1, 2]; let b = parallel_map(add, a); print(b[0]); }
      `),
      /exactly one parameter/
    );
  });

  test("rejects a function whose parameter is an array, not a scalar", () => {
    assert.throws(
      () => Kestrel.run(`
        pure fn sum3(a: [i32; N]) -> i32 { return a[0] + a[1] + a[2]; }
        fn main() { let a = [1, 2, 3]; let b = parallel_map(sum3, a); print(b[0]); }
      `),
      /must be a scalar/
    );
  });

  test("rejects an unknown function name", () => {
    assert.throws(
      () => Kestrel.run(`fn main() { let a = [1, 2]; let b = parallel_map(nosuchfn, a); print(b[0]); }`),
      /unknown function/
    );
  });

  test("rejects a non-identifier first argument", () => {
    assert.throws(
      () => Kestrel.run(`
        pure fn square(x: i32) -> i32 { return x * x; }
        fn main() { let a = [1, 2]; let b = parallel_map(square(1), a); print(b[0]); }
      `),
      /bare function name/
    );
  });

  test("is allowed inside a pure function too", () => {
    const { result } = runCollect(`
      pure fn square(x: i32) -> i32 { return x * x; }
      pure fn sum_of_squares(nums: [i32; N]) -> i32 {
        let squares = parallel_map(square, nums);
        return squares[0] + squares[1] + squares[2];
      }
      fn main() {
        let nums = [1, 2, 3];
        return sum_of_squares(nums);
      }
    `);
    assert.equal(result, 14);
  });
});

describe("example programs", () => {
  const fs = require("node:fs");
  const path = require("node:path");
  const readExample = (name) =>
    fs.readFileSync(path.join(__dirname, "../examples", name), "utf8");

  test("fibonacci.kes prints fib(0..9)", () => {
    const { output } = runCollect(readExample("fibonacci.kes"));
    assert.deepEqual(output, [
      "fib 0 = 0", "fib 1 = 1", "fib 2 = 1", "fib 3 = 2", "fib 4 = 3",
      "fib 5 = 5", "fib 6 = 8", "fib 7 = 13", "fib 8 = 21", "fib 9 = 34",
    ]);
  });

  test("purity_violation.kes fails the purity check as intended", () => {
    assert.throws(
      () => Kestrel.run(readExample("purity_violation.kes")),
      /'oops' is marked pure/
    );
  });
});

describe("checkTypes() — first honest type checker", () => {
  test("rejects mixing a number and a boolean with an arithmetic operator", () => {
    assert.throws(() => Kestrel.run(`fn main() { print(5 + true); }`), /needs two numbers/);
  });

  test("rejects '!' applied to a number", () => {
    assert.throws(() => Kestrel.run(`fn main() { print(!5); }`), /'!' needs a boolean/);
  });

  test("rejects '&&'/'||' applied to numbers", () => {
    assert.throws(() => Kestrel.run(`fn main() { print(1 && 2); }`), /needs two booleans/);
  });

  test("rejects a numeric literal used directly as an if-condition", () => {
    assert.throws(() => Kestrel.run(`fn main() { if (5) { print(1); } }`), /if-condition must be a boolean/);
  });

  test("rejects a numeric literal used directly as a while-condition", () => {
    assert.throws(() => Kestrel.run(`fn main() { while (0) { print(1); } }`), /while-condition must be a boolean/);
  });

  test("rejects a function call with the wrong number of arguments", () => {
    assert.throws(
      () => Kestrel.run(`
        fn add(x: i32, y: i32) -> i32 { return x + y; }
        fn main() { print(add(1, 2, 3)); }
      `),
      /'add' expects 2 arguments, got 3/
    );
  });

  test("rejects reassigning a variable to a different kind than its first binding", () => {
    assert.throws(() => Kestrel.run(`fn main() { let x = 5; x = true; }`), /was first bound as int/);
  });

  test("rejects comparing a number and a boolean with =='", () => {
    assert.throws(() => Kestrel.run(`fn main() { print(5 == true); }`), /compares mismatched types/);
  });

  test("does not flag a legitimate program (no false positives)", () => {
    const { output } = runCollect(`
      pure fn square(x: i32) -> i32 { return x * x; }
      fn main() {
        let nums = [1, 2, 3];
        let i = 0;
        while (i < 3) {
          print(square(nums[i]));
          i = i + 1;
        }
        if (i == 3) { print("done"); }
      }
    `);
    assert.deepEqual(output, ["1", "4", "9", "done"]);
  });

  test("does not flag a boolean-returning function call used as a condition (unknown kind, no guess)", () => {
    const { output } = runCollect(`
      fn is_even(x: i32) -> bool { return x % 2 == 0; }
      fn main() {
        if (is_even(4)) { print("even"); }
      }
    `);
    assert.deepEqual(output, ["even"]);
  });

  test("does not flag a function parameter's kind (unknown until called)", () => {
    const { output } = runCollect(`
      fn double(x: i32) -> i32 { return x * 2; }
      fn main() { print(double(21)); }
    `);
    assert.deepEqual(output, ["42"]);
  });
});

describe("purity/type errors carry a line number", () => {
  test("purity violation reports the line of the offending print", () => {
    assert.throws(
      () => Kestrel.run(`
        pure fn oops() -> i32 {
          print("hi");
          return 1;
        }
        fn main() { oops(); }
      `),
      /is marked pure but calls print or an impure function \(line 3\)/
    );
  });

  test("a type error reports the line of the offending statement, not just the function", () => {
    assert.throws(
      () => Kestrel.run(`
        fn main() {
          let x = 1;

          print(5 + true);
        }
      `),
      /needs two numbers, found int and bool \(line 5\)/
    );
  });
});

describe("pure fn memoization", () => {
  // A `pure fn` cannot observe or be affected by any other call to itself
  // (no I/O, no calls to impure fns, no mutation outside its own locals),
  // so caching its result by argument value is always safe. This checks
  // the correctness fix for a specific hazard: JSON.stringify(NaN) and
  // JSON.stringify(null) are both "null", and Kestrel can produce a real
  // `null` at runtime via a bare `return;` even with a declared return
  // type — so two calls with genuinely different arguments (one null,
  // one NaN) must not collide on the same memo cache key.
  test("a null-returning call and a NaN-argument call don't collide on the same cache key", () => {
    const { output } = runCollect(`
      pure fn maybe(x: i32) -> i32 {
        if (x > 0) { return x; }
        return;
      }
      pure fn tag(x: i32) -> i32 {
        if (x == x) { return 100; } else { return 200; }
      }
      fn main() {
        let a = maybe(-1);
        let b = 0 / 0;
        print(tag(a));
        print(tag(b));
      }
    `);
    assert.deepEqual(output, ["100", "200"]);
  });

  test("an impure function is never memoized: identical calls still both run", () => {
    const { output } = runCollect(`
      fn noisy(x: i32) -> i32 {
        print("called", x);
        return x * 2;
      }
      fn main() {
        print(noisy(5));
        print(noisy(5));
      }
    `);
    assert.deepEqual(output, ["called 5", "10", "called 5", "10"]);
  });

  test("a pure fn's side-effect-free body still returns correct values across repeated identical calls", () => {
    const { output } = runCollect(`
      pure fn square(x: i32) -> i32 { return x * x; }
      fn main() {
        print(square(4));
        print(square(4));
        print(square(5));
      }
    `);
    assert.deepEqual(output, ["16", "16", "25"]);
  });
});
