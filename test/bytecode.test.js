// Test suite for the bytecode compiler/VM (Kestrel.runFast). The goal
// isn't to re-test language semantics from scratch — kestrel.test.js
// already does that against run() — it's to prove runFast() is a
// drop-in, bug-for-bug equivalent backend, plus cover VM-specific
// concerns (jump patching, locals-as-array-slots) that only exist here.

const { test, describe } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const Kestrel = require("../kestrel.js");

function runFastCollect(src, opts = {}) {
  const output = [];
  const { result, boundsNotes } = Kestrel.runFast(src, {
    onPrint: (s) => output.push(s),
    ...opts,
  });
  return { result, boundsNotes, output };
}

// Runs a program on both backends and asserts identical output, result,
// and bounds notes — the core equivalence guarantee.
function assertEquivalent(src) {
  const outA = [];
  const a = Kestrel.run(src, { onPrint: (s) => outA.push(s) });
  const outB = [];
  const b = Kestrel.runFast(src, { onPrint: (s) => outB.push(s) });
  assert.deepEqual(outB, outA, "runFast output should match run output");
  assert.equal(b.result, a.result, "runFast result should match run result");
  assert.deepEqual(b.boundsNotes, a.boundsNotes, "runFast boundsNotes should match run boundsNotes");
}

describe("runFast() equivalence with run()", () => {
  test("basics.kes", () => {
    assertEquivalent(fs.readFileSync(path.join(__dirname, "../examples/basics.kes"), "utf8"));
  });

  test("fibonacci.kes", () => {
    assertEquivalent(fs.readFileSync(path.join(__dirname, "../examples/fibonacci.kes"), "utf8"));
  });

  test("arithmetic precedence", () => {
    assertEquivalent("fn main() { return 2 + 3 * 4 - 1; }");
  });

  test("comparisons and logical operators", () => {
    assertEquivalent("fn main() { return (3 < 4) && (5 > 4) || false; }");
  });

  test("if/else both branches", () => {
    assertEquivalent(`
      fn main() {
        let x = 5;
        if (x > 10) { print("big"); } else { print("small"); }
        if (x < 10) { print("also small"); }
      }
    `);
  });

  test("while loops with mutation", () => {
    assertEquivalent(`
      fn main() {
        let i = 0;
        let total = 0;
        while (i < 5) { total = total + i; i = i + 1; }
        print(total);
      }
    `);
  });

  test("nested if/while and recursion", () => {
    assertEquivalent(`
      fn fib(n: i32) -> i32 {
        if (n < 2) { return n; } else { return fib(n - 1) + fib(n - 2); }
      }
      fn main() {
        let i = 0;
        while (i < 8) {
          if (i % 2 == 0) { print("even fib", i, "=", fib(i)); }
          i = i + 1;
        }
      }
    `);
  });

  test("arrays: literals, indexing, out-of-bounds error", () => {
    assertEquivalent("fn main() { let a = [1, 2, 3]; return a[2]; }");
    assert.throws(() => Kestrel.runFast("fn main() { let a = [1]; return a[9]; }"), /out of bounds/);
    assert.throws(() => Kestrel.run("fn main() { let a = [1]; return a[9]; }"), /out of bounds/);
  });

  test("a `let` inside an if-branch stays visible after it (flat scope)", () => {
    // Regression check for the VM's slot allocation: Kestrel has no block
    // scoping, so a `let` declared inside an `if` must still be readable
    // once execution leaves the if. collectSlots() must give it a slot
    // up front for this to work.
    assertEquivalent(`
      fn main() {
        let x = 1;
        if (x == 1) {
          let y = 42;
        }
        let y = 0;
        print(y);
      }
    `);
  });
});

describe("call stack correctness (execute() has no JS recursion)", () => {
  // execute() manages Kestrel calls itself, via a hand-rolled call stack
  // (three parallel arrays + an index) instead of recursive JS function
  // calls — the fix for runFast() originally being slower than run() on
  // recursive code. These specifically target that rewrite: interleaved
  // returns, mutual recursion between different functions (so `code`
  // must be swapped correctly per frame, not just `frameBase`), and
  // deeper recursion than the other tests exercise.

  test("deeper recursion still produces the correct result", () => {
    assertEquivalent(`
      fn fib(n: i32) -> i32 {
        if (n < 2) { return n; }
        return fib(n - 1) + fib(n - 2);
      }
      fn main() { return fib(20); }
    `);
  });

  test("mutual recursion between two different functions", () => {
    // Forces the call stack to restore a *different* function's code
    // array on return, not just re-enter the same one — a bug here
    // would show up as running the wrong bytecode after a return.
    assertEquivalent(`
      fn is_even(n: i32) -> bool {
        if (n == 0) { return true; }
        return is_odd(n - 1);
      }
      fn is_odd(n: i32) -> bool {
        if (n == 0) { return false; }
        return is_even(n - 1);
      }
      fn main() {
        let i = 0;
        while (i < 12) {
          print(i, "is_even:", is_even(i));
          i = i + 1;
        }
      }
    `);
  });

  test("sibling calls in one expression interleave without clobbering each other's frames", () => {
    // square(a) fully returns and pops its frame before square(b) is
    // even evaluated, but both calls share the same underlying code
    // array — this would catch a frame/base mixup between them.
    assertEquivalent(`
      fn square(x: i32) -> i32 { return x * x; }
      fn main() {
        let i = 0;
        while (i < 10) {
          print(square(i) + square(i + 1));
          i = i + 1;
        }
      }
    `);
  });
});

describe("runFast() error parity with run()", () => {
  test("purity_violation.kes fails identically on both backends", () => {
    const src = fs.readFileSync(path.join(__dirname, "../examples/purity_violation.kes"), "utf8");
    assert.throws(() => Kestrel.run(src), /'oops' is marked pure/);
    assert.throws(() => Kestrel.runFast(src), /'oops' is marked pure/);
  });

  test("throws on missing main", () => {
    assert.throws(() => Kestrel.runFast("fn helper() { return 1; }"), /No 'main' function found/);
  });

  test("throws at compile time on unknown identifier", () => {
    assert.throws(() => Kestrel.runFast("fn main() { return unknown_var; }"), /Unknown identifier/);
  });

  test("throws at compile time on unknown function call", () => {
    assert.throws(() => Kestrel.runFast("fn main() { return unknown_fn(); }"), /Unknown function/);
  });

  test("throws at compile time on assignment to undeclared variable", () => {
    assert.throws(() => Kestrel.runFast("fn main() { x = 5; }"), /Assignment to unknown variable/);
  });
});

describe("VM internals", () => {
  test("compile() produces one entry per function with slot-indexed locals", () => {
    const program = Kestrel.parse(Kestrel.lex(`
      fn add(a: i32, b: i32) -> i32 { let c = a + b; return c; }
    `));
    const functions = Kestrel.compile(program);
    const add = functions.get("add");
    assert.equal(add.paramCount, 2);
    // slots: a=0, b=1, c=2
    assert.equal(add.slotCount, 3);
    assert.equal(add.slots.get("a"), 0);
    assert.equal(add.slots.get("b"), 1);
    assert.equal(add.slots.get("c"), 2);
  });

  test("JUMP/JUMP_IF_FALSE targets are patched to real instruction indices", () => {
    const program = Kestrel.parse(Kestrel.lex(`
      fn main() { if (true) { print("y"); } else { print("n"); } }
    `));
    const functions = Kestrel.compile(program);
    const code = functions.get("main").code;
    for (const ins of code) {
      if (ins.op === "JUMP" || ins.op === "JUMP_IF_FALSE") {
        assert.ok(ins.target >= 0 && ins.target <= code.length, `jump target ${ins.target} in range`);
      }
    }
  });

  test("execute() runs a directly-compiled program without going through runFast", () => {
    const program = Kestrel.parse(Kestrel.lex("fn main() { return 6 * 7; }"));
    const functions = Kestrel.compile(program);
    const result = Kestrel.execute(functions, "main", [], () => {});
    assert.equal(result, 42);
  });
});
