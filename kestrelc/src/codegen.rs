// AST -> Cranelift IR -> object file. Scope for now (see
// kestrelc/README.md): every scalar runtime value is an i64 (numbers,
// and comparison/bool results as 0/1); arrays are a (pointer, length)
// pair; string literals are only supported directly as print()
// arguments, not as general values. Anything outside that scope is a
// clear compile error, not a silent miscompile.

use crate::ast::*;
use crate::error::{ErrorKind, KestrelcError};
use crate::span::Span;
use crate::where_info::{extract_where_info, WhereInfo};
use cranelift_codegen::ir::{
    condcodes::IntCC, types, AbiParam, Block, Function, InstBuilder, MemFlags, Signature, StackSlotData,
    StackSlotKind, TrapCode, UserFuncName, Value,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};
use std::collections::HashMap;

struct StrConst {
    data_id: DataId,
}

/// Set up once in `compile_program` when a compile-cache directory is
/// available (see cache::dir()) — the same "caching is always optional,
/// never load-bearing" rule as cache.rs: no cache dir just means no
/// profile is ever written, not a compile failure. `counters` holds one
/// writable, zero-initialized i64 data cell per function, incremented on
/// every entry to that function (see `compile_fn`); `entries` is the
/// same information in declaration order, plus each function's
/// interned-name data, used only once — by `main`'s epilogue — to emit
/// the actual `kestrelc_profile_record` calls that flush every counter
/// to disk right before the program actually exits.
struct ProfileState {
    path_data: DataId,
    path_len: usize,
    record_id: FuncId,
    counters: HashMap<String, DataId>,
    entries: Vec<(DataId, usize, DataId)>, // (name_data, name_len, counter_data), in declaration order
}

/// A function is only ever assigned a slot (see MEMO_MAX_ARGS/SLOTS
/// below and `compile_program`'s eligibility check) when it's provably
/// safe: `pure`, not `main`, never passed as `parallel_map`'s callback
/// argument anywhere in the program (see
/// `inline::collect_parallel_map_callbacks` — reused here for the
/// opposite reason inline.rs uses it: that set is exactly "functions
/// ever called from more than one OS thread," so excluding them is what
/// makes the runtime cache in `kestrelc_runtime.c` safe with zero
/// locking), and has only scalar (no array) parameters — arrays would
/// need per-element hashing this first pass doesn't implement. Always
/// active (unlike `ProfileState`, doesn't depend on a cache directory
/// existing) — `slots` is simply empty when there's nothing eligible.
struct MemoState {
    lookup_id: FuncId,
    store_id: FuncId,
    slots: HashMap<String, i32>,
}

/// Mirrors `KESTRELC_MEMO_MAX_ARGS` in kestrelc_runtime.c.
const MEMO_MAX_ARGS: usize = 4;
/// Mirrors `KESTRELC_MEMO_MAX_SLOTS` in kestrelc_runtime.c — a program
/// with more eligible functions than this just stops assigning slots
/// past the cap; those functions compile normally, just unmemoized.
const MEMO_MAX_SLOTS: usize = 64;

pub struct Codegen {
    module: ObjectModule,
    fn_ids: HashMap<String, FuncId>,
    printf_id: FuncId,
    pmap_id: FuncId,
    bounds_fail_id: FuncId,
    profile: Option<ProfileState>,
    memo: MemoState,
    str_cache: HashMap<String, StrConst>,
    str_counter: usize,
    where_info: HashMap<String, WhereInfo>,
    // The host's real C calling convention (System V on Linux/macOS,
    // Windows x64 on Windows — different register assignments for the
    // same argument list). Every signature below — Kestrel functions,
    // printf, the parallel_map runtime shim — must use this, not a
    // hardcoded convention, or generated code passes arguments in the
    // wrong registers for whatever `cc` actually links against on this
    // platform.
    call_conv: CallConv,
}

impl Codegen {
    /// `profile_path`: the absolute path this compiled binary's own
    /// profile-record calls should write to, if the compile cache
    /// directory is available (see cache::dir() / profile::profile_path)
    /// — `None` means "don't instrument at all," same optional-caching
    /// posture as everywhere else this cache touches codegen.
    pub fn new(profile_path: Option<String>) -> Result<Self, KestrelcError> {
        let mut flag_builder = settings::builder();
        flag_builder.set("is_pic", "true").map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        flag_builder.set("opt_level", "speed").map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        // Array literals are stack-allocated (see compile_expr's ArrayLit
        // case) with no upper size limit — a large one (e.g. a
        // several-thousand-element literal) can blow well past a single
        // 4KB page in one function's frame. Cranelift's `enable_probestack`
        // defaults to *off*, which is silently fine on platforms whose
        // stacks grow more forgivingly, but is a guaranteed
        // STATUS_ACCESS_VIOLATION crash on Windows: the OS relies on each
        // stack page being touched in order to lazily grow the stack, and
        // a big one-shot `sub rsp, N` skips straight past the guard page.
        // `inline` keeps the probe as plain generated instructions instead
        // of a call to an external `__probestack` symbol we'd otherwise
        // have to provide ourselves.
        flag_builder.set("enable_probestack", "true").map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        flag_builder.set("probestack_strategy", "inline").map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        let isa_builder = cranelift_native::builder().map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        let call_conv = isa.default_call_conv();

        let obj_builder = ObjectBuilder::new(
            isa,
            "kestrel_module",
            cranelift_module::default_libcall_names(),
        )
        .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
        let mut module = ObjectModule::new(obj_builder);

        // printf(fmt: i64 ptr, arg: i64) -> i32 — declared with a fixed,
        // non-variadic Cranelift signature. This works on the System V
        // x86-64 ABI because the AL-register convention that real C
        // varargs callers must honor only matters when *floating-point*
        // variadic arguments are passed; every Kestrel value here is a
        // plain integer/pointer, so a fixed-arity call site is safe.
        let mut printf_sig = Signature::new(call_conv);
        printf_sig.params.push(AbiParam::new(types::I64)); // format string pointer
        printf_sig.params.push(AbiParam::new(types::I64)); // one argument (0 used if unused)
        printf_sig.returns.push(AbiParam::new(types::I32));
        let printf_id = module
            .declare_function("printf", Linkage::Import, &printf_sig)
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;

        // kestrelc_parallel_map_i64(in: i64 ptr, len: i64, f: i64 fn ptr,
        // out: i64 ptr) -> () — the real thread-parallel implementation of
        // `parallel_map`, defined in runtime/kestrelc_runtime.c and linked
        // in by `link_and_report` alongside every native build. Every
        // argument here is a plain 8-byte value on the System V ABI
        // (pointers and i64s alike), so this is exactly as simple to
        // declare as `printf` above.
        let mut pmap_sig = Signature::new(call_conv);
        pmap_sig.params.push(AbiParam::new(types::I64)); // in ptr
        pmap_sig.params.push(AbiParam::new(types::I64)); // len
        pmap_sig.params.push(AbiParam::new(types::I64)); // f (function pointer)
        pmap_sig.params.push(AbiParam::new(types::I64)); // out ptr
        let pmap_id = module
            .declare_function("kestrelc_parallel_map_i64", Linkage::Import, &pmap_sig)
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;

        // kestrelc_bounds_fail(idx: i64, len: i64) -> ! — prints a
        // friendly message and exits, instead of the runtime bounds
        // check just trapping (SIGILL) with no indication of what went
        // wrong. Declared with a real (never-taken) return type since
        // Cranelift signatures don't have a native "never returns"
        // marker; the call site still emits a trap right after, purely
        // to satisfy "every block needs a terminator."
        let mut bounds_fail_sig = Signature::new(call_conv);
        bounds_fail_sig.params.push(AbiParam::new(types::I64)); // idx
        bounds_fail_sig.params.push(AbiParam::new(types::I64)); // len
        let bounds_fail_id = module
            .declare_function("kestrelc_bounds_fail", Linkage::Import, &bounds_fail_sig)
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;

        // kestrelc_profile_record(path: i64 ptr, path_len: i64, name: i64
        // ptr, name_len: i64, count: i64, is_first: i32) -> () — declared
        // unconditionally, same as the three functions above, whether or
        // not this compile actually ends up instrumented (`profile` may
        // still be None below); the C runtime shim always defines it, so
        // there's nothing conditional to gate at the linker level either.
        let mut profile_record_sig = Signature::new(call_conv);
        profile_record_sig.params.push(AbiParam::new(types::I64)); // path ptr
        profile_record_sig.params.push(AbiParam::new(types::I64)); // path len
        profile_record_sig.params.push(AbiParam::new(types::I64)); // name ptr
        profile_record_sig.params.push(AbiParam::new(types::I64)); // name len
        profile_record_sig.params.push(AbiParam::new(types::I64)); // count
        profile_record_sig.params.push(AbiParam::new(types::I32)); // is_first
        let profile_record_id = module
            .declare_function("kestrelc_profile_record", Linkage::Import, &profile_record_sig)
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;

        // kestrelc_memo_lookup(slot: i32, args: i64 ptr, nargs: i32, out:
        // i64 ptr) -> i32 (hit) and kestrelc_memo_store(slot: i32, args:
        // i64 ptr, nargs: i32, result: i64) -> () — declared
        // unconditionally like every other runtime import; `memo.slots`
        // below may still end up empty for a given program, in which
        // case these are simply never called.
        let mut memo_lookup_sig = Signature::new(call_conv);
        memo_lookup_sig.params.push(AbiParam::new(types::I32)); // slot
        memo_lookup_sig.params.push(AbiParam::new(types::I64)); // args ptr
        memo_lookup_sig.params.push(AbiParam::new(types::I32)); // nargs
        memo_lookup_sig.params.push(AbiParam::new(types::I64)); // out ptr
        memo_lookup_sig.returns.push(AbiParam::new(types::I32)); // hit
        let memo_lookup_id = module
            .declare_function("kestrelc_memo_lookup", Linkage::Import, &memo_lookup_sig)
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;

        let mut memo_store_sig = Signature::new(call_conv);
        memo_store_sig.params.push(AbiParam::new(types::I32)); // slot
        memo_store_sig.params.push(AbiParam::new(types::I64)); // args ptr
        memo_store_sig.params.push(AbiParam::new(types::I32)); // nargs
        memo_store_sig.params.push(AbiParam::new(types::I64)); // result
        let memo_store_id = module
            .declare_function("kestrelc_memo_store", Linkage::Import, &memo_store_sig)
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;

        let profile = match profile_path {
            Some(path) => {
                let path_bytes = path.into_bytes();
                let path_len = path_bytes.len();
                let path_data = module
                    .declare_data("__kprofile_path", Linkage::Local, false, false)
                    .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
                let mut desc = DataDescription::new();
                desc.define(path_bytes.into_boxed_slice());
                module.define_data(path_data, &desc).map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
                Some(ProfileState {
                    path_data,
                    path_len,
                    record_id: profile_record_id,
                    counters: HashMap::new(),
                    entries: Vec::new(),
                })
            }
            None => None,
        };

        Ok(Codegen {
            module,
            fn_ids: HashMap::new(),
            printf_id,
            pmap_id,
            bounds_fail_id,
            profile,
            memo: MemoState { lookup_id: memo_lookup_id, store_id: memo_store_id, slots: HashMap::new() },
            str_cache: HashMap::new(),
            str_counter: 0,
            where_info: HashMap::new(),
            call_conv,
        })
    }

    // Array-typed parameters occupy two i64 slots in the Cranelift
    // signature (pointer, then length) instead of one — see the Slot
    // enum below for why arrays need two Variables at all.
    fn fn_signature(program_fn: &Fn, call_conv: CallConv) -> Signature {
        let mut sig = Signature::new(call_conv);
        for p in &program_fn.params {
            match &p.ty {
                Type::Array { .. } => {
                    sig.params.push(AbiParam::new(types::I64)); // pointer
                    sig.params.push(AbiParam::new(types::I64)); // length
                }
                Type::Named(_) => sig.params.push(AbiParam::new(types::I64)),
            }
        }
        sig.returns.push(AbiParam::new(types::I64));
        sig
    }

    pub fn compile_program(&mut self, program: &Program) -> Result<(), KestrelcError> {
        let pmap_callbacks = crate::inline::collect_parallel_map_callbacks(program);
        let mut next_memo_slot: i32 = 0;

        // Pass 1: declare every function's signature so calls (including
        // forward references and recursion) can be resolved regardless
        // of source order.
        for f in program {
            if f.pure
                && f.name != "main"
                && !pmap_callbacks.contains(&f.name)
                && !f.params.is_empty()
                && f.params.len() <= MEMO_MAX_ARGS
                && f.params.iter().all(|p| matches!(p.ty, Type::Named(_)))
                && (next_memo_slot as usize) < MEMO_MAX_SLOTS
            {
                self.memo.slots.insert(f.name.clone(), next_memo_slot);
                next_memo_slot += 1;
            }
            let sig = Self::fn_signature(f, self.call_conv);
            let id = self
                .module
                .declare_function(&f.name, Linkage::Export, &sig)
                .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
            self.fn_ids.insert(f.name.clone(), id);
            if let Some(info) = extract_where_info(f) {
                self.where_info.insert(f.name.clone(), info);
            }
            if self.profile.is_some() {
                let counter_name = format!("__kprofile_counter_{}", f.name);
                let counter_data = self
                    .module
                    .declare_data(&counter_name, Linkage::Local, true, false)
                    .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
                let mut counter_desc = DataDescription::new();
                counter_desc.define_zeroinit(8);
                self.module
                    .define_data(counter_data, &counter_desc)
                    .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;

                let name_bytes = f.name.as_bytes().to_vec();
                let name_len = name_bytes.len();
                let name_id_str = format!("__kprofile_name_{}", f.name);
                let name_data = self
                    .module
                    .declare_data(&name_id_str, Linkage::Local, false, false)
                    .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;
                let mut name_desc = DataDescription::new();
                name_desc.define(name_bytes.into_boxed_slice());
                self.module.define_data(name_data, &name_desc).map_err(|e| KestrelcError::internal(ErrorKind::Codegen, e.to_string()))?;

                let profile = self.profile.as_mut().expect("checked is_some above");
                profile.counters.insert(f.name.clone(), counter_data);
                profile.entries.push((name_data, name_len, counter_data));
            }
        }

        // Pass 2: generate bodies.
        for f in program {
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

            let slot_kinds = collect_slots(f);
            let mut vars: HashMap<String, Slot> = HashMap::new();
            let mut next_var: u32 = 0;
            for (name, kind) in &slot_kinds {
                match kind {
                    SlotKind::Scalar => {
                        let v = Variable::from_u32(next_var);
                        next_var += 1;
                        builder.declare_var(v, types::I64);
                        vars.insert(name.clone(), Slot::Scalar(v));
                    }
                    SlotKind::Array { literal_len } => {
                        let ptr = Variable::from_u32(next_var);
                        let len = Variable::from_u32(next_var + 1);
                        next_var += 2;
                        builder.declare_var(ptr, types::I64);
                        builder.declare_var(len, types::I64);
                        vars.insert(name.clone(), Slot::Array { ptr, len, literal_len: *literal_len });
                    }
                }
            }
            let mut param_idx = 0usize;
            for p in &f.params {
                match &vars[&p.name] {
                    Slot::Scalar(v) => {
                        let val = builder.block_params(entry)[param_idx];
                        builder.def_var(*v, val);
                        param_idx += 1;
                    }
                    Slot::Array { ptr, len, .. } => {
                        let ptr_val = builder.block_params(entry)[param_idx];
                        let len_val = builder.block_params(entry)[param_idx + 1];
                        builder.def_var(*ptr, ptr_val);
                        builder.def_var(*len, len_val);
                        param_idx += 2;
                    }
                }
            }

            // Every function counts its own calls, not just `main` — a
            // plain non-atomic load/add/store at entry, right after
            // params are bound. Non-atomic on purpose: a call landing
            // concurrently from a parallel_map worker thread could race
            // and undercount by one, but this is profiling data feeding
            // the *next* compile's inlining heuristic (see inline.rs),
            // never something correctness depends on — worth the
            // simplicity of skipping a real atomic RMW instruction here.
            if let Some(profile) = &self.profile {
                if let Some(&counter_data) = profile.counters.get(&f.name) {
                    let local_counter = self.module.declare_data_in_func(counter_data, builder.func);
                    let addr = builder.ins().symbol_value(types::I64, local_counter);
                    let cur = builder.ins().load(types::I64, MemFlags::new(), addr, 0);
                    let inc = builder.ins().iadd_imm(cur, 1);
                    builder.ins().store(MemFlags::new(), inc, addr, 0);
                }
            }

            // Memoization: if this function was assigned a slot (see
            // compile_program's eligibility check), pack its scalar
            // params into a small stack buffer and ask the runtime cache
            // whether this exact argument list was already computed. A
            // hit returns immediately, skipping the body entirely; a
            // miss falls through into normal codegen below, with
            // `memo_args_ptr` remembered so the epilogue (below) can
            // store the freshly-computed result before the function
            // actually returns.
            let my_memo_slot = self.memo.slots.get(&f.name).copied();
            let mut memo_args_ptr: Option<Value> = None;
            if let Some(slot) = my_memo_slot {
                let nargs = f.params.len();
                let args_size = (nargs * 8) as u32;
                let args_slot = builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, args_size, 3));
                let args_ptr = builder.ins().stack_addr(types::I64, args_slot, 0);
                for (i, p) in f.params.iter().enumerate() {
                    if let Slot::Scalar(v) = &vars[&p.name] {
                        let val = builder.use_var(*v);
                        builder.ins().store(MemFlags::new(), val, args_ptr, (i * 8) as i32);
                    }
                }
                let out_slot = builder.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
                let out_ptr = builder.ins().stack_addr(types::I64, out_slot, 0);
                let slot_val = builder.ins().iconst(types::I32, slot as i64);
                let nargs_val = builder.ins().iconst(types::I32, nargs as i64);
                let local_lookup = self.module.declare_func_in_func(self.memo.lookup_id, builder.func);
                let call = builder.ins().call(local_lookup, &[slot_val, args_ptr, nargs_val, out_ptr]);
                let hit = builder.inst_results(call)[0];
                let zero32 = builder.ins().iconst(types::I32, 0);
                let is_hit = builder.ins().icmp(IntCC::NotEqual, hit, zero32);
                let hit_blk = builder.create_block();
                let miss_blk = builder.create_block();
                builder.ins().brif(is_hit, hit_blk, &[], miss_blk, &[]);

                builder.switch_to_block(hit_blk);
                builder.seal_block(hit_blk);
                let cached = builder.ins().load(types::I64, MemFlags::new(), out_ptr, 0);
                builder.ins().return_(&[cached]);

                builder.switch_to_block(miss_blk);
                builder.seal_block(miss_blk);
                memo_args_ptr = Some(args_ptr);
            }

            // `main` compiles straight to the linked binary's C `main`
            // (see main.rs's Linkage::Export by name) — it's the only
            // function whose return is "the process is about to exit,"
            // so it's the only place a profile flush can safely run
            // exactly once regardless of which `return` statement (or
            // none at all) actually ends the run. A memoized function's
            // every `return` needs the same "run something once, right
            // before the real return, no matter which return statement
            // fired" treatment (store the freshly-computed result), so
            // both cases redirect every `return` inside the function to
            // jump to `epilogue_blk` instead of returning directly (see
            // FnCodegen::gen_stmt's Return arm) — mutually exclusive
            // triggers (memoization eligibility excludes `main`), so
            // there's no case where both would need to run.
            let epilogue: Option<(Block, Variable)> =
                if (f.name == "main" && self.profile.is_some()) || my_memo_slot.is_some() {
                    let epilogue_blk = builder.create_block();
                    let ret_var = Variable::from_u32(next_var);
                    builder.declare_var(ret_var, types::I64);
                    Some((epilogue_blk, ret_var))
                } else {
                    None
                };

            let mut fc = FnCodegen {
                builder,
                vars,
                fn_ids: &self.fn_ids,
                printf_id: self.printf_id,
                pmap_id: self.pmap_id,
                bounds_fail_id: self.bounds_fail_id,
                module: &mut self.module,
                str_cache: &mut self.str_cache,
                str_counter: &mut self.str_counter,
                where_info: &self.where_info,
                my_where: self.where_info.get(&f.name),
                epilogue,
                cur_span: f.span,
            };
            let terminated = fc.gen_block(&f.body)?;
            if let Some((epilogue_blk, ret_var)) = fc.epilogue {
                if !terminated {
                    let zero = fc.builder.ins().iconst(types::I64, 0);
                    fc.builder.def_var(ret_var, zero);
                    fc.builder.ins().jump(epilogue_blk, &[]);
                }
                fc.builder.switch_to_block(epilogue_blk);
                fc.builder.seal_block(epilogue_blk);
                if f.name == "main" {
                    if let Some(profile) = &self.profile {
                        let local_path = fc.module.declare_data_in_func(profile.path_data, fc.builder.func);
                        let path_ptr = fc.builder.ins().symbol_value(types::I64, local_path);
                        let path_len_val = fc.builder.ins().iconst(types::I64, profile.path_len as i64);
                        let local_record = fc.module.declare_func_in_func(profile.record_id, fc.builder.func);
                        for (i, (name_data, name_len, counter_data)) in profile.entries.iter().enumerate() {
                            let local_name = fc.module.declare_data_in_func(*name_data, fc.builder.func);
                            let name_ptr = fc.builder.ins().symbol_value(types::I64, local_name);
                            let name_len_val = fc.builder.ins().iconst(types::I64, *name_len as i64);
                            let local_counter = fc.module.declare_data_in_func(*counter_data, fc.builder.func);
                            let counter_addr = fc.builder.ins().symbol_value(types::I64, local_counter);
                            let count_val = fc.builder.ins().load(types::I64, MemFlags::new(), counter_addr, 0);
                            let is_first = fc.builder.ins().iconst(types::I32, if i == 0 { 1 } else { 0 });
                            fc.builder.ins().call(
                                local_record,
                                &[path_ptr, path_len_val, name_ptr, name_len_val, count_val, is_first],
                            );
                        }
                    }
                } else if let Some(slot) = my_memo_slot {
                    // A cache miss reached here (a hit already returned
                    // directly from `hit_blk` above, before the body
                    // ever ran) — store the just-computed result under
                    // this exact argument list before actually
                    // returning, so the *next* call with the same
                    // arguments hits the cache instead of recomputing.
                    let args_ptr = memo_args_ptr.expect("memo epilogue implies memo_args_ptr was set");
                    let ret_val = fc.builder.use_var(ret_var);
                    let slot_val = fc.builder.ins().iconst(types::I32, slot as i64);
                    let nargs_val = fc.builder.ins().iconst(types::I32, f.params.len() as i64);
                    let local_store = fc.module.declare_func_in_func(self.memo.store_id, fc.builder.func);
                    fc.builder.ins().call(local_store, &[slot_val, args_ptr, nargs_val, ret_val]);
                }
                let ret_val = fc.builder.use_var(ret_var);
                fc.builder.ins().return_(&[ret_val]);
            } else if !terminated {
                let zero = fc.builder.ins().iconst(types::I64, 0);
                fc.builder.ins().return_(&[zero]);
            }
            fc.builder.finalize();
        }

        cranelift_codegen::verifier::verify_function(&ctx.func, self.module.isa())
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, format!("kestrelc codegen bug in '{}': {e}", f.name)))?;

        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| KestrelcError::internal(ErrorKind::Codegen, format!("failed to define '{}': {e}", f.name)))?;

        Ok(())
    }

    pub fn finish(self) -> Vec<u8> {
        self.module.finish().emit().expect("object emission failed")
    }
}

// Every distinct name a function's body ever binds gets Cranelift
// `Variable`(s) — params first, then each `let` in first-occurrence
// order, walking into if/while bodies too. Same flat, non-block-scoped
// locals story as kestrel.js's interpreter and bytecode VM (see
// kestrel.js's collectSlots / SYNTAX.md) — kept consistent across all
// three backends on purpose.
//
// A scalar gets one Variable. An array gets two — a base pointer and a
// length — since Cranelift Variables are single SSA values and an array
// isn't one. `literal_len` is `Some(n)` for a `let x = [a, b, c];` (the
// element count is known at compile time, from the literal), and `None`
// for an array-typed parameter (the length is only known at runtime,
// passed in by the caller).
#[derive(Clone, Copy)]
enum SlotKind {
    Scalar,
    Array { literal_len: Option<usize> },
}

enum Slot {
    Scalar(Variable),
    Array { ptr: Variable, len: Variable, literal_len: Option<usize> },
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
fn slot_kind_for_let(value: &Expr, known_lens: &HashMap<String, usize>) -> SlotKind {
    match value {
        Expr::ArrayLit(elems) => SlotKind::Array { literal_len: Some(elems.len()) },
        Expr::Call { name, args } if name == "parallel_map" && args.len() == 2 => {
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

fn add_slot(
    name: &str,
    kind: SlotKind,
    slots: &mut Vec<(String, SlotKind)>,
    seen: &mut HashMap<String, ()>,
) {
    if !seen.contains_key(name) {
        seen.insert(name.to_string(), ());
        slots.push((name.to_string(), kind));
    }
}

fn walk_slots(
    stmts: &[Stmt],
    slots: &mut Vec<(String, SlotKind)>,
    seen: &mut HashMap<String, ()>,
    known_lens: &mut HashMap<String, usize>,
) {
    for s in stmts {
        match s {
            Stmt::Let { name, value, .. } => {
                let kind = slot_kind_for_let(value, known_lens);
                if let SlotKind::Array { literal_len: Some(l) } = kind {
                    known_lens.insert(name.clone(), l);
                }
                add_slot(name, kind, slots, seen);
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

fn collect_slots(f: &Fn) -> Vec<(String, SlotKind)> {
    let mut slots: Vec<(String, SlotKind)> = Vec::new();
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut known_lens: HashMap<String, usize> = HashMap::new();
    for p in &f.params {
        add_slot(&p.name, slot_kind_for_param(&p.ty), &mut slots, &mut seen);
    }
    walk_slots(&f.body, &mut slots, &mut seen, &mut known_lens);
    slots
}

struct FnCodegen<'a> {
    builder: FunctionBuilder<'a>,
    vars: HashMap<String, Slot>,
    fn_ids: &'a HashMap<String, FuncId>,
    printf_id: FuncId,
    pmap_id: FuncId,
    bounds_fail_id: FuncId,
    module: &'a mut ObjectModule,
    str_cache: &'a mut HashMap<String, StrConst>,
    str_counter: &'a mut usize,
    where_info: &'a HashMap<String, WhereInfo>,
    /// This function's own `where` pair, if it has one recognized —
    /// lets its body trust the precondition (elide the check on
    /// `arr_param[idx_param]`) since every call site is required to
    /// prove it before the call is even allowed to compile.
    my_where: Option<&'a WhereInfo>,
    /// `Some((block, var))` only when compiling `main` with profiling
    /// active: every `return` jumps to `block` (having first stashed its
    /// value in `var`) instead of returning directly, so the profile
    /// flush calls Codegen::compile_fn emits there run exactly once no
    /// matter which return statement actually ends the program.
    epilogue: Option<(Block, Variable)>,
    /// The span of the statement `gen_stmt` is currently generating code
    /// for — updated at the top of every `gen_stmt` call (see there),
    /// read by `err()` below. Same statement-granularity tradeoff as
    /// purity.rs/typecheck.rs's checker errors: a codegen error anywhere
    /// inside one statement's expression tree points at that whole
    /// statement, not the exact sub-expression.
    cur_span: Span,
}

type CgResult<T> = Result<T, KestrelcError>;

impl<'a> FnCodegen<'a> {
    /// Builds a `KestrelcError` positioned at `cur_span` — a real span
    /// now (not just a bare line/col prefix), since `Stmt` carries a
    /// full `Span` (including `len`) already; main.rs renders it through
    /// the same `format_diagnostic` caret treatment lex/parse/checker
    /// errors get, closing what used to be a smaller "line:col: only"
    /// gap for native codegen errors.
    fn err(&self, message: String) -> KestrelcError {
        KestrelcError::new(ErrorKind::Codegen, message, self.cur_span)
    }
}

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

    /// Shared by `let` and `=`: binds `name` to `value`, handling both
    /// the scalar case (one Variable) and the array-literal case (stack
    /// allocation + one store per element, then the ptr/len Variables).
    fn gen_binding(&mut self, name: &str, value: &Expr) -> CgResult<()> {
        match (&self.vars[name], value) {
            (Slot::Scalar(var), _) => {
                let var = *var;
                let v = self.gen_expr(value)?;
                self.builder.def_var(var, v);
                Ok(())
            }
            (Slot::Array { ptr, len, literal_len }, Expr::ArrayLit(elems)) => {
                let (ptr, len) = (*ptr, *len);
                let expected = literal_len.expect("array let-bindings always have a literal_len");
                if elems.len() != expected {
                    // Only possible if the same name is bound to two
                    // differently-sized literals in different branches —
                    // collect_slots only records the first occurrence's size.
                    return Err(self.err(format!(
                        "kestrelc: array variable '{name}' rebound with a different length ({} vs {expected}) — not supported",
                        elems.len()
                    )));
                }
                let size_bytes = (elems.len() * 8) as u32;
                let ss = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    size_bytes,
                    3, // 8-byte (2^3) alignment for i64 elements
                ));
                let base = self.builder.ins().stack_addr(types::I64, ss, 0);
                for (i, el) in elems.iter().enumerate() {
                    let v = self.gen_expr(el)?;
                    self.builder.ins().store(MemFlags::new(), v, base, (i * 8) as i32);
                }
                let len_val = self.builder.ins().iconst(types::I64, elems.len() as i64);
                self.builder.def_var(ptr, base);
                self.builder.def_var(len, len_val);
                Ok(())
            }
            (Slot::Array { ptr, len, literal_len }, Expr::Call { name: call_name, args }) if call_name == "parallel_map" => {
                let (ptr, len) = (*ptr, *len);
                let func_name = match &args[0] {
                    Expr::Ident(n) => n.clone(),
                    _ => {
                        return Err(self.err(
                            "parallel_map()'s first argument must be a bare function name".into(),
                        ))
                    }
                };
                let callee_id = *self
                    .fn_ids
                    .get(&func_name)
                    .ok_or_else(|| self.err(format!("Unknown function '{func_name}'")))?;
                let elem_count = self.static_array_len(&args[1]).ok_or_else(|| {
                    self.err(
                        "kestrelc only supports parallel_map over a fixed-size array literal (`let x = [...]`) so far, not an array parameter".into(),
                    )
                })?;
                let expected = literal_len.expect("array let-bindings always have a literal_len");
                if elem_count != expected {
                    return Err(self.err(format!(
                        "kestrelc: array variable '{name}' rebound with a different length ({elem_count} vs {expected}) — not supported"
                    )));
                }
                let (in_ptr, _in_len) = self.resolve_array(&args[1])?;

                let size_bytes = (elem_count * 8) as u32;
                let out_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    size_bytes,
                    3,
                ));
                let out_base = self.builder.ins().stack_addr(types::I64, out_slot, 0);

                // The callee's own machine-code address, as a plain i64
                // value — this is what the C runtime shim
                // (kestrelc_parallel_map_i64) calls back into from
                // worker threads, once per array element. Safe because
                // parallel_map only accepts a `pure fn`: no shared
                // mutable state for concurrent calls to race over.
                let local_callee = self.module.declare_func_in_func(callee_id, self.builder.func);
                let func_addr = self.builder.ins().func_addr(types::I64, local_callee);

                let len_val = self.builder.ins().iconst(types::I64, elem_count as i64);
                let local_pmap = self.module.declare_func_in_func(self.pmap_id, self.builder.func);
                self.builder.ins().call(local_pmap, &[in_ptr, len_val, func_addr, out_base]);

                self.builder.def_var(ptr, out_base);
                self.builder.def_var(len, len_val);
                Ok(())
            }
            (Slot::Array { .. }, _) => Err(self.err(format!(
                "kestrelc: '{name}' is an array variable and can only be (re)bound to an array literal so far"
            ))),
        }
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
            Stmt::Let { name, value, .. } => {
                self.gen_binding(name, value)?;
                Ok(false)
            }
            Stmt::Assign { name, value, .. } => {
                if !self.vars.contains_key(name) {
                    return Err(self.err(format!("Assignment to unknown variable '{name}'")));
                }
                self.gen_binding(name, value)?;
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
                    // merge_blk is unreachable — never switched to, so it's
                    // simply never appended to the function layout.
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
            Stmt::Print { args, .. } => {
                self.gen_print(args)?;
                Ok(false)
            }
            Stmt::Return { value, .. } => {
                let v = match value {
                    Some(e) => self.gen_expr(e)?,
                    None => self.builder.ins().iconst(types::I64, 0),
                };
                if let Some((epilogue_blk, ret_var)) = self.epilogue {
                    self.builder.def_var(ret_var, v);
                    self.builder.ins().jump(epilogue_blk, &[]);
                } else {
                    self.builder.ins().return_(&[v]);
                }
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

    /// Resolves an expression that must denote an array to its (pointer,
    /// length) pair. Scope for now: only a plain identifier naming an
    /// array local/parameter — matches every array use in the example
    /// programs (`arr[i]`, `get_safe(nums, i)`), and gives a clear error
    /// for anything fancier (e.g. indexing the result of a call) rather
    /// than silently doing the wrong thing.
    fn resolve_array(&mut self, e: &Expr) -> CgResult<(Value, Value)> {
        let name = match e {
            Expr::Ident(name) => name,
            _ => {
                return Err(self.err(
                    "kestrelc only supports indexing/passing a plain array variable so far".into(),
                ))
            }
        };
        match self.vars.get(name) {
            Some(Slot::Array { ptr, len, .. }) => Ok((self.builder.use_var(*ptr), self.builder.use_var(*len))),
            Some(Slot::Scalar(_)) => Err(self.err(format!("'{name}' is not an array"))),
            None => Err(self.err(format!("Unknown identifier '{name}'"))),
        }
    }

    /// The array's element count, if it's known at compile time (i.e. a
    /// `let x = [literal, ...]` local — array *parameters* never have a
    /// compile-time-known length, since it arrives as a runtime value
    /// from the caller). Used only to decide whether a bounds check can
    /// be proven at compile time; doesn't affect the (ptr, len) values
    /// actually used at runtime.
    fn static_array_len(&self, e: &Expr) -> Option<usize> {
        match e {
            Expr::Ident(name) => match self.vars.get(name) {
                Some(Slot::Array { literal_len, .. }) => *literal_len,
                _ => None,
            },
            _ => None,
        }
    }

    fn gen_expr(&mut self, e: &Expr) -> CgResult<Value> {
        match e {
            Expr::Num(n) => Ok(self.builder.ins().iconst(types::I64, *n)),
            Expr::Bool(b) => Ok(self.builder.ins().iconst(types::I64, if *b { 1 } else { 0 })),
            Expr::Str(_) => Err(self.err(
                "kestrelc only supports string literals as direct print() arguments so far".into(),
            )),
            Expr::Ident(name) => match self.vars.get(name) {
                Some(Slot::Scalar(var)) => Ok(self.builder.use_var(*var)),
                Some(Slot::Array { .. }) => Err(self.err(format!(
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
                // `let` literal, not a parameter) can be proven safe — or
                // proven *unsafe* — right now, with no runtime check
                // needed either way.
                if let (Expr::Num(n), Some(static_len)) = (index.as_ref(), self.static_array_len(target)) {
                    if *n < 0 || *n as usize >= static_len {
                        return Err(self.err(format!(
                            "index {n} is out of bounds for array of length {static_len} — proven at compile time, not deferred to a runtime check"
                        )));
                    }
                    let (ptr, _len) = self.resolve_array(target)?;
                    return Ok(self.builder.ins().load(types::I64, MemFlags::new(), ptr, (*n * 8) as i32));
                }

                // Proof-carrying fast path #2: this function has a
                // `where idx_param < N` clause tying exactly this
                // (array parameter, index parameter) pair together, and
                // this is exactly that access (`arr_param[idx_param]`).
                // Every call site to this function is required (see the
                // Call arm below) to prove the precondition before the
                // call is even allowed to compile — so by the time we're
                // generating code *inside* this function, the precondition
                // is already guaranteed, and the check would be redundant.
                if let (Expr::Ident(t), Expr::Ident(i)) = (target.as_ref(), index.as_ref()) {
                    if let Some(w) = self.my_where {
                        if t == &w.arr_param && i == &w.idx_param {
                            let (ptr, _len) = self.resolve_array(target)?;
                            let idx = self.gen_expr(index)?;
                            let offset = self.builder.ins().imul_imm(idx, 8);
                            let addr = self.builder.ins().iadd(ptr, offset);
                            return Ok(self.builder.ins().load(types::I64, MemFlags::new(), addr, 0));
                        }
                    }
                }

                let (ptr, len) = self.resolve_array(target)?;
                let idx = self.gen_expr(index)?;

                let zero = self.builder.ins().iconst(types::I64, 0);
                let too_low = self.builder.ins().icmp(IntCC::SignedLessThan, idx, zero);
                let too_high = self.builder.ins().icmp(IntCC::SignedGreaterThanOrEqual, idx, len);
                let out_of_bounds = self.builder.ins().bor(too_low, too_high);

                let ok_blk = self.builder.create_block();
                let oob_blk = self.builder.create_block();
                self.builder.ins().brif(out_of_bounds, oob_blk, &[], ok_blk, &[]);

                self.builder.switch_to_block(oob_blk);
                self.builder.seal_block(oob_blk);
                // Matches run()/runFast()'s "always check" behavior, and now
                // also their friendly error message: kestrelc_bounds_fail
                // prints it and exits before this trap would ever actually
                // execute (Cranelift still requires a terminator here).
                let local_bounds_fail = self.module.declare_func_in_func(self.bounds_fail_id, self.builder.func);
                self.builder.ins().call(local_bounds_fail, &[idx, len]);
                self.builder.ins().trap(TrapCode::unwrap_user(1));

                self.builder.switch_to_block(ok_blk);
                self.builder.seal_block(ok_blk);
                let offset = self.builder.ins().imul_imm(idx, 8);
                let addr = self.builder.ins().iadd(ptr, offset);
                Ok(self.builder.ins().load(types::I64, MemFlags::new(), addr, 0))
            }
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
                    .ok_or_else(|| self.err(format!("Unknown function '{name}'")))?;

                // If the callee has a recognized `where idx < N` clause,
                // its precondition must be proven right here, at compile
                // time, before the call is allowed at all — matching
                // kestrel-DESIGN.md's own stated rule: "If the compiler
                // can't prove the where clause at a call site, it's a
                // compile error, not a runtime check." Our prover is
                // narrow (see kestrelc/README.md): only a literal index
                // against a literal-length array argument is provable;
                // anything else is rejected, not silently trusted.
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
                    if idx_lit < 0 || idx_lit as usize >= arr_len {
                        return Err(self.err(format!(
                            "kestrelc: call to '{name}' can't satisfy its own `where {} < N` clause — index {idx_lit} is out of bounds for an array of length {arr_len}",
                            w.idx_param
                        )));
                    }
                }

                let mut arg_vals = Vec::with_capacity(args.len());
                for a in args {
                    // An array argument expands to two Cranelift values
                    // (pointer, length), matching fn_signature's two
                    // AbiParams per array-typed parameter.
                    let is_array_ident = matches!(a, Expr::Ident(n) if matches!(self.vars.get(n), Some(Slot::Array { .. })));
                    if is_array_ident {
                        let (ptr, len) = self.resolve_array(a)?;
                        arg_vals.push(ptr);
                        arg_vals.push(len);
                    } else {
                        arg_vals.push(self.gen_expr(a)?);
                    }
                }
                let local_func = self.module.declare_func_in_func(func_id, self.builder.func);
                let call = self.builder.ins().call(local_func, &arg_vals);
                Ok(self.builder.inst_results(call)[0])
            }
        }
    }
}
