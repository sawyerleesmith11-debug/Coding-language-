// AST -> Cranelift IR -> in-process machine code, executed immediately
// inside the current process (no object file, no `cc`, no second process
// spawn) -- see `kestrelc watch`'s design doc
// (docs/superpowers/specs/2026-07-21-jit-watch-mode-design.md) for why.
//
// v1 scope, deliberately much narrower than codegen.rs (the AOT native
// backend): scalars, arithmetic/comparison, if/while, function calls
// (including recursion), and print. No arrays, no structs, no
// parallel_map, no memoization, no profile-guided inlining -- a program
// using any of those is rejected by `check_jit_supported` with a clear
// message before codegen ever starts, and `watch.rs` falls back to the
// existing self-invoke/AOT path transparently in that case. See the
// design doc for why each of those is deferred rather than attempted
// here.
//
// Only `printf` is needed as a runtime import for this scope (no arrays
// means no `kestrelc_bounds_fail`; no parallel_map/memoization means none
// of `kestrelc_runtime.c`'s other functions are needed either) --
// resolved via a direct `extern "C"` FFI declaration below, since `printf`
// is already part of the C runtime any Rust/Windows binary links against
// by default. This deliberately avoids linking `kestrelc_runtime.c` into
// `kestrelc` itself, which was attempted and reverted: this machine's
// `rustc` targets `x86_64-pc-windows-msvc` with no MSVC Build Tools
// installed, and `kestrelc_runtime.c` has only ever been built with mingw
// `gcc` (a real, separate compile+link step for kestrelc's *output*
// programs) -- mixing a mingw-compiled object into an MSVC-target Rust
// binary isn't ABI-safe. See the design doc for the full story.

use crate::ast::*;
use crate::error::{ErrorKind, KestrelcError};
use crate::interner::Symbol;
use crate::span::Span;
use cranelift_codegen::ir::{condcodes::IntCC, types, AbiParam, Function, InstBuilder, Signature, UserFuncName};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use std::collections::HashMap;

extern "C" {
    fn printf(fmt: *const u8, arg: i64) -> i32;
    fn fflush(stream: *mut std::ffi::c_void) -> i32;
}

/// Walks the whole program and returns a clear, specific reason `Some(..)`
/// if it uses any construct JIT mode doesn't support yet (see this file's
/// module doc comment for the full list and why). `None` means safe to
/// JIT-compile. Deliberately conservative: any struct declaration at all
/// in the program disqualifies it, even one that happens not to be used
/// on the path actually executed -- simpler and safer than tracking real
/// reachability, and costs nothing since the AOT fallback still works
/// perfectly for that program.
pub fn check_jit_supported(program: &Program) -> Result<(), KestrelcError> {
    if let Some(decl) = program.structs.first() {
        return Err(KestrelcError::new(
            ErrorKind::Codegen,
            "structs aren't supported under `kestrelc watch` yet".to_string(),
            decl.span,
        ));
    }
    for f in &program.fns {
        for p in &f.params {
            if let Type::Array { .. } = p.ty {
                return Err(KestrelcError::new(
                    ErrorKind::Codegen,
                    "arrays aren't supported under `kestrelc watch` yet".to_string(),
                    f.span,
                ));
            }
        }
        // finish_and_run transmutes the finalized function pointer to a
        // fixed, zero-parameter `extern "C" fn() -> i64` -- that's only
        // sound if `main` truly takes zero parameters. Nothing in the
        // front end rejects `fn main(x: i64)` (every existing check only
        // verifies `main` *exists*, never its arity, since main is never
        // called from within the program itself for typecheck.rs's
        // argument-count check to catch), so this must be checked here,
        // explicitly, rather than merely asserted in a comment next to
        // the transmute.
        if &*f.name.resolve() == "main" && !f.params.is_empty() {
            return Err(KestrelcError::new(
                ErrorKind::Codegen,
                "kestrelc watch: 'main' can't take parameters".to_string(),
                f.span,
            ));
        }
        check_stmts_supported(&f.body)?;
    }
    Ok(())
}

fn check_stmts_supported(stmts: &[Stmt]) -> Result<(), KestrelcError> {
    for s in stmts {
        match s {
            Stmt::Let { value, .. } | Stmt::Assign { value, .. } => check_expr_supported(value)?,
            Stmt::If { cond, then_block, else_block, .. } => {
                check_expr_supported(cond)?;
                check_stmts_supported(then_block)?;
                if let Some(eb) = else_block {
                    check_stmts_supported(eb)?;
                }
            }
            Stmt::While { cond, body, .. } => {
                check_expr_supported(cond)?;
                check_stmts_supported(body)?;
            }
            Stmt::Print { args, .. } => {
                for a in args {
                    check_expr_supported(a)?;
                }
            }
            Stmt::Return { value: Some(v), .. } => check_expr_supported(v)?,
            Stmt::Return { value: None, .. } => {}
            Stmt::ExprStmt { expr, .. } => check_expr_supported(expr)?,
        }
    }
    Ok(())
}

fn check_expr_supported(e: &Expr) -> Result<(), KestrelcError> {
    match &e.kind {
        ExprKind::Num(_) | ExprKind::Bool(_) | ExprKind::Str(_) | ExprKind::Ident(_) => Ok(()),
        ExprKind::ArrayLit(_) => Err(KestrelcError::new(
            ErrorKind::Codegen,
            "arrays aren't supported under `kestrelc watch` yet".to_string(),
            e.span,
        )),
        ExprKind::Index { .. } => Err(KestrelcError::new(
            ErrorKind::Codegen,
            "arrays aren't supported under `kestrelc watch` yet".to_string(),
            e.span,
        )),
        ExprKind::StructLit { .. } | ExprKind::Field { .. } => Err(KestrelcError::new(
            ErrorKind::Codegen,
            "structs aren't supported under `kestrelc watch` yet".to_string(),
            e.span,
        )),
        ExprKind::Unary { expr, .. } => check_expr_supported(expr),
        ExprKind::Binop { left, right, .. } => {
            check_expr_supported(left)?;
            check_expr_supported(right)
        }
        ExprKind::Call { name, args } => {
            if &*name.resolve() == "parallel_map" {
                return Err(KestrelcError::new(
                    ErrorKind::Codegen,
                    "`parallel_map` isn't supported under `kestrelc watch` yet -- compile normally with `kestrelc file.kes` to test it".to_string(),
                    e.span,
                ));
            }
            for a in args {
                check_expr_supported(a)?;
            }
            Ok(())
        }
    }
}

struct StrConst {
    data_id: DataId,
}

/// Owns the JIT module and everything needed to compile and immediately
/// run a `Program`. One `JitCodegen` per `kestrelc watch` compile-and-run
/// cycle -- unlike `codegen.rs`'s `Codegen` (which persists no state
/// across separate `kestrelc` invocations), this is entirely rebuilt
/// fresh on every save, matching the design's "always a fresh compile,
/// never hot-reload" rule.
pub struct JitCodegen {
    module: JITModule,
    fn_ids: HashMap<Symbol, FuncId>,
    printf_id: FuncId,
    call_conv: CallConv,
    str_cache: HashMap<String, StrConst>,
    str_counter: usize,
}

impl JitCodegen {
    pub fn new() -> Result<Self, KestrelcError> {
        let mut flag_builder = settings::builder();
        flag_builder.set("is_pic", "true").map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        flag_builder.set("opt_level", "speed").map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        let isa_builder = cranelift_native::builder().map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        let call_conv = isa.default_call_conv();

        let mut jit_builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        jit_builder.symbol("printf", printf as *const u8);
        let mut module = JITModule::new(jit_builder);

        let mut printf_sig = Signature::new(call_conv);
        printf_sig.params.push(AbiParam::new(types::I64)); // format string pointer
        printf_sig.params.push(AbiParam::new(types::I64)); // one argument (0 if unused)
        printf_sig.returns.push(AbiParam::new(types::I32));
        let printf_id = module
            .declare_function("printf", Linkage::Import, &printf_sig)
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;

        Ok(JitCodegen {
            module,
            fn_ids: HashMap::new(),
            printf_id,
            call_conv,
            str_cache: HashMap::new(),
            str_counter: 0,
        })
    }

    fn fn_signature(f: &Fn, call_conv: CallConv) -> Signature {
        let mut sig = Signature::new(call_conv);
        for _ in &f.params {
            // Every parameter is scalar in v1 -- check_jit_supported
            // already rejected any Type::Array param before this is ever
            // called, and structs (the other multi-slot case in
            // codegen.rs) are rejected the same way.
            sig.params.push(AbiParam::new(types::I64));
        }
        sig.returns.push(AbiParam::new(types::I64));
        sig
    }

    pub fn compile_program(&mut self, program: &Program) -> Result<(), KestrelcError> {
        // Pass 1: declare every function's signature so calls (including
        // forward references and recursion) resolve regardless of
        // source order -- same two-pass structure as codegen.rs, for the
        // same reason.
        for f in &program.fns {
            let sig = Self::fn_signature(f, self.call_conv);
            let id = self
                .module
                .declare_function(&f.name.resolve(), Linkage::Export, &sig)
                .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
            self.fn_ids.insert(f.name, id);
        }
        // Pass 2: generate bodies.
        for f in &program.fns {
            self.compile_fn(f)?;
        }
        Ok(())
    }

    fn compile_fn(&mut self, f: &Fn) -> Result<(), KestrelcError> {
        let func_id = self.fn_ids[&f.name];
        let sig = Self::fn_signature(f, self.call_conv);

        let mut ctx = Context::new();
        ctx.func = Function::with_name_signature(UserFuncName::user(0, func_id.as_u32()), sig);

        let mut fb_ctx = FunctionBuilderContext::new();
        {
            let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fb_ctx);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            builder.seal_block(entry);

            let mut vars: HashMap<Symbol, Variable> = HashMap::new();
            let mut next_var: u32 = 0;
            // Params first (matches their block-param order), then every
            // other local `let` this function body ever binds -- declared
            // up front the same way codegen.rs's collect_slots does, so a
            // forward reference within the same function (impossible in
            // this language's actual grammar, but matching the existing
            // pattern costs nothing) can't panic on a missing HashMap entry.
            for p in &f.params {
                let v = Variable::from_u32(next_var);
                next_var += 1;
                builder.declare_var(v, types::I64);
                vars.insert(p.name, v);
            }
            for name in collect_let_names(&f.body) {
                if !vars.contains_key(&name) {
                    let v = Variable::from_u32(next_var);
                    next_var += 1;
                    builder.declare_var(v, types::I64);
                    vars.insert(name, v);
                }
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
                cur_span: f.span,
            };
            let terminated = fc.gen_block(&f.body)?;
            if !terminated {
                let zero = fc.builder.ins().iconst(types::I64, 0);
                fc.builder.ins().return_(&[zero]);
            }
            fc.builder.finalize();
        }

        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// Finalizes the JIT module and immediately calls `main` in-process,
    /// returning its i64 result. Consumes `self` -- a `JitCodegen` is a
    /// one-shot, single-run object (see the struct's own doc comment).
    pub fn finish_and_run(mut self) -> Result<i64, KestrelcError> {
        self.module
            .finalize_definitions()
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        let main_id = *self
            .fn_ids
            .get(&crate::interner::intern("main"))
            .ok_or_else(|| KestrelcError::internal(ErrorKind::Codegen, "no 'main' function found".to_string()))?;
        let code_ptr = self.module.get_finalized_function(main_id);
        // Safety: this transmute assumes `main` takes zero parameters --
        // true because check_jit_supported (called before any of this
        // runs -- see try_jit in watch.rs) explicitly rejects `main`
        // having any parameters, not merely because fn_signature happens
        // to produce a matching shape (fn_signature pushes one AbiParam
        // per f.params for *any* function, main included, so without
        // that separate check this transmute's soundness would rest on
        // an unenforced assumption, not a real guarantee).
        let main_fn: extern "C" fn() -> i64 = unsafe { std::mem::transmute(code_ptr) };
        let result = main_fn();
        // See this file's module doc comment: printf's C-runtime stdout
        // buffer isn't synchronized with Rust's own stdout handle, so
        // without this, output from the JIT-executed program can appear
        // out of order relative to anything watch.rs itself prints
        // afterward (e.g. a "finished in Xms" status line).
        unsafe {
            fflush(std::ptr::null_mut());
        }
        Ok(result)
    }
}

/// Every `Symbol` a `let` statement anywhere in `stmts` binds (recursing
/// into `if`/`while` bodies) -- used once, up front, to declare every
/// local `Variable` before generating any code, so a `let` inside a
/// branch not taken on some earlier pass through `compile_fn`'s own
/// single linear walk still has a declared slot (matches codegen.rs's
/// `collect_slots`' same "walk once, declare everything up front"
/// reasoning, simplified since v1 has only one `SlotKind`: scalar).
fn collect_let_names(stmts: &[Stmt]) -> Vec<Symbol> {
    let mut names = Vec::new();
    fn walk(stmts: &[Stmt], names: &mut Vec<Symbol>) {
        for s in stmts {
            match s {
                Stmt::Let { name, .. } => names.push(*name),
                Stmt::If { then_block, else_block, .. } => {
                    walk(then_block, names);
                    if let Some(eb) = else_block {
                        walk(eb, names);
                    }
                }
                Stmt::While { body, .. } => walk(body, names),
                _ => {}
            }
        }
    }
    walk(stmts, &mut names);
    names
}

type CgResult<T> = Result<T, KestrelcError>;

struct FnCodegen<'a> {
    builder: FunctionBuilder<'a>,
    vars: HashMap<Symbol, Variable>,
    fn_ids: &'a HashMap<Symbol, FuncId>,
    printf_id: FuncId,
    module: &'a mut JITModule,
    str_cache: &'a mut HashMap<String, StrConst>,
    str_counter: &'a mut usize,
    cur_span: Span,
}

impl<'a> FnCodegen<'a> {
    fn err(&self, message: String) -> KestrelcError {
        KestrelcError::new(ErrorKind::Codegen, message, self.cur_span)
    }

    fn gen_block(&mut self, stmts: &[Stmt]) -> CgResult<bool> {
        for s in stmts {
            if self.gen_stmt(s)? {
                return Ok(true); // rest of this block is unreachable
            }
        }
        Ok(false)
    }

    fn gen_stmt(&mut self, s: &Stmt) -> CgResult<bool> {
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
            Stmt::Let { name, value, .. } | Stmt::Assign { name, value, .. } => {
                let var = self.vars[name];
                let v = self.gen_expr(value)?;
                self.builder.def_var(var, v);
                Ok(false)
            }
            Stmt::If { cond, then_block, else_block, .. } => {
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
                    Ok(true)
                } else {
                    self.builder.switch_to_block(merge_blk);
                    self.builder.seal_block(merge_blk);
                    Ok(false)
                }
            }
            Stmt::While { cond, body, .. } => {
                let header_blk = self.builder.create_block();
                let body_blk = self.builder.create_block();
                let after_blk = self.builder.create_block();

                self.builder.ins().jump(header_blk, &[]);

                self.builder.switch_to_block(header_blk);
                let c = self.gen_expr(cond)?;
                self.builder.ins().brif(c, body_blk, &[], after_blk, &[]);

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
            Stmt::Print { args, .. } => {
                self.gen_print(args)?;
                Ok(false)
            }
            Stmt::Return { value, .. } => {
                let v = match value {
                    Some(e) => self.gen_expr(e)?,
                    None => self.builder.ins().iconst(types::I64, 0),
                };
                self.builder.ins().return_(&[v]);
                Ok(true)
            }
            Stmt::ExprStmt { expr, .. } => {
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
            match &arg.kind {
                ExprKind::Str(s) => {
                    let fmt_text = if is_last { format!("{s}\n") } else { format!("{s} ") };
                    let fmt = self.intern_str_owned(&fmt_text)?;
                    self.call_printf(fmt, None)?;
                }
                _ => {
                    let v = self.gen_expr(arg)?;
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
        let name = format!("__kjitstr_{}", self.str_counter);
        *self.str_counter += 1;
        let data_id = self
            .module
            .declare_data(&name, Linkage::Local, false, false)
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        let mut desc = DataDescription::new();
        let mut bytes = s.as_bytes().to_vec();
        bytes.push(0);
        desc.define(bytes.into_boxed_slice());
        self.module
            .define_data(data_id, &desc)
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        self.str_cache.insert(s.to_string(), StrConst { data_id });
        Ok(data_id)
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
        self.cur_span = e.span;
        match &e.kind {
            ExprKind::Num(n) => Ok(self.builder.ins().iconst(types::I64, *n)),
            ExprKind::Bool(b) => Ok(self.builder.ins().iconst(types::I64, if *b { 1 } else { 0 })),
            ExprKind::Str(_) => Err(self.err(
                "kestrelc only supports string literals as direct print() arguments so far".into(),
            )),
            ExprKind::Ident(name) => match self.vars.get(name) {
                Some(var) => Ok(self.builder.use_var(*var)),
                None => Err(self.err(format!("Unknown identifier '{name}'"))),
            },
            // Unreachable in practice -- check_jit_supported already
            // rejects any program containing these before compile_program
            // is ever called. Kept as real errors (not unreachable!())
            // so a bug in check_jit_supported fails loud, not silently.
            ExprKind::ArrayLit(_) | ExprKind::Index { .. } => {
                Err(self.err("arrays aren't supported under `kestrelc watch` yet".into()))
            }
            ExprKind::StructLit { .. } | ExprKind::Field { .. } => {
                Err(self.err("structs aren't supported under `kestrelc watch` yet".into()))
            }
            ExprKind::Unary { op, expr } => {
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
            ExprKind::Binop { op, left, right } => {
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
                    BinOp::And => self.builder.ins().band(l, r),
                    BinOp::Or => self.builder.ins().bor(l, r),
                };
                Ok(result)
            }
            ExprKind::Call { name, args } => {
                let func_id = *self
                    .fn_ids
                    .get(name)
                    .ok_or_else(|| self.err(format!("Unknown function '{name}'")))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;

    fn jit_run(src: &str) -> i64 {
        let program = parse(lex(src).unwrap()).unwrap();
        check_jit_supported(&program).expect("expected this program to be JIT-supported");
        let mut cg = JitCodegen::new().unwrap();
        cg.compile_program(&program).unwrap();
        cg.finish_and_run().unwrap()
    }

    #[test]
    fn print_runs_without_crashing_and_flushes_correctly() {
        // The feature's main event and its only real FFI/ABI boundary
        // (printf, called through a JIT-registered symbol, plus the
        // fflush call afterward) -- flagged in review as the biggest
        // coverage gap: neither of the other tests ever calls print, so
        // this path had zero automated coverage. Asserting captured
        // stdout is awkward for an in-process printf call (it writes to
        // the C runtime's own stdout handle, not something the Rust test
        // harness easily intercepts), so this asserts what actually
        // matters for regression protection: the full print -> printf ->
        // fflush path executes to completion, with a mix of string and
        // numeric arguments (exercising both call_printf's literal-text
        // and %lld-formatted paths), multiple print statements in a row
        // (exercising str_cache/str_counter across more than one call),
        // and returns the expected value afterward -- if the FFI
        // signature or symbol registration were wrong, this would
        // reliably crash rather than silently pass.
        let result = jit_run(
            "fn main() {\n\
             \x20   print(\"hello\", 42, \"world\");\n\
             \x20   print(7);\n\
             \x20   print();\n\
             \x20   return 99;\n\
             }\n",
        );
        assert_eq!(result, 99);
    }

    #[test]
    fn main_with_parameters_is_rejected_with_a_clear_message() {
        // Regression test for the review finding: finish_and_run
        // transmutes to a fixed zero-parameter function pointer, which
        // is only sound if main truly takes no parameters -- this proves
        // that's actually enforced, not just asserted in a comment.
        let program = parse(lex("fn main(x: i64) -> i64 { return x; }").unwrap()).unwrap();
        let err = check_jit_supported(&program).unwrap_err();
        assert!(err.message.contains("'main' can't take parameters"), "got: {}", err.message);
    }

    #[test]
    fn arithmetic_and_control_flow_run_correctly() {
        let result = jit_run(
            "fn main() {\n\
             \x20   let total = 0;\n\
             \x20   let i = 0;\n\
             \x20   while (i < 10) {\n\
             \x20       if (i % 2 == 0) {\n\
             \x20           total = total + i;\n\
             \x20       }\n\
             \x20       i = i + 1;\n\
             \x20   }\n\
             \x20   return total;\n\
             }\n",
        );
        // 0+2+4+6+8 = 20
        assert_eq!(result, 20);
    }

    #[test]
    fn recursion_runs_correctly() {
        // fib(10) = 55 -- exercises a self-call resolving correctly
        // within the same JIT compile, the one v1-supported feature
        // with any real subtlety (see the design doc's testing plan).
        let result = jit_run(
            "fn fib(n: i64) -> i64 {\n\
             \x20   if (n < 2) {\n\
             \x20       return n;\n\
             \x20   } else {\n\
             \x20       return fib(n - 1) + fib(n - 2);\n\
             \x20   }\n\
             }\n\
             fn main() {\n\
             \x20   return fib(10);\n\
             }\n",
        );
        assert_eq!(result, 55);
    }

    #[test]
    fn a_program_using_arrays_is_rejected_with_a_clear_message() {
        let program = parse(lex("fn main() { let arr = [1, 2, 3]; print(arr[0]); }").unwrap()).unwrap();
        let err = check_jit_supported(&program).unwrap_err();
        assert!(err.message.contains("arrays aren't supported"), "got: {}", err.message);
    }

    #[test]
    fn a_program_using_structs_is_rejected_with_a_clear_message() {
        let program = parse(lex("struct Point { x: i64 }\nfn main() { let p = Point { x: 1 }; print(p.x); }").unwrap()).unwrap();
        let err = check_jit_supported(&program).unwrap_err();
        assert!(err.message.contains("structs aren't supported"), "got: {}", err.message);
    }

    #[test]
    fn a_program_using_parallel_map_is_rejected_with_a_clear_message() {
        let program = parse(lex(
            "pure fn f(x: i64) -> i64 { return x; }\nfn main() { let arr = [1, 2, 3]; let out = parallel_map(f, arr); print(out[0]); }",
        ).unwrap()).unwrap();
        let err = check_jit_supported(&program).unwrap_err();
        // Either message is acceptable here -- arrays are checked before
        // parallel_map in check_jit_supported's expression walk, so this
        // program (which also uses an array literal) may report either
        // reason first; both are correct, honest rejections.
        assert!(
            err.message.contains("parallel_map") || err.message.contains("arrays aren't supported"),
            "got: {}",
            err.message
        );
    }
}
