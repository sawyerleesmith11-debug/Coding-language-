// AST -> Cranelift IR -> object file. Scope for now (see
// kestrelc/README.md): every runtime value is an i64 (numbers, and
// comparison/bool results as 0/1); string literals are only supported
// directly as print() arguments, not as general values; arrays aren't
// supported yet. Anything outside that scope is a clear compile error,
// not a silent miscompile.

use crate::ast::*;
use cranelift_codegen::ir::{types, AbiParam, Function, InstBuilder, Signature, UserFuncName};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::collections::HashMap;

pub struct CodegenError(pub String);

struct StrConst {
    data_id: DataId,
}

pub struct Codegen {
    module: ObjectModule,
    fn_ids: HashMap<String, FuncId>,
    printf_id: FuncId,
    str_cache: HashMap<String, StrConst>,
    str_counter: usize,
}

impl Codegen {
    pub fn new() -> Result<Self, CodegenError> {
        let mut flag_builder = settings::builder();
        flag_builder.set("is_pic", "true").map_err(|e| CodegenError(e.to_string()))?;
        flag_builder.set("opt_level", "speed").map_err(|e| CodegenError(e.to_string()))?;
        let isa_builder = cranelift_native::builder().map_err(|e| CodegenError(e.to_string()))?;
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| CodegenError(e.to_string()))?;

        let obj_builder = ObjectBuilder::new(
            isa,
            "kestrel_module",
            cranelift_module::default_libcall_names(),
        )
        .map_err(|e| CodegenError(e.to_string()))?;
        let mut module = ObjectModule::new(obj_builder);

        // printf(fmt: i64 ptr, arg: i64) -> i32 — declared with a fixed,
        // non-variadic Cranelift signature. This works on the System V
        // x86-64 ABI because the AL-register convention that real C
        // varargs callers must honor only matters when *floating-point*
        // variadic arguments are passed; every Kestrel value here is a
        // plain integer/pointer, so a fixed-arity call site is safe.
        let mut printf_sig = Signature::new(CallConv::SystemV);
        printf_sig.params.push(AbiParam::new(types::I64)); // format string pointer
        printf_sig.params.push(AbiParam::new(types::I64)); // one argument (0 used if unused)
        printf_sig.returns.push(AbiParam::new(types::I32));
        let printf_id = module
            .declare_function("printf", Linkage::Import, &printf_sig)
            .map_err(|e| CodegenError(e.to_string()))?;

        Ok(Codegen {
            module,
            fn_ids: HashMap::new(),
            printf_id,
            str_cache: HashMap::new(),
            str_counter: 0,
        })
    }

    fn fn_signature(program_fn: &Fn) -> Signature {
        let mut sig = Signature::new(CallConv::SystemV);
        for _ in &program_fn.params {
            sig.params.push(AbiParam::new(types::I64));
        }
        sig.returns.push(AbiParam::new(types::I64));
        sig
    }

    pub fn compile_program(&mut self, program: &Program) -> Result<(), CodegenError> {
        // Pass 1: declare every function's signature so calls (including
        // forward references and recursion) can be resolved regardless
        // of source order.
        for f in program {
            let sig = Self::fn_signature(f);
            let id = self
                .module
                .declare_function(&f.name, Linkage::Export, &sig)
                .map_err(|e| CodegenError(e.to_string()))?;
            self.fn_ids.insert(f.name.clone(), id);
        }

        // Pass 2: generate bodies.
        for f in program {
            self.compile_fn(f)?;
        }
        Ok(())
    }

    fn compile_fn(&mut self, f: &Fn) -> Result<(), CodegenError> {
        if let Some(w) = &f.where_clause {
            let _ = w; // bounds proofs aren't enforced by kestrelc yet — see README.
        }
        let func_id = self.fn_ids[&f.name];
        let sig = Self::fn_signature(f);

        let mut ctx = Context::new();
        ctx.func = Function::with_name_signature(UserFuncName::user(0, func_id.as_u32()), sig);

        let mut fb_ctx = FunctionBuilderContext::new();
        {
            let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            builder.seal_block(entry);

            let slots = collect_slots(f);
            let mut vars: HashMap<String, Variable> = HashMap::new();
            for (name, idx) in &slots {
                let var = Variable::from_u32(*idx as u32);
                builder.declare_var(var, types::I64);
                vars.insert(name.clone(), var);
            }
            for (i, p) in f.params.iter().enumerate() {
                let val = builder.block_params(entry)[i];
                builder.def_var(vars[&p.name], val);
            }

            let mut fc = FnCodegen {
                builder,
                vars,
                fn_ids: &self.fn_ids,
                printf_id: self.printf_id,
                module: &mut self.module,
                str_cache: &mut self.str_cache,
                str_counter: &mut self.str_counter,
            };
            let terminated = fc.gen_block(&f.body)?;
            if !terminated {
                let zero = fc.builder.ins().iconst(types::I64, 0);
                fc.builder.ins().return_(&[zero]);
            }
            fc.builder.finalize();
        }

        cranelift_codegen::verifier::verify_function(&ctx.func, self.module.isa())
            .map_err(|e| CodegenError(format!("kestrelc codegen bug in '{}': {e}", f.name)))?;

        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| CodegenError(format!("failed to define '{}': {e}", f.name)))?;

        Ok(())
    }

    pub fn finish(self) -> Vec<u8> {
        self.module.finish().emit().expect("object emission failed")
    }
}

// Every distinct name a function's body ever binds gets one Cranelift
// `Variable` — params first, then each `let` in first-occurrence order,
// walking into if/while bodies too. Same flat, non-block-scoped locals
// story as kestrel.js's interpreter and bytecode VM (see kestrel.js's
// collectSlots / SYNTAX.md) — kept consistent across all three backends
// on purpose.
fn add_slot(name: &str, slots: &mut Vec<(String, usize)>, seen: &mut HashMap<String, usize>) {
    if !seen.contains_key(name) {
        let idx = slots.len();
        seen.insert(name.to_string(), idx);
        slots.push((name.to_string(), idx));
    }
}

fn walk_slots(stmts: &[Stmt], slots: &mut Vec<(String, usize)>, seen: &mut HashMap<String, usize>) {
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

fn collect_slots(f: &Fn) -> Vec<(String, usize)> {
    let mut slots: Vec<(String, usize)> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for p in &f.params {
        add_slot(&p.name, &mut slots, &mut seen);
    }
    walk_slots(&f.body, &mut slots, &mut seen);
    slots
}

struct FnCodegen<'a> {
    builder: FunctionBuilder<'a>,
    vars: HashMap<String, Variable>,
    fn_ids: &'a HashMap<String, FuncId>,
    printf_id: FuncId,
    module: &'a mut ObjectModule,
    str_cache: &'a mut HashMap<String, StrConst>,
    str_counter: &'a mut usize,
}

type CgResult<T> = Result<T, CodegenError>;

impl<'a> FnCodegen<'a> {
    /// Generates a statement sequence. Returns true if every path through
    /// it ends in a `return` (i.e. control can't fall off the end of it).
    fn gen_block(&mut self, stmts: &[Stmt]) -> CgResult<bool> {
        for s in stmts {
            if self.gen_stmt(s)? {
                return Ok(true); // rest of this block is unreachable
            }
        }
        Ok(false)
    }

    fn gen_stmt(&mut self, s: &Stmt) -> CgResult<bool> {
        match s {
            Stmt::Let { name, value } => {
                let v = self.gen_expr(value)?;
                self.builder.def_var(self.vars[name], v);
                Ok(false)
            }
            Stmt::Assign { name, value } => {
                let var = *self
                    .vars
                    .get(name)
                    .ok_or_else(|| CodegenError(format!("Assignment to unknown variable '{name}'")))?;
                let v = self.gen_expr(value)?;
                self.builder.def_var(var, v);
                Ok(false)
            }
            Stmt::If { cond, then_block, else_block } => {
                let c = self.gen_expr(cond)?;
                let then_blk = self.builder.create_block();
                let else_blk = self.builder.create_block();
                let merge_blk = self.builder.create_block();

                self.builder.ins().brif(c, then_blk, &[], else_blk, &[]);

                self.builder.switch_to_block(then_blk);
                let then_term = self.gen_block(then_block)?;
                if !then_term {
                    self.builder.ins().jump(merge_blk, &[]);
                }
                self.builder.seal_block(then_blk);

                self.builder.switch_to_block(else_blk);
                let else_term = if let Some(eb) = else_block {
                    self.gen_block(eb)?
                } else {
                    false
                };
                if !else_term {
                    self.builder.ins().jump(merge_blk, &[]);
                }
                self.builder.seal_block(else_blk);

                if then_term && else_term {
                    // merge_blk is unreachable — never switched to, so it's
                    // simply never appended to the function layout.
                    Ok(true)
                } else {
                    self.builder.switch_to_block(merge_blk);
                    self.builder.seal_block(merge_blk);
                    Ok(false)
                }
            }
            Stmt::While { cond, body } => {
                let header_blk = self.builder.create_block();
                let body_blk = self.builder.create_block();
                let after_blk = self.builder.create_block();

                self.builder.ins().jump(header_blk, &[]);

                self.builder.switch_to_block(header_blk);
                let c = self.gen_expr(cond)?;
                self.builder.ins().brif(c, body_blk, &[], after_blk, &[]);
                // header_blk is sealed after the body's back-edge is known.

                self.builder.switch_to_block(body_blk);
                let body_term = self.gen_block(body)?;
                if !body_term {
                    self.builder.ins().jump(header_blk, &[]);
                }
                self.builder.seal_block(body_blk);
                self.builder.seal_block(header_blk);

                self.builder.switch_to_block(after_blk);
                self.builder.seal_block(after_blk);
                Ok(false)
            }
            Stmt::Print { args } => {
                self.gen_print(args)?;
                Ok(false)
            }
            Stmt::Return { value } => {
                let v = match value {
                    Some(e) => self.gen_expr(e)?,
                    None => self.builder.ins().iconst(types::I64, 0),
                };
                self.builder.ins().return_(&[v]);
                Ok(true)
            }
            Stmt::ExprStmt { expr } => {
                self.gen_expr(expr)?;
                Ok(false)
            }
        }
    }

    fn gen_print(&mut self, args: &[Expr]) -> CgResult<()> {
        if args.is_empty() {
            let fmt = self.intern_str_owned("\n")?;
            self.call_printf(fmt, None)?;
            return Ok(());
        }
        for (i, arg) in args.iter().enumerate() {
            let is_last = i == args.len() - 1;
            match arg {
                Expr::Str(s) => {
                    let fmt_text = if is_last { format!("{s}\n") } else { format!("{s} ") };
                    let fmt = self.intern_str_owned(&fmt_text)?;
                    self.call_printf_str_literal(fmt)?;
                }
                other => {
                    let v = self.gen_expr(other)?;
                    let fmt_text = if is_last { "%lld\n" } else { "%lld " };
                    let fmt = self.intern_str_owned(fmt_text)?;
                    self.call_printf(fmt, Some(v))?;
                }
            }
        }
        Ok(())
    }

    fn intern_str_owned(&mut self, s: &str) -> CgResult<DataId> {
        if let Some(existing) = self.str_cache.get(s) {
            return Ok(existing.data_id);
        }
        let name = format!("__kstr_{}", self.str_counter);
        *self.str_counter += 1;
        let data_id = self
            .module
            .declare_data(&name, Linkage::Local, false, false)
            .map_err(|e| CodegenError(e.to_string()))?;
        let mut desc = DataDescription::new();
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        desc.define(bytes.into_boxed_slice());
        self.module
            .define_data(data_id, &desc)
            .map_err(|e| CodegenError(e.to_string()))?;
        self.str_cache.insert(s.to_string(), StrConst { data_id });
        Ok(data_id)
    }

    fn call_printf_str_literal(&mut self, fmt_data: DataId) -> CgResult<()> {
        // A literal-text format string with no %-specifier, so the
        // "argument" slot is unused. Pass 0 for it.
        self.call_printf(fmt_data, None)
    }

    fn call_printf(&mut self, fmt_data: DataId, arg: Option<cranelift_codegen::ir::Value>) -> CgResult<()> {
        let local_data = self.module.declare_data_in_func(fmt_data, self.builder.func);
        let fmt_ptr = self.builder.ins().symbol_value(types::I64, local_data);
        let arg_val = arg.unwrap_or_else(|| self.builder.ins().iconst(types::I64, 0));
        let local_printf = self.module.declare_func_in_func(self.printf_id, self.builder.func);
        self.builder.ins().call(local_printf, &[fmt_ptr, arg_val]);
        Ok(())
    }

    fn gen_expr(&mut self, e: &Expr) -> CgResult<cranelift_codegen::ir::Value> {
        use cranelift_codegen::ir::condcodes::IntCC;
        match e {
            Expr::Num(n) => Ok(self.builder.ins().iconst(types::I64, *n)),
            Expr::Bool(b) => Ok(self.builder.ins().iconst(types::I64, if *b { 1 } else { 0 })),
            Expr::Str(_) => Err(CodegenError(
                "kestrelc only supports string literals as direct print() arguments so far".into(),
            )),
            Expr::Ident(name) => {
                let var = self
                    .vars
                    .get(name)
                    .ok_or_else(|| CodegenError(format!("Unknown identifier '{name}'")))?;
                Ok(self.builder.use_var(*var))
            }
            Expr::ArrayLit(_) | Expr::Index { .. } => Err(CodegenError(
                "kestrelc doesn't support arrays yet — see kestrelc/README.md".into(),
            )),
            Expr::Unary { op, expr } => {
                let v = self.gen_expr(expr)?;
                match op {
                    UnOp::Neg => Ok(self.builder.ins().ineg(v)),
                    UnOp::Not => {
                        let zero = self.builder.ins().iconst(types::I64, 0);
                        let is_zero = self.builder.ins().icmp(IntCC::Equal, v, zero);
                        Ok(self.builder.ins().uextend(types::I64, is_zero))
                    }
                }
            }
            Expr::Binop { op, left, right } => {
                let l = self.gen_expr(left)?;
                let r = self.gen_expr(right)?;
                let result = match op {
                    BinOp::Add => self.builder.ins().iadd(l, r),
                    BinOp::Sub => self.builder.ins().isub(l, r),
                    BinOp::Mul => self.builder.ins().imul(l, r),
                    BinOp::Div => self.builder.ins().sdiv(l, r),
                    BinOp::Mod => self.builder.ins().srem(l, r),
                    BinOp::Eq => {
                        let c = self.builder.ins().icmp(IntCC::Equal, l, r);
                        self.builder.ins().uextend(types::I64, c)
                    }
                    BinOp::Neq => {
                        let c = self.builder.ins().icmp(IntCC::NotEqual, l, r);
                        self.builder.ins().uextend(types::I64, c)
                    }
                    BinOp::Lt => {
                        let c = self.builder.ins().icmp(IntCC::SignedLessThan, l, r);
                        self.builder.ins().uextend(types::I64, c)
                    }
                    BinOp::Gt => {
                        let c = self.builder.ins().icmp(IntCC::SignedGreaterThan, l, r);
                        self.builder.ins().uextend(types::I64, c)
                    }
                    BinOp::Le => {
                        let c = self.builder.ins().icmp(IntCC::SignedLessThanOrEqual, l, r);
                        self.builder.ins().uextend(types::I64, c)
                    }
                    BinOp::Ge => {
                        let c = self.builder.ins().icmp(IntCC::SignedGreaterThanOrEqual, l, r);
                        self.builder.ins().uextend(types::I64, c)
                    }
                    // Not short-circuiting, same as kestrel.js's interpreter/VM
                    // (both operands are always evaluated).
                    BinOp::And => self.builder.ins().band(l, r),
                    BinOp::Or => self.builder.ins().bor(l, r),
                };
                Ok(result)
            }
            Expr::Call { name, args } => {
                let func_id = *self
                    .fn_ids
                    .get(name)
                    .ok_or_else(|| CodegenError(format!("Unknown function '{name}'")))?;
                let mut arg_vals = Vec::with_capacity(args.len());
                for a in args {
                    arg_vals.push(self.gen_expr(a)?);
                }
                let local_func = self.module.declare_func_in_func(func_id, self.builder.func);
                let call = self.builder.ins().call(local_func, &arg_vals);
                Ok(self.builder.inst_results(call)[0])
            }
        }
    }
}
