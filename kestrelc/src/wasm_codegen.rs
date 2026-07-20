// AST -> WebAssembly module, directly (not via Cranelift — Cranelift's
// codegen targets real CPUs, not WASM as an output format; Wasmtime uses
// it the other way around, compiling *from* WASM). Uses wasm-encoder to
// build a real .wasm binary.
//
// WASM's instruction encoding is inherently stack-based and its control
// flow (`if`/`else`/`end`, `block`/`loop`/`br`) is structured, so this
// codegen is actually simpler than the Cranelift path: no manual basic
// blocks, no SSA construction, no lazy merge-block trick for early
// returns — WASM's own `return` instruction and structured nesting
// handle all of that for free.
//
// Arrays are a (pointer, length) pair, same idea as the native backend —
// but WASM has no `alloca`/stack-slot instruction, so array literals are
// allocated from a simple bump allocator: a single mutable global ($bump)
// that only ever moves forward, out of a fixed-size arena reserved after
// the string data. Nothing is ever freed. That's a real, deliberate
// limitation (fine for short-lived toy programs, not for anything
// long-running or allocation-heavy) — see kestrelc-web/README.md.

use crate::ast::*;
use crate::error::{ErrorKind, KestrelcError};
use crate::interner::Symbol;
use crate::span::Span;
use crate::where_info::{extract_where_info, WhereInfo};
use std::collections::HashMap;
use wasm_encoder::{
    CodeSection, ConstExpr, EntityType, ExportKind, ExportSection, Function, FunctionSection,
    GlobalSection, GlobalType, ImportSection, MemArg, MemorySection, MemoryType, Module,
    TypeSection, ValType,
};

// Host imports every module needs: two ways for the running program to
// report output back to whatever's embedding it (the browser, or Node
// for testing), since WASM has no I/O of its own. `is_last` is nonzero
// for a print statement's final argument — matches print()'s "join with
// spaces, then one newline" behavior without needing the callee to know
// argument counts.
const IMPORT_PRINT_I64: u32 = 0; // (value: i64, is_last: i32) -> ()
const IMPORT_PRINT_STR: u32 = 1; // (ptr: i32, len: i32, is_last: i32) -> ()
const NUM_IMPORTS: u32 = 2;

const BUMP_GLOBAL: u32 = 0;
// Fixed, non-growing arena for array data. Plenty for toy programs;
// exhausting it silently corrupts memory rather than trapping — a known,
// undocumented-until-now-in-code limitation, same spirit as the native
// backend not guarding against real OS stack overflow on deep recursion.
const ARENA_BYTES: u32 = 1 << 20; // 1 MiB
const WASM_PAGE: u32 = 65536;

pub fn compile_to_wasm(program: &Program) -> Result<Vec<u8>, KestrelcError> {
    let mut types = TypeSection::new();
    let mut imports = ImportSection::new();
    let mut functions = FunctionSection::new();
    let mut exports = ExportSection::new();
    let mut code = CodeSection::new();
    let mut data_bytes: Vec<u8> = Vec::new();
    let mut str_offsets: HashMap<String, (u32, u32)> = HashMap::new(); // text -> (offset, len)

    // Type 0: print_i64, Type 1: print_str.
    types.ty().function([ValType::I64, ValType::I32], []);
    types.ty().function([ValType::I32, ValType::I32, ValType::I32], []);
    imports.import("env", "print_i64", EntityType::Function(0));
    imports.import("env", "print_str", EntityType::Function(1));

    // Declare every function's type + a name -> wasm-func-index map
    // first, so calls (forward references, recursion) resolve regardless
    // of source order — same two-pass structure as the native backend.
    // An array-typed parameter expands to two i32 params (pointer, then
    // length), matching the native backend's two AbiParams per array.
    let mut fn_indices: HashMap<Symbol, u32> = HashMap::new();
    let mut where_infos: HashMap<Symbol, WhereInfo> = HashMap::new();
    for (i, f) in program.iter().enumerate() {
        let mut params = Vec::with_capacity(f.params.len());
        for p in &f.params {
            match &p.ty {
                Type::Array { .. } => {
                    params.push(ValType::I32); // pointer
                    params.push(ValType::I32); // length
                }
                Type::Named(_) => params.push(ValType::I64),
            }
        }
        types.ty().function(params, [ValType::I64]);
        let type_idx = NUM_IMPORTS + i as u32;
        functions.function(type_idx);
        fn_indices.insert(f.name, NUM_IMPORTS + i as u32);
        if &*f.name.resolve() == "main" {
            exports.export("main", ExportKind::Func, NUM_IMPORTS + i as u32);
        }
        if let Some(info) = extract_where_info(f) {
            where_infos.insert(f.name, info);
        }
    }

    for f in program {
        let my_where = where_infos.get(&f.name);
        let body = gen_fn(f, &fn_indices, &where_infos, my_where, &mut data_bytes, &mut str_offsets)?;
        code.function(&body);
    }

    let mut data = wasm_encoder::DataSection::new();
    if !data_bytes.is_empty() {
        data.active(0, &ConstExpr::i32_const(0), data_bytes.iter().copied());
    }

    // The array arena starts right after the string data, 8-byte aligned
    // so i64 element stores land on aligned addresses.
    let arena_start = (data_bytes.len() as u32 + 7) & !7;
    let total_bytes = arena_start + ARENA_BYTES;
    let pages = total_bytes.div_ceil(WASM_PAGE);

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: pages as u64,
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    // Exported so the host can read string data written into it by the
    // data section, and (for debugging) array data from the bump arena.
    exports.export("memory", ExportKind::Memory, 0);

    let mut globals = GlobalSection::new();
    globals.global(
        GlobalType { val_type: ValType::I32, mutable: true, shared: false },
        &ConstExpr::i32_const(arena_start as i32),
    );

    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&functions);
    module.section(&memories);
    module.section(&globals);
    module.section(&exports);
    module.section(&code);
    module.section(&data);

    Ok(module.finish())
}

#[derive(Clone, Copy)]
enum SlotKind {
    Scalar,
    Array { literal_len: Option<u32> },
}

// A scalar gets one i64 local. An array gets two i32 locals — a base
// pointer and a length — since a WASM local is a single value. `len` is
// still materialized as a real local even when `literal_len` is known
// (kept uniform with the native backend's Slot::Array; the constant is
// used directly wherever it's known, at compile time, and the runtime
// copy exists mainly so array parameters — where the length only exists
// at runtime — use the exact same code paths as array locals).
enum VarLoc {
    Scalar(u32),
    Array { ptr: u32, len: u32, literal_len: Option<u32> },
}

// `known_lens` is every literal-length array slot seen *so far* in this
// same pass (params first, then `let`s in occurrence order) — needed to
// classify `let out = parallel_map(f, arr);` as an array slot with the
// same length as `arr`, without a separate type-checking pass. Only
// works when `arr` is itself a literal-length array (an earlier `let`
// with an array-literal value); an array *parameter* has no
// compile-time-known length, so parallel_map over one isn't supported
// yet (rejected with a clear error at codegen time, once `resolve_array`
// runs — see `gen_binding`'s parallel_map arm).
fn slot_kind_for_let(value: &Expr, known_lens: &HashMap<Symbol, u32>) -> SlotKind {
    match value {
        Expr::ArrayLit(elems) => SlotKind::Array { literal_len: Some(elems.len() as u32) },
        Expr::Call { name, args } if &*name.resolve() == "parallel_map" && args.len() == 2 => {
            let len = match &args[1] {
                Expr::Ident(arr_name) => known_lens.get(arr_name).copied(),
                _ => None,
            };
            SlotKind::Array { literal_len: len }
        }
        _ => SlotKind::Scalar,
    }
}

fn slot_kind_for_param(ty: &Type) -> SlotKind {
    match ty {
        Type::Array { .. } => SlotKind::Array { literal_len: None },
        Type::Named(_) => SlotKind::Scalar,
    }
}

fn add_slot(name: Symbol, kind: SlotKind, slots: &mut Vec<(Symbol, SlotKind)>, seen: &mut HashMap<Symbol, ()>) {
    if !seen.contains_key(&name) {
        seen.insert(name, ());
        slots.push((name, kind));
    }
}

fn walk_slots(
    stmts: &[Stmt],
    slots: &mut Vec<(Symbol, SlotKind)>,
    seen: &mut HashMap<Symbol, ()>,
    known_lens: &mut HashMap<Symbol, u32>,
) {
    for s in stmts {
        match s {
            Stmt::Let { name, value, .. } => {
                let kind = slot_kind_for_let(value, known_lens);
                if let SlotKind::Array { literal_len: Some(l) } = kind {
                    known_lens.insert(*name, l);
                }
                add_slot(*name, kind, slots, seen);
            }
            Stmt::If { then_block, else_block, .. } => {
                walk_slots(then_block, slots, seen, known_lens);
                if let Some(eb) = else_block {
                    walk_slots(eb, slots, seen, known_lens);
                }
            }
            Stmt::While { body, .. } => walk_slots(body, slots, seen, known_lens),
            _ => {}
        }
    }
}

fn collect_slots(f: &Fn) -> Vec<(Symbol, SlotKind)> {
    let mut slots: Vec<(Symbol, SlotKind)> = Vec::new();
    let mut seen: HashMap<Symbol, ()> = HashMap::new();
    let mut known_lens: HashMap<Symbol, u32> = HashMap::new();
    for p in &f.params {
        add_slot(p.name, slot_kind_for_param(&p.ty), &mut slots, &mut seen);
    }
    walk_slots(&f.body, &mut slots, &mut seen, &mut known_lens);
    slots
}

fn gen_fn(
    f: &Fn,
    fn_indices: &HashMap<Symbol, u32>,
    where_info: &HashMap<Symbol, WhereInfo>,
    my_where: Option<&WhereInfo>,
    data_bytes: &mut Vec<u8>,
    str_offsets: &mut HashMap<String, (u32, u32)>,
) -> Result<Function, KestrelcError> {
    let slot_kinds = collect_slots(f);

    // Assign WASM local indices: params first (their count/types must
    // match the function's declared signature exactly), then every other
    // slot as an additional declared local.
    let mut vars: HashMap<Symbol, VarLoc> = HashMap::new();
    let mut next_local: u32 = 0;
    let mut extra_locals: Vec<(u32, ValType)> = Vec::new();
    let param_names: std::collections::HashSet<Symbol> = f.params.iter().map(|p| p.name).collect();

    for (name, kind) in &slot_kinds {
        let is_param = param_names.contains(name);
        match kind {
            SlotKind::Scalar => {
                let idx = next_local;
                next_local += 1;
                if !is_param {
                    extra_locals.push((1, ValType::I64));
                }
                vars.insert(*name, VarLoc::Scalar(idx));
            }
            SlotKind::Array { literal_len } => {
                let ptr = next_local;
                let len = next_local + 1;
                next_local += 2;
                if !is_param {
                    extra_locals.push((2, ValType::I32));
                }
                vars.insert(*name, VarLoc::Array { ptr, len, literal_len: *literal_len });
            }
        }
    }

    // One scratch i32 local per function, reused for duplicating an
    // index value across the bounds check and the address computation —
    // WASM has no stack-dup instruction, only `local.tee`.
    let scratch = next_local;
    extra_locals.push((1, ValType::I32));

    let mut func = Function::new(extra_locals);
    let mut fc = FnWasm {
        func: &mut func,
        vars,
        fn_indices,
        where_info,
        my_where,
        data_bytes,
        str_offsets,
        scratch,
        cur_span: f.span,
    };
    fc.gen_block(&f.body)?;
    // Falling off the end returns 0, matching the other two backends.
    func.instructions().i64_const(0).return_();
    func.instructions().end();
    Ok(func)
}

struct FnWasm<'a> {
    func: &'a mut Function,
    vars: HashMap<Symbol, VarLoc>,
    fn_indices: &'a HashMap<Symbol, u32>,
    // `where_info` covers every function with a recognized `where idx <
    // N` clause, used to validate the precondition at each *call site*.
    // `my_where` is this specific function's own entry (if any), used to
    // elide the redundant runtime check on the one access inside its own
    // body that the precondition already covers — see codegen.rs's
    // identical native-backend logic for the full rationale.
    where_info: &'a HashMap<Symbol, WhereInfo>,
    my_where: Option<&'a WhereInfo>,
    data_bytes: &'a mut Vec<u8>,
    str_offsets: &'a mut HashMap<String, (u32, u32)>,
    scratch: u32,
    /// The span of the statement `gen_stmt` is currently generating code
    /// for — see codegen.rs's identical `cur_span` field/`err()` method
    /// for the full rationale. Closes the gap left by native codegen
    /// errors getting real positions and this backend's not.
    cur_span: Span,
}

type WResult<T> = Result<T, KestrelcError>;

impl<'a> FnWasm<'a> {
    fn err(&self, message: String) -> KestrelcError {
        KestrelcError::new(ErrorKind::Codegen, message, self.cur_span)
    }

    fn gen_block(&mut self, stmts: &[Stmt]) -> WResult<()> {
        for s in stmts {
            self.gen_stmt(s)?;
        }
        Ok(())
    }

    /// Shared by `let` and `=`: binds `name` to `value`, handling both
    /// the scalar case and the array-literal case (bump-allocate, one
    /// store per element, then set the ptr/len locals).
    fn gen_binding(&mut self, name: Symbol, value: &Expr) -> WResult<()> {
        match (&self.vars[&name], value) {
            (VarLoc::Scalar(idx), _) => {
                let idx = *idx;
                self.gen_expr(value)?;
                self.func.instructions().local_set(idx);
                Ok(())
            }
            (VarLoc::Array { ptr, len, literal_len }, Expr::ArrayLit(elems)) => {
                let (ptr, len) = (*ptr, *len);
                let expected = literal_len.expect("array let-bindings always have a literal_len");
                if elems.len() as u32 != expected {
                    return Err(self.err(format!(
                        "kestrelc: array variable '{name}' rebound with a different length ({} vs {expected}) — not supported",
                        elems.len()
                    )));
                }
                let size_bytes = elems.len() as u32 * 8;
                // ptr = $bump; $bump += size_bytes
                self.func.instructions().global_get(BUMP_GLOBAL);
                self.func.instructions().local_set(ptr);
                self.func.instructions().global_get(BUMP_GLOBAL);
                self.func.instructions().i32_const(size_bytes as i32);
                self.func.instructions().i32_add();
                self.func.instructions().global_set(BUMP_GLOBAL);
                for (i, el) in elems.iter().enumerate() {
                    self.func.instructions().local_get(ptr);
                    self.gen_expr(el)?;
                    self.func.instructions().i64_store(MemArg { offset: (i * 8) as u64, align: 3, memory_index: 0 });
                }
                self.func.instructions().i32_const(elems.len() as i32);
                self.func.instructions().local_set(len);
                Ok(())
            }
            (VarLoc::Array { ptr, len, literal_len }, Expr::Call { name: call_name, args }) if &*call_name.resolve() == "parallel_map" => {
                let (ptr, len) = (*ptr, *len);
                let func_name = match &args[0] {
                    Expr::Ident(n) => *n,
                    _ => return Err(self.err(
                        "parallel_map()'s first argument must be a bare function name".into(),
                    )),
                };
                let callee_idx = *self
                    .fn_indices
                    .get(&func_name)
                    .ok_or_else(|| self.err(format!("Unknown function '{func_name}'")))?;
                let elem_count = self.static_array_len(&args[1]).ok_or_else(|| self.err(
                    "kestrelc's WASM backend only supports parallel_map over a fixed-size array literal (`let x = [...]`) so far, not an array parameter".into(),
                ))?;
                let expected = literal_len.expect("array let-bindings always have a literal_len");
                if elem_count != expected {
                    return Err(self.err(format!(
                        "kestrelc: array variable '{name}' rebound with a different length ({elem_count} vs {expected}) — not supported"
                    )));
                }
                let (in_ptr, _in_len) = self.resolve_array(&args[1])?;
                let size_bytes = elem_count * 8;

                // out = $bump; $bump += size_bytes
                self.func.instructions().global_get(BUMP_GLOBAL);
                self.func.instructions().local_set(ptr);
                self.func.instructions().global_get(BUMP_GLOBAL);
                self.func.instructions().i32_const(size_bytes as i32);
                self.func.instructions().i32_add();
                self.func.instructions().global_set(BUMP_GLOBAL);

                // Sequential loop — this backend has no threads (WASM's
                // threads proposal needs SharedArrayBuffer + a Worker per
                // thread, well out of scope here). Real parallelism is
                // kestrelc's native backend only; see kestrelc-web/README.md.
                // for (i = 0; i < elem_count; i++) out[i] = f(in[i]);
                let idx = self.scratch;
                self.func.instructions().i32_const(0);
                self.func.instructions().local_set(idx);
                self.func.instructions().block(wasm_encoder::BlockType::Empty);
                self.func.instructions().loop_(wasm_encoder::BlockType::Empty);
                self.func.instructions().local_get(idx);
                self.func.instructions().i32_const(elem_count as i32);
                self.func.instructions().i32_ge_s();
                self.func.instructions().br_if(1); // i >= elem_count -> break

                // dest_addr = out_ptr + i*8
                self.func.instructions().local_get(ptr);
                self.func.instructions().local_get(idx);
                self.func.instructions().i32_const(3);
                self.func.instructions().i32_shl();
                self.func.instructions().i32_add();

                // f(in[i])
                self.func.instructions().local_get(in_ptr);
                self.func.instructions().local_get(idx);
                self.func.instructions().i32_const(3);
                self.func.instructions().i32_shl();
                self.func.instructions().i32_add();
                self.func.instructions().i64_load(MemArg { offset: 0, align: 3, memory_index: 0 });
                self.func.instructions().call(callee_idx);

                // out[i] = <call result>  (address was pushed before the
                // call above and is untouched by it — call only consumes
                // its own i64 argument/result off the top of the stack)
                self.func.instructions().i64_store(MemArg { offset: 0, align: 3, memory_index: 0 });

                self.func.instructions().local_get(idx);
                self.func.instructions().i32_const(1);
                self.func.instructions().i32_add();
                self.func.instructions().local_set(idx);
                self.func.instructions().br(0);
                self.func.instructions().end(); // end loop
                self.func.instructions().end(); // end block

                self.func.instructions().i32_const(elem_count as i32);
                self.func.instructions().local_set(len);
                Ok(())
            }
            (VarLoc::Array { .. }, _) => Err(self.err(format!(
                "kestrelc: '{name}' is an array variable and can only be (re)bound to an array literal so far"
            ))),
        }
    }

    fn gen_stmt(&mut self, s: &Stmt) -> WResult<()> {
        self.cur_span = match s {
            Stmt::Let { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::If { span, .. }
            | Stmt::While { span, .. }
            | Stmt::Print { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::ExprStmt { span, .. } => *span,
        };
        match s {
            Stmt::Let { name, value, .. } => self.gen_binding(*name, value),
            Stmt::Assign { name, value, .. } => {
                if !self.vars.contains_key(name) {
                    return Err(self.err(format!("Assignment to unknown variable '{name}'")));
                }
                self.gen_binding(*name, value)
            }
            Stmt::If { cond, then_block, else_block, .. } => {
                self.gen_expr(cond)?;
                self.func.instructions().i64_const(0).i64_ne(); // WASM `if` needs an i32 condition
                self.func.instructions().if_(wasm_encoder::BlockType::Empty);
                self.gen_block(then_block)?;
                if let Some(eb) = else_block {
                    self.func.instructions().else_();
                    self.gen_block(eb)?;
                }
                self.func.instructions().end();
                Ok(())
            }
            Stmt::While { cond, body, .. } => {
                self.func.instructions().block(wasm_encoder::BlockType::Empty);
                self.func.instructions().loop_(wasm_encoder::BlockType::Empty);
                self.gen_expr(cond)?;
                self.func.instructions().i64_const(0).i64_eq();
                self.func.instructions().br_if(1); // condition false -> break out of the block
                self.gen_block(body)?;
                self.func.instructions().br(0); // loop back
                self.func.instructions().end(); // end loop
                self.func.instructions().end(); // end block
                Ok(())
            }
            Stmt::Print { args, .. } => self.gen_print(args),
            Stmt::Return { value, .. } => {
                match value {
                    Some(e) => self.gen_expr(e)?,
                    None => {
                        self.func.instructions().i64_const(0);
                    }
                }
                self.func.instructions().return_();
                Ok(())
            }
            Stmt::ExprStmt { expr, .. } => {
                self.gen_expr(expr)?;
                self.func.instructions().drop();
                Ok(())
            }
        }
    }

    fn intern_str(&mut self, s: &str) -> (u32, u32) {
        if let Some(v) = self.str_offsets.get(s) {
            return *v;
        }
        let offset = self.data_bytes.len() as u32;
        self.data_bytes.extend_from_slice(s.as_bytes());
        let len = s.len() as u32;
        self.str_offsets.insert(s.to_string(), (offset, len));
        (offset, len)
    }

    fn gen_print(&mut self, args: &[Expr]) -> WResult<()> {
        if args.is_empty() {
            // An empty print() call never appears in the example programs;
            // treat it as printing an empty final "argument" so the host
            // still emits the trailing newline.
            let (offset, len) = self.intern_str("");
            self.func.instructions().i32_const(offset as i32).i32_const(len as i32).i32_const(1);
            self.func.instructions().call(IMPORT_PRINT_STR);
            return Ok(());
        }
        for (i, arg) in args.iter().enumerate() {
            let is_last = if i == args.len() - 1 { 1 } else { 0 };
            match arg {
                Expr::Str(s) => {
                    let (offset, len) = self.intern_str(&s.resolve());
                    self.func.instructions().i32_const(offset as i32).i32_const(len as i32).i32_const(is_last);
                    self.func.instructions().call(IMPORT_PRINT_STR);
                }
                other => {
                    self.gen_expr(other)?;
                    self.func.instructions().i32_const(is_last);
                    self.func.instructions().call(IMPORT_PRINT_I64);
                }
            }
        }
        Ok(())
    }

    /// The array's element count, if known at compile time (a `let x =
    /// [literal, ...]` local — array *parameters* never have a
    /// compile-time-known length). Used only to decide whether a bounds
    /// check can be proven/elided at compile time.
    fn static_array_len(&self, e: &Expr) -> Option<u32> {
        match e {
            Expr::Ident(name) => match self.vars.get(name) {
                Some(VarLoc::Array { literal_len, .. }) => *literal_len,
                _ => None,
            },
            _ => None,
        }
    }

    /// Resolves an expression that must denote an array to its (ptr, len)
    /// local indices. Scope for now: only a plain identifier naming an
    /// array local/parameter, matching the native backend.
    fn resolve_array(&self, e: &Expr) -> WResult<(u32, u32)> {
        let name = match e {
            Expr::Ident(name) => name,
            _ => {
                return Err(self.err(
                    "kestrelc only supports indexing/passing a plain array variable so far".into(),
                ))
            }
        };
        match self.vars.get(name) {
            Some(VarLoc::Array { ptr, len, .. }) => Ok((*ptr, *len)),
            Some(VarLoc::Scalar(_)) => Err(self.err(format!("'{name}' is not an array"))),
            None => Err(self.err(format!("Unknown identifier '{name}'"))),
        }
    }

    fn gen_expr(&mut self, e: &Expr) -> WResult<()> {
        match e {
            Expr::Num(n) => {
                self.func.instructions().i64_const(*n);
                Ok(())
            }
            Expr::Bool(b) => {
                self.func.instructions().i64_const(if *b { 1 } else { 0 });
                Ok(())
            }
            Expr::Str(_) => Err(self.err(
                "kestrelc's WASM backend only supports string literals as direct print() arguments so far".into(),
            )),
            Expr::Ident(name) => match self.vars.get(name) {
                Some(VarLoc::Scalar(idx)) => {
                    self.func.instructions().local_get(*idx);
                    Ok(())
                }
                Some(VarLoc::Array { .. }) => Err(self.err(format!(
                    "'{name}' is an array — it can only be indexed (arr[i]) or passed to a function, not used as a value directly"
                ))),
                None => Err(self.err(format!("Unknown identifier '{name}'"))),
            },
            Expr::ArrayLit(_) => Err(self.err(
                "kestrelc only supports array literals as the direct value of a `let`/assignment so far".into(),
            )),
            Expr::Index { target, index } => {
                // Proof-carrying fast path #1: a literal index into an
                // array whose length is also known at compile time (a
                // `let` literal, not a parameter) is proven safe — or
                // proven *unsafe* — right now, with no runtime check
                // either way.
                if let (Expr::Num(n), Some(static_len)) = (index.as_ref(), self.static_array_len(target)) {
                    if *n < 0 || *n as u32 >= static_len {
                        return Err(self.err(format!(
                            "index {n} is out of bounds for array of length {static_len} — proven at compile time, not deferred to a runtime check"
                        )));
                    }
                    let (ptr, _len) = self.resolve_array(target)?;
                    self.func.instructions().local_get(ptr);
                    self.func.instructions().i64_load(MemArg { offset: (*n as u64) * 8, align: 3, memory_index: 0 });
                    return Ok(());
                }

                // Proof-carrying fast path #2: this function has a
                // `where idx_param < N` clause tying exactly this (array
                // parameter, index parameter) pair together, and this is
                // exactly that access (`arr_param[idx_param]`). Every
                // call site to this function is required (see the Call
                // arm below) to prove the precondition before the call
                // is even allowed to compile, so by the time we're
                // generating code *inside* this function, the
                // precondition is already guaranteed and the check would
                // be redundant. Same logic as the native backend's
                // identical fast path in codegen.rs.
                if let (Expr::Ident(t), Expr::Ident(i)) = (target.as_ref(), index.as_ref()) {
                    if let Some(w) = self.my_where {
                        if t == &w.arr_param && i == &w.idx_param {
                            let (ptr, _len) = self.resolve_array(target)?;
                            self.gen_expr(index)?; // pushes i64 index
                            self.func.instructions().i32_wrap_i64();
                            self.func.instructions().i32_const(3);
                            self.func.instructions().i32_shl();
                            self.func.instructions().local_get(ptr);
                            self.func.instructions().i32_add();
                            self.func.instructions().i64_load(MemArg { offset: 0, align: 3, memory_index: 0 });
                            return Ok(());
                        }
                    }
                }

                let (ptr, len) = self.resolve_array(target)?;
                let scratch = self.scratch;
                self.gen_expr(index)?; // pushes i64 index
                self.func.instructions().i32_wrap_i64();
                self.func.instructions().local_tee(scratch); // stash idx32, leave a copy on the stack

                // idx32 <s 0
                self.func.instructions().i32_const(0);
                self.func.instructions().i32_lt_s();
                // idx32 >=s len32
                self.func.instructions().local_get(scratch);
                self.func.instructions().local_get(len);
                self.func.instructions().i32_ge_s();
                self.func.instructions().i32_or();
                self.func.instructions().if_(wasm_encoder::BlockType::Empty);
                // Matches run()/runFast()'s "always check" behavior, and
                // now also their friendly error message — printed through
                // the same host imports print() uses, right before the
                // trap that actually halts the module. `scratch` already
                // holds the wrapped i32 index; extending it back to i64
                // for printing avoids re-evaluating (and, if it has a
                // function call in it, re-running) the index expression.
                // print_i64/print_str join every segment of one "line"
                // with a space (matching print()'s own arg-separator
                // behavior — this reuses those same host imports), so
                // the message strings themselves carry no leading/
                // trailing space of their own.
                let (msg1_off, msg1_len) = self.intern_str("kestrelc: Index");
                self.func.instructions().i32_const(msg1_off as i32).i32_const(msg1_len as i32).i32_const(0);
                self.func.instructions().call(IMPORT_PRINT_STR);
                self.func.instructions().local_get(scratch);
                self.func.instructions().i64_extend_i32_s();
                self.func.instructions().i32_const(0);
                self.func.instructions().call(IMPORT_PRINT_I64);
                let (msg2_off, msg2_len) = self.intern_str("out of bounds for array of length");
                self.func.instructions().i32_const(msg2_off as i32).i32_const(msg2_len as i32).i32_const(0);
                self.func.instructions().call(IMPORT_PRINT_STR);
                self.func.instructions().local_get(len);
                self.func.instructions().i64_extend_i32_u();
                self.func.instructions().i32_const(1);
                self.func.instructions().call(IMPORT_PRINT_I64);
                self.func.instructions().unreachable();
                self.func.instructions().end();

                // address = ptr + idx32 * 8
                self.func.instructions().local_get(ptr);
                self.func.instructions().local_get(scratch);
                self.func.instructions().i32_const(3);
                self.func.instructions().i32_shl();
                self.func.instructions().i32_add();
                self.func.instructions().i64_load(MemArg { offset: 0, align: 3, memory_index: 0 });
                Ok(())
            }
            Expr::Unary { op, expr } => {
                self.gen_expr(expr)?;
                match op {
                    UnOp::Neg => {
                        self.func.instructions().i64_const(-1).i64_mul();
                    }
                    UnOp::Not => {
                        self.func.instructions().i64_const(0).i64_eq().i64_extend_i32_u();
                    }
                }
                Ok(())
            }
            Expr::Binop { op, left, right } => {
                self.gen_expr(left)?;
                self.gen_expr(right)?;
                let ins = &mut self.func.instructions();
                match op {
                    BinOp::Add => { ins.i64_add(); }
                    BinOp::Sub => { ins.i64_sub(); }
                    BinOp::Mul => { ins.i64_mul(); }
                    BinOp::Div => { ins.i64_div_s(); }
                    BinOp::Mod => { ins.i64_rem_s(); }
                    BinOp::Eq => { ins.i64_eq(); ins.i64_extend_i32_u(); }
                    BinOp::Neq => { ins.i64_ne(); ins.i64_extend_i32_u(); }
                    BinOp::Lt => { ins.i64_lt_s(); ins.i64_extend_i32_u(); }
                    BinOp::Gt => { ins.i64_gt_s(); ins.i64_extend_i32_u(); }
                    BinOp::Le => { ins.i64_le_s(); ins.i64_extend_i32_u(); }
                    BinOp::Ge => { ins.i64_ge_s(); ins.i64_extend_i32_u(); }
                    // Not short-circuiting, same as the other two backends.
                    BinOp::And => { ins.i64_and(); }
                    BinOp::Or => { ins.i64_or(); }
                }
                Ok(())
            }
            Expr::Call { name, args } => {
                let idx = *self
                    .fn_indices
                    .get(name)
                    .ok_or_else(|| self.err(format!("Unknown function '{name}'")))?;

                // If the callee has a recognized `where idx < N` clause,
                // its precondition must be proven right here, at compile
                // time, before the call is allowed at all — matching
                // kestrel-DESIGN.md's own stated rule and the native
                // backend's identical check in codegen.rs. Narrow prover:
                // only a literal index against a literal-length array
                // argument is provable; anything else is rejected, not
                // silently trusted.
                if let Some(w) = self.where_info.get(name) {
                    let idx_arg = &args[w.idx_pos];
                    let arr_arg = &args[w.arr_pos];
                    let idx_lit = match idx_arg {
                        Expr::Num(n) => *n,
                        _ => {
                            return Err(self.err(format!(
                                "kestrelc: can't prove '{name}''s `where {} < ...` clause here — the index argument must be a literal number so far",
                                w.idx_param
                            )))
                        }
                    };
                    let arr_len = self.static_array_len(arr_arg).ok_or_else(|| {
                        self.err(format!(
                            "kestrelc: can't prove '{name}''s where clause here — the array argument must be a fixed-size array literal (`let x = [...]`) so far, not a parameter passed further down"
                        ))
                    })?;
                    if idx_lit < 0 || idx_lit as u32 >= arr_len {
                        return Err(self.err(format!(
                            "kestrelc: call to '{name}' can't satisfy its own `where {} < N` clause — index {idx_lit} is out of bounds for an array of length {arr_len}",
                            w.idx_param
                        )));
                    }
                }

                for a in args {
                    let is_array_ident =
                        matches!(a, Expr::Ident(n) if matches!(self.vars.get(n), Some(VarLoc::Array { .. })));
                    if is_array_ident {
                        let (ptr, len) = self.resolve_array(a)?;
                        self.func.instructions().local_get(ptr);
                        self.func.instructions().local_get(len);
                    } else {
                        self.gen_expr(a)?;
                    }
                }
                self.func.instructions().call(idx);
                Ok(())
            }
        }
    }
}
