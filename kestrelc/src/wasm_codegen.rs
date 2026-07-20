// AST -> WebAssembly module, directly (not via Cranelift — Cranelift's
// codegen targets real CPUs, not WASM as an output format; Wasmtime uses
// it the other way around, compiling *from* WASM). Uses wasm-encoder to
// build a real .wasm binary. Scope for this first version, matching how
// the native backend started: integers, functions/recursion, control
// flow, print — no arrays yet. See kestrelc/README.md.
//
// WASM's instruction encoding is inherently stack-based and its control
// flow (`if`/`else`/`end`, `block`/`loop`/`br`) is structured, so this
// codegen is actually simpler than the Cranelift path: no manual basic
// blocks, no SSA construction, no lazy merge-block trick for early
// returns — WASM's own `return` instruction and structured nesting
// handle all of that for free.

use crate::ast::*;
use std::collections::HashMap;
use wasm_encoder::{
    CodeSection, ConstExpr, EntityType, ExportKind, ExportSection, Function, FunctionSection,
    ImportSection, MemorySection, MemoryType, Module, TypeSection, ValType,
};

pub struct WasmError(pub String);

// Host imports every module needs: two ways for the running program to
// report output back to whatever's embedding it (the browser, or Node
// for testing), since WASM has no I/O of its own. `is_last` is nonzero
// for a print statement's final argument — matches print()'s "join with
// spaces, then one newline" behavior without needing the callee to know
// argument counts.
const IMPORT_PRINT_I64: u32 = 0; // (value: i64, is_last: i32) -> ()
const IMPORT_PRINT_STR: u32 = 1; // (ptr: i32, len: i32, is_last: i32) -> ()
const NUM_IMPORTS: u32 = 2;

pub fn compile_to_wasm(program: &Program) -> Result<Vec<u8>, WasmError> {
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
    imports.import(
        "env",
        "print_i64",
        EntityType::Function(0),
    );
    imports.import(
        "env",
        "print_str",
        EntityType::Function(1),
    );

    // One WASM memory, exported so the host can read string data written
    // into it by the data section below — this is how a WASM module
    // shares byte data with its host without any I/O syscalls.
    let mut memories = MemorySection::new();
    memories.memory(MemoryType { minimum: 1, maximum: None, memory64: false, shared: false, page_size_log2: None });
    exports.export("memory", ExportKind::Memory, 0);

    // Declare every function's type + a name -> wasm-func-index map
    // first, so calls (forward references, recursion) resolve regardless
    // of source order — same two-pass structure as the native backend.
    let mut fn_indices: HashMap<String, u32> = HashMap::new();
    for (i, f) in program.iter().enumerate() {
        if f.params.iter().any(|p| matches!(p.ty, Type::Array { .. })) {
            return Err(WasmError(format!(
                "kestrelc's WASM backend doesn't support arrays yet ('{}' has an array parameter) — see kestrelc/README.md",
                f.name
            )));
        }
        let params = vec![ValType::I64; f.params.len()];
        types.ty().function(params, [ValType::I64]);
        let type_idx = NUM_IMPORTS + i as u32;
        functions.function(type_idx);
        fn_indices.insert(f.name.clone(), NUM_IMPORTS + i as u32);
        if f.name == "main" {
            exports.export("main", ExportKind::Func, NUM_IMPORTS + i as u32);
        }
    }

    for f in program {
        let body = gen_fn(f, &fn_indices, &mut data_bytes, &mut str_offsets)?;
        code.function(&body);
    }

    let mut data = wasm_encoder::DataSection::new();
    if !data_bytes.is_empty() {
        data.active(0, &ConstExpr::i32_const(0), data_bytes.iter().copied());
    }

    let mut module = Module::new();
    module.section(&types);
    module.section(&imports);
    module.section(&functions);
    module.section(&memories);
    module.section(&exports);
    module.section(&code);
    module.section(&data);

    Ok(module.finish())
}

fn add_slot(name: &str, slots: &mut Vec<String>, seen: &mut HashMap<String, u32>) {
    if !seen.contains_key(name) {
        seen.insert(name.to_string(), slots.len() as u32);
        slots.push(name.to_string());
    }
}

fn walk_slots(stmts: &[Stmt], slots: &mut Vec<String>, seen: &mut HashMap<String, u32>) {
    for s in stmts {
        match s {
            Stmt::Let { name, .. } => add_slot(name, slots, seen),
            Stmt::If { then_block, else_block, .. } => {
                walk_slots(then_block, slots, seen);
                if let Some(eb) = else_block {
                    walk_slots(eb, slots, seen);
                }
            }
            Stmt::While { body, .. } => walk_slots(body, slots, seen),
            _ => {}
        }
    }
}

fn gen_fn(
    f: &Fn,
    fn_indices: &HashMap<String, u32>,
    data_bytes: &mut Vec<u8>,
    str_offsets: &mut HashMap<String, (u32, u32)>,
) -> Result<Function, WasmError> {
    let mut slots: Vec<String> = Vec::new();
    let mut seen: HashMap<String, u32> = HashMap::new();
    for p in &f.params {
        add_slot(&p.name, &mut slots, &mut seen);
    }
    let param_count = slots.len() as u32;
    walk_slots(&f.body, &mut slots, &mut seen);
    let extra_locals = slots.len() as u32 - param_count;

    let mut func = Function::new(if extra_locals > 0 { vec![(extra_locals, ValType::I64)] } else { vec![] });
    let mut fc = FnWasm { func: &mut func, slots: seen, fn_indices, data_bytes, str_offsets };
    fc.gen_block(&f.body)?;
    // Falling off the end returns 0, matching the other two backends.
    func.instructions().i64_const(0).return_();
    func.instructions().end();
    Ok(func)
}

struct FnWasm<'a> {
    func: &'a mut Function,
    slots: HashMap<String, u32>,
    fn_indices: &'a HashMap<String, u32>,
    data_bytes: &'a mut Vec<u8>,
    str_offsets: &'a mut HashMap<String, (u32, u32)>,
}

type WResult<T> = Result<T, WasmError>;

impl<'a> FnWasm<'a> {
    fn gen_block(&mut self, stmts: &[Stmt]) -> WResult<()> {
        for s in stmts {
            self.gen_stmt(s)?;
        }
        Ok(())
    }

    fn gen_stmt(&mut self, s: &Stmt) -> WResult<()> {
        match s {
            Stmt::Let { name, value } => {
                self.gen_expr(value)?;
                let idx = self.slots[name];
                self.func.instructions().local_set(idx);
                Ok(())
            }
            Stmt::Assign { name, value } => {
                let idx = *self
                    .slots
                    .get(name)
                    .ok_or_else(|| WasmError(format!("Assignment to unknown variable '{name}'")))?;
                self.gen_expr(value)?;
                self.func.instructions().local_set(idx);
                Ok(())
            }
            Stmt::If { cond, then_block, else_block } => {
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
            Stmt::While { cond, body } => {
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
            Stmt::Print { args } => self.gen_print(args),
            Stmt::Return { value } => {
                match value {
                    Some(e) => self.gen_expr(e)?,
                    None => {
                        self.func.instructions().i64_const(0);
                    }
                }
                self.func.instructions().return_();
                Ok(())
            }
            Stmt::ExprStmt { expr } => {
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
                    let (offset, len) = self.intern_str(s);
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
            Expr::Str(_) => Err(WasmError(
                "kestrelc's WASM backend only supports string literals as direct print() arguments so far".into(),
            )),
            Expr::Ident(name) => {
                let idx = *self
                    .slots
                    .get(name)
                    .ok_or_else(|| WasmError(format!("Unknown identifier '{name}'")))?;
                self.func.instructions().local_get(idx);
                Ok(())
            }
            Expr::ArrayLit(_) | Expr::Index { .. } => Err(WasmError(
                "kestrelc's WASM backend doesn't support arrays yet — see kestrelc/README.md".into(),
            )),
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
                    .ok_or_else(|| WasmError(format!("Unknown function '{name}'")))?;
                for a in args {
                    self.gen_expr(a)?;
                }
                self.func.instructions().call(idx);
                Ok(())
            }
        }
    }
}
