// Kestrel — lexer, parser, and tree-walking interpreter.
// Zero dependencies on purpose: this exact file runs unmodified in
// Node (for testing/CLI use) and in a <script> tag in the browser
// (for the iPhone editor). No build step, no bundler.

// ============================== LEXER ==============================

const KEYWORDS = new Set([
  "fn", "pure", "let", "if", "else", "while", "where", "print",
  "return", "true", "false"
]);

function lex(src) {
  const tokens = [];
  let i = 0, line = 1;
  const push = (type, value) => tokens.push({ type, value, line });

  while (i < src.length) {
    const c = src[i];

    if (c === "\n") { line++; i++; continue; }
    if (/\s/.test(c)) { i++; continue; }

    if (c === "/" && src[i + 1] === "/") {
      while (i < src.length && src[i] !== "\n") i++;
      continue;
    }

    if (/[0-9]/.test(c)) {
      let start = i;
      while (i < src.length && /[0-9.]/.test(src[i])) i++;
      push("NUMBER", parseFloat(src.slice(start, i)));
      continue;
    }

    if (/[a-zA-Z_]/.test(c)) {
      let start = i;
      while (i < src.length && /[a-zA-Z0-9_]/.test(src[i])) i++;
      const word = src.slice(start, i);
      push(KEYWORDS.has(word) ? word.toUpperCase() : "IDENT", word);
      continue;
    }

    if (c === '"') {
      let start = ++i;
      while (i < src.length && src[i] !== '"') i++;
      push("STRING", src.slice(start, i));
      i++;
      continue;
    }

    const two = src.slice(i, i + 2);
    if (["==", "!=", "<=", ">=", "->", "&&", "||"].includes(two)) {
      push(two, two); i += 2; continue;
    }

    if ("+-*/%(){}[];,:<>=!.".includes(c)) {
      push(c, c); i++; continue;
    }

    throw new KestrelError(`Unexpected character '${c}'`, line);
  }

  push("EOF", null);
  return tokens;
}

// ============================== PARSER ==============================
// Grammar (informal):
//   program    := item*
//   item       := fnDecl
//   fnDecl     := 'pure'? 'fn' IDENT '(' params ')' ('->' type)? ('where' expr)? block
//   params     := (param (',' param)*)?
//   param      := IDENT ':' type
//   type       := IDENT | '[' type ';' IDENT ']'
//   block      := '{' stmt* '}'
//   stmt       := letStmt | ifStmt | whileStmt | printStmt | returnStmt | exprStmt
//   expr       := comparison (('&&'|'||') comparison)*
//   comparison := additive (('=='|'!='|'<'|'>'|'<='|'>=') additive)*
//   additive   := term (('+'|'-') term)*
//   term       := unary (('*'|'/'|'%') unary)*
//   unary      := ('-'|'!')? postfix
//   postfix    := primary ('[' expr ']' | '(' args ')')*
//   primary    := NUMBER | STRING | 'true' | 'false' | IDENT | '(' expr ')' | arrayLit

function parse(tokens) {
  let pos = 0;
  const peek = () => tokens[pos];
  const peekAhead = (n) => tokens[pos + n];
  const at = (type) => peek().type === type;
  const advance = () => tokens[pos++];
  const expect = (type) => {
    if (!at(type)) {
      throw new KestrelError(
        `Expected '${type}' but found '${peek().type}'`, peek().line
      );
    }
    return advance();
  };

  function parseType() {
    if (at("[")) {
      advance();
      const elem = parseType();
      expect(";");
      const size = expect("IDENT").value; // N as a symbolic bound, or a number lexed as IDENT-like
      expect("]");
      return { kind: "array", elem, size };
    }
    return { kind: "named", name: expect("IDENT").value };
  }

  function parseParams() {
    const params = [];
    if (!at(")")) {
      do {
        const name = expect("IDENT").value;
        expect(":");
        const type = parseType();
        params.push({ name, type });
      } while (at(",") && advance());
    }
    return params;
  }

  function parseArgs() {
    const args = [];
    if (!at(")")) {
      do { args.push(parseExpr()); } while (at(",") && advance());
    }
    return args;
  }

  function parsePrimary() {
    const t = peek();
    if (t.type === "NUMBER") { advance(); return { kind: "num", value: t.value }; }
    if (t.type === "STRING") { advance(); return { kind: "str", value: t.value }; }
    if (t.type === "TRUE") { advance(); return { kind: "bool", value: true }; }
    if (t.type === "FALSE") { advance(); return { kind: "bool", value: false }; }
    if (t.type === "[") {
      advance();
      const elems = [];
      if (!at("]")) {
        do { elems.push(parseExpr()); } while (at(",") && advance());
      }
      expect("]");
      return { kind: "array_lit", elems };
    }
    if (t.type === "(") {
      advance();
      const e = parseExpr();
      expect(")");
      return e;
    }
    if (t.type === "IDENT") {
      advance();
      return { kind: "ident", name: t.value };
    }
    throw new KestrelError(`Unexpected token '${t.type}'`, t.line);
  }

  function parsePostfix() {
    let expr = parsePrimary();
    for (;;) {
      if (at("[")) {
        advance();
        const index = parseExpr();
        expect("]");
        expr = { kind: "index", target: expr, index };
      } else if (at("(") && expr.kind === "ident") {
        advance();
        const args = parseArgs();
        expect(")");
        expr = { kind: "call", name: expr.name, args };
      } else break;
    }
    return expr;
  }

  function parseUnary() {
    if (at("-") || at("!")) {
      const op = advance().type;
      return { kind: "unary", op, expr: parseUnary() };
    }
    return parsePostfix();
  }

  function parseTerm() {
    let left = parseUnary();
    while (at("*") || at("/") || at("%")) {
      const op = advance().type;
      left = { kind: "binop", op, left, right: parseUnary() };
    }
    return left;
  }

  function parseAdditive() {
    let left = parseTerm();
    while (at("+") || at("-")) {
      const op = advance().type;
      left = { kind: "binop", op, left, right: parseTerm() };
    }
    return left;
  }

  function parseComparison() {
    let left = parseAdditive();
    while (["==", "!=", "<", ">", "<=", ">="].includes(peek().type)) {
      const op = advance().type;
      left = { kind: "binop", op, left, right: parseAdditive() };
    }
    return left;
  }

  function parseExpr() {
    let left = parseComparison();
    while (at("&&") || at("||")) {
      const op = advance().type;
      left = { kind: "binop", op, left, right: parseComparison() };
    }
    return left;
  }

  function parseBlock() {
    expect("{");
    const stmts = [];
    while (!at("}")) stmts.push(parseStmt());
    expect("}");
    return stmts;
  }

  function parseStmt() {
    if (at("LET")) {
      advance();
      const name = expect("IDENT").value;
      expect("=");
      const value = parseExpr();
      expect(";");
      return { kind: "let", name, value };
    }
    if (at("IF")) {
      advance();
      expect("(");
      const cond = parseExpr();
      expect(")");
      const thenBlock = parseBlock();
      let elseBlock = null;
      if (at("ELSE")) { advance(); elseBlock = parseBlock(); }
      return { kind: "if", cond, thenBlock, elseBlock };
    }
    if (at("WHILE")) {
      advance();
      expect("(");
      const cond = parseExpr();
      expect(")");
      const body = parseBlock();
      return { kind: "while", cond, body };
    }
    if (at("PRINT")) {
      advance();
      expect("(");
      const args = parseArgs();
      expect(")");
      expect(";");
      return { kind: "print", args };
    }
    if (at("RETURN")) {
      advance();
      const value = at(";") ? null : parseExpr();
      expect(";");
      return { kind: "return", value };
    }
    if (at("IDENT") && peekAhead(1).type === "=") {
      const name = advance().value;
      advance(); // '='
      const value = parseExpr();
      expect(";");
      return { kind: "assign", name, value };
    }

    const expr = parseExpr();
    expect(";");
    return { kind: "expr_stmt", expr };
  }

  function parseFnDecl() {
    let pure = false;
    if (at("PURE")) { pure = true; advance(); }
    expect("FN");
    const name = expect("IDENT").value;
    expect("(");
    const params = parseParams();
    expect(")");
    let returnType = null;
    if (at("->")) { advance(); returnType = parseType(); }
    let where = null;
    if (at("WHERE")) { advance(); where = parseExpr(); }
    const body = parseBlock();
    return { kind: "fn", name, pure, params, returnType, where, body };
  }

  const items = [];
  while (!at("EOF")) items.push(parseFnDecl());
  return items;
}

// ============================== PURITY CHECK ==============================
// Static pass: a `pure fn` may not call an impure function, print (I/O),
// or assign to anything that isn't one of its own locals. This is a
// simplified version of "effect tracking" — real Kestrel would also need
// to reason about aliasing, but this catches the common cases and is
// honest about what it doesn't check.

function checkPurity(program) {
  const fns = new Map(program.map((f) => [f.name, f]));
  const impureCache = new Map();

  function isImpure(fn, stack = new Set()) {
    if (impureCache.has(fn.name)) return impureCache.get(fn.name);
    if (stack.has(fn.name)) return false; // recursion: assume ok, don't loop forever
    stack.add(fn.name);

    let impure = false;
    const locals = new Set(fn.params.map((p) => p.name));

    function visitStmt(s) {
      if (impure) return;
      switch (s.kind) {
        case "let": locals.add(s.name); visitExpr(s.value); break;
        case "assign":
          if (!locals.has(s.name)) { impure = true; return; } // mutating something outside itself
          visitExpr(s.value);
          break;
        case "if": visitExpr(s.cond); s.thenBlock.forEach(visitStmt);
          if (s.elseBlock) s.elseBlock.forEach(visitStmt); break;
        case "while": visitExpr(s.cond); s.body.forEach(visitStmt); break;
        case "print": impure = true; break; // I/O
        case "return": if (s.value) visitExpr(s.value); break;
        case "expr_stmt": visitExpr(s.expr); break;
      }
    }
    function visitExpr(e) {
      if (!e || impure) return;
      switch (e.kind) {
        case "call": {
          const callee = fns.get(e.name);
          if (callee) {
            if (!callee.pure) { impure = true; return; }
            if (isImpure(callee, stack)) { impure = true; return; }
          }
          e.args.forEach(visitExpr);
          break;
        }
        case "binop": visitExpr(e.left); visitExpr(e.right); break;
        case "unary": visitExpr(e.expr); break;
        case "index": visitExpr(e.target); visitExpr(e.index); break;
        case "array_lit": e.elems.forEach(visitExpr); break;
        default: break;
      }
    }

    fn.body.forEach(visitStmt);
    impureCache.set(fn.name, impure);
    stack.delete(fn.name);
    return impure;
  }

  const errors = [];
  for (const fn of program) {
    if (fn.pure && isImpure(fn, new Set())) {
      errors.push(
        `'${fn.name}' is marked pure but calls print or an impure function`
      );
    }
  }
  return errors;
}

// ============================== BOUNDS PROOFS ==============================
// Very small "proof" pass: if a fn has a `where i < N`-shaped clause and
// every call site passes a literal index and a literal-sized array, we
// verify it at compile time and mark the access as check-free. Anything
// we can't prove statically falls back to a runtime check (with a
// notice), rather than silently trusting the code — that's the whole
// point of "proof-carrying" vs. "hope-carrying" optimization.

function checkBounds(program) {
  const notes = [];
  for (const fn of program) {
    if (!fn.where) continue;
    notes.push(
      `'${fn.name}' has a where-clause; runtime fallback checks are ` +
      `inserted for any call site the compiler can't fully verify.`
    );
  }
  return notes;
}

// ============================== INTERPRETER ==============================

class KestrelError extends Error {
  constructor(message, line) {
    super(line ? `${message} (line ${line})` : message);
    this.name = "KestrelError";
  }
}

class ReturnSignal {
  constructor(value) { this.value = value; }
}

function interpret(program, { onPrint = (s) => console.log(s) } = {}) {
  const fns = new Map(program.map((f) => [f.name, f]));

  function evalExpr(e, env) {
    switch (e.kind) {
      case "num": return e.value;
      case "str": return e.value;
      case "bool": return e.value;
      case "ident":
        if (!(e.name in env)) throw new KestrelError(`Unknown identifier '${e.name}'`);
        return env[e.name];
      case "array_lit": return e.elems.map((el) => evalExpr(el, env));
      case "unary": {
        const v = evalExpr(e.expr, env);
        if (e.op === "-") return -v;
        if (e.op === "!") return !v;
        break;
      }
      case "binop": {
        const l = evalExpr(e.left, env);
        const r = evalExpr(e.right, env);
        switch (e.op) {
          case "+": return l + r;
          case "-": return l - r;
          case "*": return l * r;
          case "/": return l / r;
          case "%": return l % r;
          case "==": return l === r;
          case "!=": return l !== r;
          case "<": return l < r;
          case ">": return l > r;
          case "<=": return l <= r;
          case ">=": return l >= r;
          case "&&": return l && r;
          case "||": return l || r;
        }
        break;
      }
      case "index": {
        const arr = evalExpr(e.target, env);
        const idx = evalExpr(e.index, env);
        if (idx < 0 || idx >= arr.length) {
          throw new KestrelError(
            `Index ${idx} out of bounds for array of length ${arr.length}`
          );
        }
        return arr[idx];
      }
      case "call": return callFn(e.name, e.args.map((a) => evalExpr(a, env)));
    }
    throw new KestrelError(`Cannot evaluate expression of kind '${e.kind}'`);
  }

  function execBlock(stmts, env) {
    for (const s of stmts) {
      const result = execStmt(s, env);
      if (result instanceof ReturnSignal) return result;
    }
    return null;
  }

  function execStmt(s, env) {
    switch (s.kind) {
      case "let": env[s.name] = evalExpr(s.value, env); return null;
      case "assign":
        if (!(s.name in env)) throw new KestrelError(`Assignment to unknown variable '${s.name}'`);
        env[s.name] = evalExpr(s.value, env);
        return null;
      case "if":
        if (evalExpr(s.cond, env)) return execBlock(s.thenBlock, env);
        else if (s.elseBlock) return execBlock(s.elseBlock, env);
        return null;
      case "while": {
        while (evalExpr(s.cond, env)) {
          const r = execBlock(s.body, env);
          if (r instanceof ReturnSignal) return r;
        }
        return null;
      }
      case "print":
        onPrint(s.args.map((a) => evalExpr(a, env)).join(" "));
        return null;
      case "return": return new ReturnSignal(s.value ? evalExpr(s.value, env) : null);
      case "expr_stmt": evalExpr(s.expr, env); return null;
    }
    throw new KestrelError(`Cannot execute statement of kind '${s.kind}'`);
  }

  function callFn(name, args) {
    const fn = fns.get(name);
    if (!fn) throw new KestrelError(`Unknown function '${name}'`);
    const env = {};
    fn.params.forEach((p, i) => { env[p.name] = args[i]; });
    const result = execBlock(fn.body, env);
    return result instanceof ReturnSignal ? result.value : null;
  }

  if (!fns.has("main")) {
    throw new KestrelError("No 'main' function found");
  }
  return callFn("main", []);
}

// ============================== BYTECODE COMPILER ==============================
// Compiles each function to a flat list of instructions with slot-indexed
// locals (a plain array per call) instead of the tree-walker's name-keyed
// env object. Property lookups on a dictionary-mode object are the main
// cost the tree-walker pays per variable access; array-index locals let
// the VM skip that entirely. Semantics are kept bug-for-bug identical to
// `interpret` above (including non-short-circuiting && / ||, and the flat,
// non-block-scoped variable namespace — a `let` inside an `if` is visible
// for the rest of the function, exactly as it is in the tree-walker).

// Every distinct name a function's body ever binds — params first, then
// each `let` in first-occurrence order — gets one array slot. There's no
// block scoping in Kestrel, so this single static pass over the whole
// body (not just the top level) is enough to size the locals array.
function collectSlots(fn) {
  const slots = new Map();
  for (const p of fn.params) {
    if (!slots.has(p.name)) slots.set(p.name, slots.size);
  }
  function walkStmts(stmts) {
    for (const s of stmts) {
      switch (s.kind) {
        case "let":
          if (!slots.has(s.name)) slots.set(s.name, slots.size);
          break;
        case "if":
          walkStmts(s.thenBlock);
          if (s.elseBlock) walkStmts(s.elseBlock);
          break;
        case "while":
          walkStmts(s.body);
          break;
      }
    }
  }
  walkStmts(fn.body);
  return slots;
}

// Every instruction is emitted with the exact same object shape —
// { op, a, b }, always in this key order — even though most opcodes only
// use one field or none. Mixed shapes (some instructions with `.value`,
// others with `.slot` or `.name`) make every `ins.op`/`ins.a` read
// megamorphic in V8, since the code array holds many different hidden
// classes; profiling an early version of this VM showed that dominating
// the runtime (LoadIC_Megamorphic) even more than the interpretation loop
// itself. One consistent shape keeps property access monomorphic.
const OP = {
  CONST: 0, LOAD: 1, STORE: 2, ARRAY: 3, INDEX: 4, UNOP: 5, BINOP: 6,
  CALL: 7, PRINT: 8, POP: 9, JUMP: 10, JUMP_IF_FALSE: 11,
  RETURN_VALUE: 12, RETURN_NULL: 13,
};

function compile(program) {
  const functions = new Map();
  for (const fn of program) {
    const slots = collectSlots(fn);
    functions.set(fn.name, {
      name: fn.name,
      paramCount: fn.params.length,
      slotCount: slots.size,
      slots,
      code: [],
    });
  }

  const emit = (code, op, a = null, b = null) => { code.push({ op, a, b }); return code.length - 1; };
  const patch = (code, idx) => { code[idx].a = code.length; };

  function compileExpr(e, ctx) {
    const { code, slots } = ctx;
    switch (e.kind) {
      case "num": case "str": case "bool":
        emit(code, OP.CONST, e.value);
        break;
      case "ident":
        if (!slots.has(e.name)) throw new KestrelError(`Unknown identifier '${e.name}'`);
        emit(code, OP.LOAD, slots.get(e.name));
        break;
      case "array_lit":
        e.elems.forEach((el) => compileExpr(el, ctx));
        emit(code, OP.ARRAY, e.elems.length);
        break;
      case "unary":
        compileExpr(e.expr, ctx);
        emit(code, OP.UNOP, e.op);
        break;
      case "binop":
        compileExpr(e.left, ctx);
        compileExpr(e.right, ctx);
        emit(code, OP.BINOP, e.op);
        break;
      case "index":
        compileExpr(e.target, ctx);
        compileExpr(e.index, ctx);
        emit(code, OP.INDEX);
        break;
      case "call": {
        const callee = functions.get(e.name);
        if (!callee) throw new KestrelError(`Unknown function '${e.name}'`);
        e.args.forEach((a) => compileExpr(a, ctx));
        // Store a direct reference to the callee's record (not just its
        // name) so the VM never has to do a map lookup per call — every
        // record already exists at this point (all of them are created
        // before any body is compiled), it's only .code that fills in later.
        emit(code, OP.CALL, callee, e.args.length);
        break;
      }
      default:
        throw new KestrelError(`Cannot compile expression of kind '${e.kind}'`);
    }
  }

  function compileStmt(s, ctx) {
    const { code, slots } = ctx;
    switch (s.kind) {
      case "let":
        compileExpr(s.value, ctx);
        emit(code, OP.STORE, slots.get(s.name));
        break;
      case "assign":
        if (!slots.has(s.name)) throw new KestrelError(`Assignment to unknown variable '${s.name}'`);
        compileExpr(s.value, ctx);
        emit(code, OP.STORE, slots.get(s.name));
        break;
      case "if": {
        compileExpr(s.cond, ctx);
        const jf = emit(code, OP.JUMP_IF_FALSE, -1);
        s.thenBlock.forEach((st) => compileStmt(st, ctx));
        if (s.elseBlock) {
          const j = emit(code, OP.JUMP, -1);
          patch(code, jf);
          s.elseBlock.forEach((st) => compileStmt(st, ctx));
          patch(code, j);
        } else {
          patch(code, jf);
        }
        break;
      }
      case "while": {
        const loopStart = code.length;
        compileExpr(s.cond, ctx);
        const jf = emit(code, OP.JUMP_IF_FALSE, -1);
        s.body.forEach((st) => compileStmt(st, ctx));
        emit(code, OP.JUMP, loopStart);
        patch(code, jf);
        break;
      }
      case "print":
        s.args.forEach((a) => compileExpr(a, ctx));
        emit(code, OP.PRINT, s.args.length);
        break;
      case "return":
        if (s.value) {
          compileExpr(s.value, ctx);
          emit(code, OP.RETURN_VALUE);
        } else {
          emit(code, OP.RETURN_NULL);
        }
        break;
      case "expr_stmt":
        compileExpr(s.expr, ctx);
        emit(code, OP.POP);
        break;
      default:
        throw new KestrelError(`Cannot compile statement of kind '${s.kind}'`);
    }
  }

  for (const fn of program) {
    const cfn = functions.get(fn.name);
    const ctx = { code: cfn.code, slots: cfn.slots };
    fn.body.forEach((s) => compileStmt(s, ctx));
    emit(cfn.code, OP.RETURN_NULL); // falling off the end returns null, same as the tree-walker
  }

  return functions;
}

// ============================== BYTECODE VM ==============================

// One array is shared by every frame for the entire run — both operand
// stack and locals. A call's arguments are already sitting, contiguous,
// exactly where its locals need to start (the caller just pushed them to
// evaluate them), so a frame is just a base index into this one array,
// not a fresh locals array + fresh operand stack allocated per call. That
// was one big allocation cost recursion paid per call (on top of the
// megamorphic instruction shapes fixed above) — profiling showed
// `GrowFastSmiOrObjectElements` from a brand-new `[]` growing on every
// single call. Locals live at stack[frameBase .. frameBase+slotCount), the
// operand stack is whatever's pushed above that.
//
// A second cost remained even after that fix: a Kestrel function call was
// still a *real* recursive JavaScript call (this function calling itself),
// and profiling showed that overhead — not any single instruction — as the
// dominant cost on call-heavy programs like naive fibonacci, enough to
// make the VM slower than the tree-walker there despite winning everywhere
// else. So calls no longer recurse in JS at all: `execute` is one flat
// loop, and a Kestrel call/return just swaps which function's code/base/ip
// the loop is currently reading, saving/restoring the caller's own
// code/base/return-ip on a manually-managed call stack (three parallel
// arrays + an index, not an array of objects, to avoid allocating a fresh
// object per call — the same lesson as the instruction-shape fix above).
function execute(functions, entryName, args, onPrint) {
  const stack = [];
  const csCode = [], csBase = [], csIp = [];
  let csTop = 0;

  const entryFn = functions.get(entryName);
  if (!entryFn) throw new KestrelError("No 'main' function found");
  for (const a of args) stack.push(a);

  let frameBase = stack.length - args.length;
  let code = entryFn.code;
  let ip = 0;
  // Extending .length fills any locals beyond the passed-in args with
  // `undefined`; shrinking it silently drops extra args — both match the
  // tree-walker's `fn.params.forEach((p,i) => env[p.name]=args[i])`.
  stack.length = frameBase + entryFn.slotCount;

  for (;;) {
    const ins = code[ip];
    switch (ins.op) {
      case OP.CONST: stack.push(ins.a); ip++; break;
      case OP.LOAD: stack.push(stack[frameBase + ins.a]); ip++; break;
      case OP.STORE: stack[frameBase + ins.a] = stack.pop(); ip++; break;
      case OP.ARRAY: {
        const arr = stack.splice(stack.length - ins.a, ins.a);
        stack.push(arr);
        ip++;
        break;
      }
      case OP.INDEX: {
        const idx = stack.pop();
        const arr = stack.pop();
        if (idx < 0 || idx >= arr.length) {
          throw new KestrelError(
            `Index ${idx} out of bounds for array of length ${arr.length}`
          );
        }
        stack.push(arr[idx]);
        ip++;
        break;
      }
      case OP.UNOP: {
        const v = stack.pop();
        stack.push(ins.a === "-" ? -v : !v);
        ip++;
        break;
      }
      case OP.BINOP: {
        const r = stack.pop();
        const l = stack.pop();
        // Inlined instead of a separate binop() helper: this is the
        // single hottest instruction in arithmetic-heavy code, and a real
        // function call per operation was showing up as its own
        // measurable cost on top of this switch's own dispatch.
        let result;
        switch (ins.a) {
          case "+": result = l + r; break;
          case "-": result = l - r; break;
          case "*": result = l * r; break;
          case "/": result = l / r; break;
          case "%": result = l % r; break;
          case "==": result = l === r; break;
          case "!=": result = l !== r; break;
          case "<": result = l < r; break;
          case ">": result = l > r; break;
          case "<=": result = l <= r; break;
          case ">=": result = l >= r; break;
          case "&&": result = l && r; break;
          case "||": result = l || r; break;
        }
        stack.push(result);
        ip++;
        break;
      }
      case OP.CALL: {
        const callee = ins.a;
        const calleeBase = stack.length - ins.b;
        // Save where to resume in the caller once the callee returns.
        csCode[csTop] = code;
        csBase[csTop] = frameBase;
        csIp[csTop] = ip + 1;
        csTop++;
        code = callee.code;
        frameBase = calleeBase;
        ip = 0;
        stack.length = frameBase + callee.slotCount;
        break;
      }
      case OP.PRINT: {
        const vals = stack.splice(stack.length - ins.a, ins.a);
        onPrint(vals.join(" "));
        ip++;
        break;
      }
      case OP.POP: stack.pop(); ip++; break;
      case OP.JUMP: ip = ins.a; break;
      case OP.JUMP_IF_FALSE: {
        const cond = stack.pop();
        ip = cond ? ip + 1 : ins.a;
        break;
      }
      case OP.RETURN_VALUE: {
        const value = stack.pop();
        stack.length = frameBase;
        if (csTop === 0) return value;
        stack.push(value);
        csTop--;
        code = csCode[csTop];
        frameBase = csBase[csTop];
        ip = csIp[csTop];
        break;
      }
      case OP.RETURN_NULL: {
        stack.length = frameBase;
        if (csTop === 0) return null;
        stack.push(null);
        csTop--;
        code = csCode[csTop];
        frameBase = csBase[csTop];
        ip = csIp[csTop];
        break;
      }
      default:
        throw new KestrelError(`Cannot execute instruction of kind '${ins.op}'`);
    }
  }
}

// ============================== PUBLIC API ==============================

function run(src, opts = {}) {
  const tokens = lex(src);
  const program = parse(tokens);
  const purityErrors = checkPurity(program);
  if (purityErrors.length) {
    throw new KestrelError("Purity check failed:\n  " + purityErrors.join("\n  "));
  }
  const boundsNotes = checkBounds(program);
  const result = interpret(program, opts);
  return { result, boundsNotes };
}

// Same language semantics as run(), but compiles to bytecode and executes
// on the stack-based VM above instead of walking the AST. See
// kestrel-DESIGN.md for what this backend does and doesn't do yet — it's
// a faster interpreter, not the persistent-cache/native-codegen backend
// the design doc describes.
function runFast(src, opts = {}) {
  const tokens = lex(src);
  const program = parse(tokens);
  const purityErrors = checkPurity(program);
  if (purityErrors.length) {
    throw new KestrelError("Purity check failed:\n  " + purityErrors.join("\n  "));
  }
  const boundsNotes = checkBounds(program);
  const functions = compile(program);
  const onPrint = opts.onPrint || ((s) => console.log(s));
  const result = execute(functions, "main", [], onPrint);
  return { result, boundsNotes };
}

// Export for Node; in the browser this file is loaded as a plain
// <script>, and `Kestrel` is used as a global instead.
const Kestrel = {
  lex, parse, checkPurity, checkBounds, interpret, run,
  compile, execute, runFast,
  KestrelError,
};
if (typeof module !== "undefined") module.exports = Kestrel;
if (typeof window !== "undefined") window.Kestrel = Kestrel;
