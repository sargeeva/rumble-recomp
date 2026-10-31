//! Translates the game's code to host code at load time with Cranelift, in
//! place of the Rust blocks `rumble-port recomp` generates ahead of time
//! (see docs/recomp.md, Codegen). Nothing derived from the game is built
//! into the executable: the translation runs on the user's machine from
//! the DOL it booted.
//!
//! It cuts the code into the same blocks as the Rust generator
//! ([`blocks::plan`]) and keeps the same contract with the dispatcher: one
//! host function per guest function, chaining between its blocks while the
//! budget lasts, handing back before a block that doesn't fit or would trap
//! on the FPU, and stopping where the dispatcher has to look (hardware
//! register accesses, interrupt and timer state, idle loops, leaving the
//! function).
//!
//! The common integer instructions (arithmetic, logic, rotates, compares,
//! loads and stores, branches, moves to and from LR/CTR/XER/CR), and the
//! floating-point and paired-single arithmetic, loads and stores, are
//! emitted as Cranelift IR. Loads and stores reach cached MEM1 and MEM2
//! directly and call into `Memory` for everything else (an FP load or store
//! there, or a quantized one of an integer type, calls its `ops` function). Every other instruction calls its
//! `ops` function through a small `extern "C"` shim, so the interpreter, the
//! Rust blocks and this all share one definition of it.
//!
//! Guest registers live in `Cpu`; every access is a load or store, which
//! Cranelift's alias analysis forwards within a block where it can (guest
//! state and guest RAM are given separate alias regions).

use cranelift_codegen::ir::{
    condcodes::{FloatCC, IntCC}, types, AbiParam, AliasRegionData, BlockArg, InstBuilder, MemFlagsData, SigRef,
    Signature, Value,
};
use cranelift_codegen::ir::{Block as Label, JumpTableData};
use cranelift_codegen::settings::Configurable;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::{settings, Context};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Module};
use rumble_recomp::analysis::Program;
use rumble_recomp::ppc::{Insn, Op};
use rumble_runtime::cpu::blocks::{self, BlockPlan, Blocks, FuncPlan, JitFn};
use rumble_runtime::cpu::{ops, Cpu, MSR_FP, SPR_CTR, SPR_GQR0, SPR_LR, SPR_XER};
use rumble_runtime::mem::{Fault, Memory};
use std::collections::HashMap;
use std::mem::offset_of;

/// What was translated, for the log.
#[derive(Default, Debug)]
pub struct Stats {
    pub functions: usize,
    pub blocks: usize,
    pub insns: usize,
    /// Instructions emitted as IR; the rest call their `ops` function.
    pub native: usize,
    /// Blocks in functions that differ from RAM, left to the interpreter.
    pub skipped: usize,
    pub code_bytes: usize,
    pub seconds: f64,
}

pub struct Translated {
    pub blocks: Blocks,
    pub stats: Stats,
    /// Each function's host address and guest start, for the profiler.
    pub funcs: Vec<(u64, u32)>,
}

/// What the translated code points into, kept alive with the block table.
struct Code {
    _module: JITModule,
    /// Never shrunk or grown, so the addresses in the code stay valid.
    _insns: Vec<Vec<Insn>>,
    _regions: Box<[[u64; 2]; 16]>,
}

/// Translates every function in `prog` that matches RAM.
pub fn translate(prog: &Program, mem: &mut Memory, max_len: u32) -> Result<Translated, String> {
    let t = std::time::Instant::now();
    let plan = blocks::plan(prog, max_len);
    let mut out = translate_plan(&plan, mem)?;
    out.stats.seconds = t.elapsed().as_secs_f64();
    Ok(out)
}

/// Translates the planned functions that match RAM.
pub fn translate_plan(plan: &[FuncPlan], mem: &mut Memory) -> Result<Translated, String> {
    let t = std::time::Instant::now();
    let bad = blocks::differing_functions(
        mem,
        plan.iter().flat_map(|f| f.blocks.iter().map(|b| (b.start, b.body.len() as u32, b.hash(), f.start))),
    );
    let mut stats = Stats::default();

    // Host base and size of each 256 MB region of the address space that
    // has a direct path: the cached mirrors of MEM1 and MEM2. Size 0 sends
    // the access to `Memory`.
    let mut regions = Box::new([[0u64; 2]; 16]);
    let [(mem1, mem1_len), (mem2, mem2_len)] = mem.ram_host();
    regions[0x8] = [mem1 as u64, mem1_len as u64];
    regions[0x9] = [mem2 as u64, mem2_len as u64];

    let mut flags = settings::builder();
    flags.set("opt_level", "speed").map_err(|e| e.to_string())?;
    let isa = cranelift_native::builder()
        .map_err(|e| e.to_string())?
        .finish(settings::Flags::new(flags))
        .map_err(|e| e.to_string())?;
    let mut module = JITModule::new(JITBuilder::with_isa(isa.clone(), default_libcall_names()));
    let ptr = module.target_config().pointer_type();
    let call_conv = isa.default_call_conv();
    let sig = |params: &[types::Type], ret: types::Type| {
        let mut s = Signature::new(call_conv);
        s.params.extend(params.iter().map(|&t| AbiParam::new(t)));
        s.returns.push(AbiParam::new(ret));
        s
    };
    let sigs = Sigs {
        func: sig(&[ptr, ptr], types::I64),
        shim: sig(&[ptr, ptr, ptr], types::I64),
        read: sig(&[ptr, types::I32], types::I64),
        write: sig(&[ptr, types::I32, types::I32], types::I64),
    };

    let good: Vec<&FuncPlan> = plan.iter().filter(|f| !bad.contains(&f.start)).collect();
    stats.skipped = plan.iter().filter(|f| bad.contains(&f.start)).map(|f| f.blocks.len()).sum();

    // Functions are compiled on every core, each to bytes: the code calls
    // and points to absolute addresses only, so it needs no relocation.
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(good.len().max(1));
    let compiled: Vec<Result<(Vec<Compiled>, Stats), String>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (good, sigs, regions, isa) = (&good, &sigs, &regions, &isa);
                s.spawn(move || -> Result<(Vec<Compiled>, Stats), String> {
                    let mut out = Vec::new();
                    let mut stats = Stats::default();
                    let mut ctx = Context::new();
                    let mut fctx = FunctionBuilderContext::new();
                    let mut cp = ControlPlane::default();
                    for (k, f) in good.iter().enumerate().skip(t).step_by(threads) {
                        // Instructions handed to `ops` functions by address:
                        // the arena is sized up front so it never moves.
                        let mut arena: Vec<Insn> = Vec::with_capacity(f.blocks.iter().map(|b| b.body.len()).sum());
                        ctx.func.signature = sigs.func.clone();
                        {
                            let b = FunctionBuilder::new(&mut ctx.func, &mut fctx);
                            let mut tr = Tr::new(b, sigs, regions, &mut arena, ptr);
                            tr.function(f, &mut stats);
                            tr.b.finalize(isa.frontend_config());
                        }
                        let code = ctx
                            .compile(&**isa, &mut cp)
                            .map_err(|e| format!("translating fn_{:08x}: {:?}", f.start, e.inner))?;
                        if !code.buffer.relocs().is_empty() {
                            return Err(format!("fn_{:08x} needs relocations", f.start));
                        }
                        out.push(Compiled {
                            index: k,
                            bytes: code.code_buffer().to_vec(),
                            align: code.buffer.min_alignment as u64,
                            arena,
                        });
                        ctx.clear();
                    }
                    Ok((out, stats))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("translation thread panicked".into()))).collect()
    });
    let mut all = Vec::new();
    for r in compiled {
        let (out, st) = r?;
        stats.blocks += st.blocks;
        stats.insns += st.insns;
        stats.native += st.native;
        all.extend(out);
    }
    all.sort_by_key(|c| c.index);

    let mut ids = Vec::new();
    let mut arenas = Vec::new();
    for c in all {
        let id = module.declare_anonymous_function(&sigs.func).map_err(|e| e.to_string())?;
        module.define_function_bytes(id, c.align, &c.bytes, &[]).map_err(|e| e.to_string())?;
        stats.code_bytes += c.bytes.len();
        stats.functions += 1;
        ids.push((good[c.index], id));
        arenas.push(c.arena);
    }
    module.finalize_definitions().map_err(|e| e.to_string())?;

    let mut entries = Vec::new();
    let mut funcs = Vec::new();
    for (f, id) in ids {
        let code = module.get_finalized_function(id);
        // SAFETY: compiled with the `JitFn` signature.
        let run: JitFn = unsafe { std::mem::transmute(code) };
        funcs.push((code as u64, f.start));
        for b in &f.blocks {
            entries.push((b.start, run));
        }
    }
    let code = Code { _module: module, _insns: arenas, _regions: regions };
    stats.seconds = t.elapsed().as_secs_f64();
    Ok(Translated { blocks: Blocks::jit(entries, Box::new(code)), stats, funcs })
}

/// A function compiled on a worker thread.
struct Compiled {
    /// Its place in the list of functions translated.
    index: usize,
    bytes: Vec<u8>,
    align: u64,
    arena: Vec<Insn>,
}

struct Sigs {
    func: Signature,
    shim: Signature,
    read: Signature,
    write: Signature,
}

/// An `ops` function behind the C ABI: 0, or a fault as [`JitFn`] returns
/// it.
type Shim = extern "C" fn(*mut Cpu, *mut Memory, *const Insn) -> u64;

fn status(r: Result<(), Fault>) -> u64 {
    match r {
        Ok(()) => 0,
        Err(f) => fault(f),
    }
}

fn fault(f: Fault) -> u64 {
    match f {
        Fault::Unmapped(a) => 1 << 32 | a as u64,
    }
}

macro_rules! shims {
    ($($f:ident)*) => {
        /// The shim for the `ops` function named `name` (as `ops::handler`
        /// gives it).
        fn shim(name: &str) -> Option<Shim> {
            $(
                extern "C" fn $f(c: *mut Cpu, m: *mut Memory, i: *const Insn) -> u64 {
                    // SAFETY: called by translated code with the pointers it
                    // was given and an instruction from its arena.
                    status(unsafe { ops::$f(&mut *c, &mut *m, &*i) })
                }
            )*
            match name {
                $(stringify!($f) => Some($f as Shim),)*
                _ => None,
            }
        }
    };
}

shims! {
    addi addis addic subfic mulli add add_carry subf neg mullw mulhw mulhwu divw divwu cmpi cmpli cmp cmpl nop
    ori oris xori xoris andi_rc andis_rc logical slw srw sraw cntlzw extsb extsh rlwinm rlwnm rlwimi load store
    lmw stmw string lfs lfd stfs stfd stfiwx psq_l psq_st fmr fneg fabs fnabs frsp fctiw fsel fres frsqrte fctiwz
    fp_arith fcmp ps_mr ps_neg ps_abs ps_nabs ps_merge ps_arith ps_sum0 ps_sum1 ps_cmp mffs mcrfs mtfsf mtfsb0
    mtfsb1 mtfsfi b bc bclr bcctr rfi sc mcrf cr_logic mfcr mtcrf mcrxr mfmsr mtmsr mfspr mtspr mftb mfsr dcbz
}

// Slow paths for loads and stores outside the cached mirrors: a value, or
// a fault as `JitFn` returns it.

macro_rules! accessors {
    ($($read:ident $write:ident $r:ident $w:ident $t:ty;)*) => {$(
        extern "C" fn $read(m: *mut Memory, a: u32) -> u64 {
            // SAFETY: called by translated code with its `Memory`.
            match unsafe { &mut *m }.$r(a) {
                Ok(v) => v as u64,
                Err(f) => fault(f),
            }
        }

        extern "C" fn $write(m: *mut Memory, a: u32, v: u32) -> u64 {
            // SAFETY: as above.
            status(unsafe { &mut *m }.$w(a, v as $t))
        }
    )*};
}

accessors! {
    read8 write8 r8 w8 u8;
    read16 write16 r16 w16 u16;
    read32 write32 r32 w32 u32;
}

const GPR: i32 = offset_of!(Cpu, gpr) as i32;
const FPR: i32 = offset_of!(Cpu, fpr) as i32;
const FPSCR: i32 = offset_of!(Cpu, fpscr) as i32;
const CR: i32 = offset_of!(Cpu, cr) as i32;
const MSR: i32 = offset_of!(Cpu, msr) as i32;
const PC: i32 = offset_of!(Cpu, pc) as i32;
const SPR: i32 = offset_of!(Cpu, spr) as i32;
const BUDGET: i32 = offset_of!(Cpu, budget) as i32;
const IDLE: i32 = offset_of!(Cpu, idle_branch) as i32;
const MMIO_TOUCHED: i32 = offset_of!(Memory, mmio_touched) as i32;

const XER_CA: u32 = 1 << 29;

/// Where a block goes next.
enum Next {
    /// A known address.
    To(u32),
    /// `taken` if the condition holds, else `fall`.
    Cond(Value, u32, u32),
    /// An address only known at run time.
    Dyn(Value),
}

/// Translates one guest function.
struct Tr<'a, 'b> {
    b: FunctionBuilder<'a>,
    cpu: Value,
    mem: Value,
    /// `Cpu::spr`'s boxed array.
    spr: Value,
    budget: Variable,
    /// Flags for guest state (`Cpu`), guest RAM, `Memory` and the region
    /// table.
    fc: MemFlagsData,
    fr: MemFlagsData,
    fm: MemFlagsData,
    ft: MemFlagsData,
    shim_sig: SigRef,
    read_sig: SigRef,
    write_sig: SigRef,
    regions: u64,
    arena: &'b mut Vec<Insn>,
    ptr: types::Type,
    /// Returns 0 / the status in its parameter, writing the budget back.
    ret_ok: Label,
    ret_fault: Label,
    dispatch: Label,
    labels: HashMap<u32, Label>,
}

impl<'a, 'b> Tr<'a, 'b> {
    fn new(
        mut b: FunctionBuilder<'a>,
        sigs: &Sigs,
        regions: &[[u64; 2]; 16],
        arena: &'b mut Vec<Insn>,
        ptr: types::Type,
    ) -> Self {
        let dfg = &mut b.func.dfg;
        let cpu_region = dfg.alias_regions.insert(AliasRegionData { user_id: 1, description: "cpu".into() });
        let ram_region = dfg.alias_regions.insert(AliasRegionData { user_id: 2, description: "guest ram".into() });
        let mem_region = dfg.alias_regions.insert(AliasRegionData { user_id: 3, description: "memory".into() });
        let fc = MemFlagsData::trusted().with_alias_region(Some(cpu_region));
        let fr = MemFlagsData::new().with_notrap().with_alias_region(Some(ram_region));
        let fm = MemFlagsData::trusted().with_alias_region(Some(mem_region));
        let ft = MemFlagsData::trusted().with_readonly().with_can_move();
        let shim_sig = b.import_signature(sigs.shim.clone());
        let read_sig = b.import_signature(sigs.read.clone());
        let write_sig = b.import_signature(sigs.write.clone());

        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let cpu = b.block_params(entry)[0];
        let mem = b.block_params(entry)[1];
        let spr = b.ins().load(ptr, fc, cpu, SPR);
        let budget = b.declare_var(types::I32);
        let v = b.ins().load(types::I32, fc, cpu, BUDGET);
        b.def_var(budget, v);

        let ret_ok = b.create_block();
        let ret_fault = b.create_block();
        b.append_block_param(ret_fault, types::I64);
        b.set_cold_block(ret_fault);
        let dispatch = b.create_block();
        b.ins().jump(dispatch, &[]);

        Tr {
            b,
            cpu,
            mem,
            spr,
            budget,
            fc,
            fr,
            fm,
            ft,
            shim_sig,
            read_sig,
            write_sig,
            regions: regions.as_ptr() as u64,
            arena,
            ptr,
            ret_ok,
            ret_fault,
            dispatch,
            labels: HashMap::new(),
        }
    }

    fn function(&mut self, f: &FuncPlan, stats: &mut Stats) {
        for blk in &f.blocks {
            let l = self.b.create_block();
            self.labels.insert(blk.start, l);
        }

        // The entry, and indirect jumps within the function, go by `c.pc`
        // through a jump table over the function's words.
        self.b.switch_to_block(self.dispatch);
        let pc = self.b.ins().load(types::I32, self.fc, self.cpu, PC);
        let last = f.blocks.last().unwrap().start;
        let words = ((last - f.start) / 4 + 1) as usize;
        let mut targets = vec![self.ret_ok; words];
        for blk in &f.blocks {
            targets[((blk.start - f.start) / 4) as usize] = self.labels[&blk.start];
        }
        // Each entry needs a block call of its own.
        let table: Vec<_> = targets.iter().map(|&l| self.b.func.dfg.block_call(l, &[])).collect();
        let default = self.b.func.dfg.block_call(self.ret_ok, &[]);
        let jt = self.b.create_jump_table(JumpTableData::new(default, &table));
        let off = self.b.ins().iadd_imm_s(pc, -(f.start as i32 as i64));
        let idx = self.b.ins().ushr_imm_s(off, 2);
        // A PC below the function wraps to an index far past the table.
        self.b.ins().br_table(idx, jt);

        for blk in &f.blocks {
            self.block(blk, stats);
        }

        self.b.switch_to_block(self.ret_ok);
        let budget = self.b.use_var(self.budget);
        self.b.ins().store(self.fc, budget, self.cpu, BUDGET);
        let zero = self.b.ins().iconst(types::I64, 0);
        self.b.ins().return_(&[zero]);

        self.b.switch_to_block(self.ret_fault);
        let budget = self.b.use_var(self.budget);
        self.b.ins().store(self.fc, budget, self.cpu, BUDGET);
        let status = self.b.block_params(self.ret_fault)[0];
        self.b.ins().return_(&[status]);

        self.b.seal_all_blocks();
    }

    fn block(&mut self, blk: &BlockPlan, stats: &mut Stats) {
        let len = blk.body.len() as i64;
        stats.blocks += 1;
        stats.insns += blk.body.len();
        self.b.switch_to_block(self.labels[&blk.start]);
        // Hand back before running if the block doesn't fit the budget, or
        // would trap on the FPU (the interpreter raises that exactly).
        let budget = self.b.use_var(self.budget);
        let mut stop = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, budget, len);
        if blk.fpu() {
            let msr = self.b.ins().load(types::I32, self.fc, self.cpu, MSR);
            let fp = self.b.ins().band_imm_s(msr, MSR_FP as i64);
            let off = self.b.ins().icmp_imm_s(IntCC::Equal, fp, 0);
            stop = self.b.ins().bor(stop, off);
        }
        let body = self.b.create_block();
        self.b.ins().brif(stop, self.ret_ok, &[], body, &[]);
        self.b.switch_to_block(body);

        let mut touches = false;
        let mut next = None;
        for i in &blk.body {
            match self.native(i) {
                Some(n) => {
                    stats.native += 1;
                    touches |= is_memory(i.op);
                    next = n;
                }
                None => {
                    touches = true;
                    next = self.call_op(i);
                }
            }
        }
        let last = blk.body.last().unwrap();
        let next = next.unwrap_or(Next::To(last.addr + 4));

        let pc = match next {
            Next::To(a) => self.b.ins().iconst(types::I32, a as i64),
            Next::Cond(c, t, f) => {
                let t = self.b.ins().iconst(types::I32, t as i64);
                let f = self.b.ins().iconst(types::I32, f as i64);
                self.b.ins().select(c, t, f)
            }
            Next::Dyn(v) => v,
        };
        self.b.ins().store(self.fc, pc, self.cpu, PC);
        let budget = self.b.use_var(self.budget);
        let budget = self.b.ins().iadd_imm_s(budget, -len);
        self.b.def_var(self.budget, budget);

        if blk.is_idle_loop() {
            let a = self.b.ins().iconst(types::I32, last.addr as i64);
            self.b.ins().store(self.fc, a, self.cpu, IDLE);
            self.b.ins().jump(self.ret_ok, &[]);
            return;
        }
        if blk.stops_chain() {
            self.b.ins().jump(self.ret_ok, &[]);
            return;
        }
        if touches {
            // A hardware register access may have raised an interrupt.
            let t = self.b.ins().load(types::I8, self.fm, self.mem, MMIO_TOUCHED);
            let go = self.b.create_block();
            self.b.ins().brif(t, self.ret_ok, &[], go, &[]);
            self.b.switch_to_block(go);
        }
        match next {
            Next::To(a) => {
                let l = self.target(a);
                self.b.ins().jump(l, &[]);
            }
            Next::Cond(c, t, f) => {
                let (t, f) = (self.target(t), self.target(f));
                self.b.ins().brif(c, t, &[], f, &[]);
            }
            Next::Dyn(_) => {
                self.b.ins().jump(self.dispatch, &[]);
            }
        }
    }

    /// The block starting at `a` in this function, or the exit.
    fn target(&self, a: u32) -> Label {
        self.labels.get(&a).copied().unwrap_or(self.ret_ok)
    }

    // --- guest state ---

    fn gpr(&mut self, r: u32) -> Value {
        self.b.ins().load(types::I32, self.fc, self.cpu, GPR + 4 * r as i32)
    }

    /// rA, or 0 for r0.
    fn gpr0(&mut self, r: u32) -> Value {
        if r == 0 { self.b.ins().iconst(types::I32, 0) } else { self.gpr(r) }
    }

    fn set_gpr(&mut self, r: u32, v: Value) {
        self.b.ins().store(self.fc, v, self.cpu, GPR + 4 * r as i32);
    }

    fn spr_get(&mut self, n: usize) -> Value {
        self.b.ins().load(types::I32, self.fc, self.spr, 4 * n as i32)
    }

    fn spr_set(&mut self, n: usize, v: Value) {
        self.b.ins().store(self.fc, v, self.spr, 4 * n as i32);
    }

    fn cr(&mut self) -> Value {
        self.b.ins().load(types::I32, self.fc, self.cpu, CR)
    }

    fn set_cr(&mut self, v: Value) {
        self.b.ins().store(self.fc, v, self.cpu, CR);
    }

    fn set_cr_field(&mut self, field: u32, f: Value) {
        let sh = 28 - 4 * field;
        let cr = self.cr();
        let cleared = self.b.ins().band_imm_s(cr, !(0xfu32 << sh) as i32 as i64);
        let f = self.b.ins().ishl_imm_s(f, sh as i64);
        let v = self.b.ins().bor(cleared, f);
        self.set_cr(v);
    }

    /// The CR field for a compare: lt, gt, eq, and SO from XER.
    fn cmp_flags(&mut self, lt: Value, gt: Value, eq: Value) -> Value {
        let lt = self.b.ins().uextend(types::I32, lt);
        let gt = self.b.ins().uextend(types::I32, gt);
        let eq = self.b.ins().uextend(types::I32, eq);
        let xer = self.spr_get(SPR_XER);
        let so = self.b.ins().ushr_imm_s(xer, 31);
        let lt = self.b.ins().ishl_imm_s(lt, 3);
        let gt = self.b.ins().ishl_imm_s(gt, 2);
        let eq = self.b.ins().ishl_imm_s(eq, 1);
        let a = self.b.ins().bor(lt, gt);
        let b = self.b.ins().bor(eq, so);
        self.b.ins().bor(a, b)
    }

    fn compare(&mut self, field: u32, a: Value, b: Value, signed: bool) {
        let (lt, gt) = if signed {
            (IntCC::SignedLessThan, IntCC::SignedGreaterThan)
        } else {
            (IntCC::UnsignedLessThan, IntCC::UnsignedGreaterThan)
        };
        let lt = self.b.ins().icmp(lt, a, b);
        let gt = self.b.ins().icmp(gt, a, b);
        let eq = self.b.ins().icmp(IntCC::Equal, a, b);
        let f = self.cmp_flags(lt, gt, eq);
        self.set_cr_field(field, f);
    }

    /// CR0 from a result (Rc).
    fn record(&mut self, v: Value) {
        let zero = self.b.ins().iconst(types::I32, 0);
        self.compare(0, v, zero, true);
    }

    fn set_ca(&mut self, ca: Value) {
        let xer = self.spr_get(SPR_XER);
        let cleared = self.b.ins().band_imm_s(xer, !XER_CA as i32 as i64);
        let ca = self.b.ins().uextend(types::I32, ca);
        let ca = self.b.ins().ishl_imm_s(ca, 29);
        let v = self.b.ins().bor(cleared, ca);
        self.spr_set(SPR_XER, v);
    }

    fn ca(&mut self) -> Value {
        let xer = self.spr_get(SPR_XER);
        let v = self.b.ins().ushr_imm_s(xer, 29);
        self.b.ins().band_imm_s(v, 1)
    }

    /// Writes rD (or rA), and CR0 when Rc is set.
    fn result(&mut self, r: u32, v: Value, rc: bool) {
        self.set_gpr(r, v);
        if rc {
            self.record(v);
        }
    }

    fn cr_bit(&mut self, bit: u32) -> Value {
        let cr = self.cr();
        let v = self.b.ins().ushr_imm_s(cr, (31 - bit) as i64);
        self.b.ins().band_imm_s(v, 1)
    }

    fn iconst(&mut self, v: u32) -> Value {
        self.b.ins().iconst(types::I32, v as i32 as i64)
    }

    // --- faults and calls ---

    /// Leaves with `status` if it is a fault, the PC past the instruction
    /// at `addr` as the interpreter leaves it.
    fn check(&mut self, status: Value, pc: Option<u32>) {
        let hi = self.b.ins().ushr_imm_s(status, 32);
        let faulted = self.b.ins().icmp_imm_s(IntCC::NotEqual, hi, 0);
        let fail = self.b.create_block();
        let ok = self.b.create_block();
        self.b.set_cold_block(fail);
        self.b.ins().brif(faulted, fail, &[], ok, &[]);
        self.b.switch_to_block(fail);
        if let Some(pc) = pc {
            let pc = self.iconst(pc);
            self.b.ins().store(self.fc, pc, self.cpu, PC);
        }
        self.b.ins().jump(self.ret_fault, &[BlockArg::Value(status)]);
        self.b.switch_to_block(ok);
    }

    /// Runs the instruction's `ops` function.
    fn call_op(&mut self, i: &Insn) -> Option<Next> {
        let f = shim(ops::handler(i.op).unwrap()).expect("every ops function has a shim");
        assert!(self.arena.len() < self.arena.capacity());
        self.arena.push(*i);
        let insn = self.b.ins().iconst(self.ptr, self.arena.last().unwrap() as *const Insn as i64);
        let callee = self.b.ins().iconst(self.ptr, f as usize as i64);
        if i.is_branch() {
            // Branches read the PC for the link and the fall-through.
            let pc = self.iconst(i.addr + 4);
            self.b.ins().store(self.fc, pc, self.cpu, PC);
        }
        let call = self.b.ins().call_indirect(self.shim_sig, callee, &[self.cpu, self.mem, insn]);
        let status = self.b.inst_results(call)[0];
        if i.is_branch() {
            self.check(status, None);
            let pc = self.b.ins().load(types::I32, self.fc, self.cpu, PC);
            Some(Next::Dyn(pc))
        } else {
            self.check(status, Some(i.addr + 4));
            None
        }
    }

    // --- memory ---

    /// The host address of `size` bytes at `ea` if they are in a cached
    /// mirror: branches to `fast` with it, else to `slow`.
    fn host_addr(&mut self, ea: Value, size: i64, fast: Label, slow: Label) {
        let ea64 = self.b.ins().uextend(types::I64, ea);
        let hi = self.b.ins().ushr_imm_s(ea64, 28);
        let entry = self.b.ins().ishl_imm_s(hi, 4);
        let table = self.b.ins().iconst(self.ptr, self.regions as i64);
        let entry = self.b.ins().iadd(table, entry);
        let base = self.b.ins().load(types::I64, self.ft, entry, 0);
        let len = self.b.ins().load(types::I64, self.ft, entry, 8);
        let off = self.b.ins().band_imm_s(ea64, 0x0fff_ffff);
        let end = self.b.ins().iadd_imm_s(off, size);
        let fits = self.b.ins().icmp(IntCC::UnsignedLessThanOrEqual, end, len);
        let host = self.b.ins().iadd(base, off);
        self.b.ins().brif(fits, fast, &[BlockArg::Value(host)], slow, &[]);
    }

    /// Loads `size` bytes at `ea` as a big-endian value, extended to 32
    /// bits.
    fn load(&mut self, ea: Value, size: u32, signed: bool, addr: u32) -> Value {
        let fast = self.b.create_block();
        self.b.append_block_param(fast, self.ptr);
        let slow = self.b.create_block();
        let join = self.b.create_block();
        self.b.append_block_param(join, types::I32);
        self.b.set_cold_block(slow);
        self.host_addr(ea, size as i64, fast, slow);

        self.b.switch_to_block(fast);
        let host = self.b.block_params(fast)[0];
        let v = match size {
            1 => {
                let v = self.b.ins().load(types::I8, self.fr, host, 0);
                if signed { self.b.ins().sextend(types::I32, v) } else { self.b.ins().uextend(types::I32, v) }
            }
            2 => {
                let v = self.b.ins().load(types::I16, self.fr, host, 0);
                let v = self.b.ins().bswap(v);
                if signed { self.b.ins().sextend(types::I32, v) } else { self.b.ins().uextend(types::I32, v) }
            }
            _ => {
                let v = self.b.ins().load(types::I32, self.fr, host, 0);
                self.b.ins().bswap(v)
            }
        };
        self.b.ins().jump(join, &[BlockArg::Value(v)]);

        self.b.switch_to_block(slow);
        let f = match size {
            1 => read8 as *const () as usize,
            2 => read16 as *const () as usize,
            _ => read32 as *const () as usize,
        };
        let callee = self.b.ins().iconst(self.ptr, f as i64);
        let call = self.b.ins().call_indirect(self.read_sig, callee, &[self.mem, ea]);
        let r = self.b.inst_results(call)[0];
        self.check(r, Some(addr + 4));
        let v = self.b.ins().ireduce(types::I32, r);
        let v = match (size, signed) {
            (2, true) => {
                let v = self.b.ins().ireduce(types::I16, v);
                self.b.ins().sextend(types::I32, v)
            }
            _ => v,
        };
        self.b.ins().jump(join, &[BlockArg::Value(v)]);

        self.b.switch_to_block(join);
        self.b.block_params(join)[0]
    }

    /// Stores the low `size` bytes of `v` at `ea`, big-endian.
    fn store(&mut self, ea: Value, v: Value, size: u32, addr: u32) {
        let fast = self.b.create_block();
        self.b.append_block_param(fast, self.ptr);
        let slow = self.b.create_block();
        let join = self.b.create_block();
        self.b.set_cold_block(slow);
        self.host_addr(ea, size as i64, fast, slow);

        self.b.switch_to_block(fast);
        let host = self.b.block_params(fast)[0];
        match size {
            1 => {
                let x = self.b.ins().ireduce(types::I8, v);
                self.b.ins().store(self.fr, x, host, 0);
            }
            2 => {
                let x = self.b.ins().ireduce(types::I16, v);
                let x = self.b.ins().bswap(x);
                self.b.ins().store(self.fr, x, host, 0);
            }
            _ => {
                let x = self.b.ins().bswap(v);
                self.b.ins().store(self.fr, x, host, 0);
            }
        }
        self.b.ins().jump(join, &[]);

        self.b.switch_to_block(slow);
        let f = match size {
            1 => write8 as *const () as usize,
            2 => write16 as *const () as usize,
            _ => write32 as *const () as usize,
        };
        let callee = self.b.ins().iconst(self.ptr, f as i64);
        let call = self.b.ins().call_indirect(self.write_sig, callee, &[self.mem, ea, v]);
        let r = self.b.inst_results(call)[0];
        self.check(r, Some(addr + 4));
        self.b.ins().jump(join, &[]);

        self.b.switch_to_block(join);
    }

    // --- floating point ---

    /// ps0 (`k` = 0) or ps1 of FPR `r`.
    fn fpr(&mut self, r: u32, k: i32) -> Value {
        self.b.ins().load(types::F64, self.fc, self.cpu, FPR + 16 * r as i32 + 8 * k)
    }

    fn set_fpr(&mut self, r: u32, k: i32, v: Value) {
        self.b.ins().store(self.fc, v, self.cpu, FPR + 16 * r as i32 + 8 * k);
    }

    /// Both halves of FPR `r`.
    fn set_pair(&mut self, r: u32, ps0: Value, ps1: Value) {
        self.set_fpr(r, 0, ps0);
        self.set_fpr(r, 1, ps1);
    }

    /// `v as f32 as f64`.
    fn single(&mut self, v: Value) -> Value {
        let s = self.b.ins().fdemote(types::F32, v);
        self.b.ins().fpromote(types::F64, s)
    }

    fn fconst(&mut self, v: f64) -> Value {
        self.b.ins().f64const(v)
    }

    /// The CR field for an FP compare: unordered, lt, gt or eq.
    fn fp_flags(&mut self, x: Value, y: Value) -> Value {
        let uno = self.b.ins().fcmp(FloatCC::Unordered, x, y);
        let lt = self.b.ins().fcmp(FloatCC::LessThan, x, y);
        let gt = self.b.ins().fcmp(FloatCC::GreaterThan, x, y);
        let (one, eight, four, two) = (self.iconst(1), self.iconst(8), self.iconst(4), self.iconst(2));
        let v = self.b.ins().select(gt, four, two);
        let v = self.b.ins().select(lt, eight, v);
        self.b.ins().select(uno, one, v)
    }

    /// An FP load or store of `size` bytes at `ea`: `fast` with the host
    /// address if they are in a cached mirror (and `ok` holds, if given),
    /// else the instruction's `ops` function.
    fn fp_access(&mut self, i: &Insn, ea: Value, size: i64, ok: Option<Value>, fast: impl FnOnce(&mut Self, Value)) {
        let fast_b = self.b.create_block();
        self.b.append_block_param(fast_b, self.ptr);
        let slow = self.b.create_block();
        let join = self.b.create_block();
        self.b.set_cold_block(slow);
        if let Some(ok) = ok {
            let check = self.b.create_block();
            self.b.ins().brif(ok, check, &[], slow, &[]);
            self.b.switch_to_block(check);
        }
        self.host_addr(ea, size, fast_b, slow);

        self.b.switch_to_block(fast_b);
        let host = self.b.block_params(fast_b)[0];
        fast(self, host);
        self.b.ins().jump(join, &[]);

        self.b.switch_to_block(slow);
        self.call_op(i);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
    }

    /// The big-endian single at `host + off`, as f64.
    fn load_single(&mut self, host: Value, off: i32) -> Value {
        let v = self.b.ins().load(types::I32, self.fr, host, off);
        let v = self.b.ins().bswap(v);
        let v = self.b.ins().bitcast(types::F32, MemFlagsData::new(), v);
        self.b.ins().fpromote(types::F64, v)
    }

    /// Stores `v` rounded to single precision at `host + off`, big-endian.
    fn store_single(&mut self, host: Value, off: i32, v: Value) {
        let v = self.b.ins().fdemote(types::F32, v);
        let v = self.b.ins().bitcast(types::I32, MemFlagsData::new(), v);
        let v = self.b.ins().bswap(v);
        self.b.ins().store(self.fr, v, host, off);
    }

    /// The double-precision result of an `fp_arith` operation (multiply-adds
    /// round twice, as the `ops` functions do).
    fn fp_op(&mut self, op: Op, fa: Value, fb: Value, fc: Value) -> Value {
        use Op::*;
        match op {
            Fadd | Fadds => self.b.ins().fadd(fa, fb),
            Fsub | Fsubs => self.b.ins().fsub(fa, fb),
            Fmul | Fmuls => self.b.ins().fmul(fa, fc),
            Fdiv | Fdivs => self.b.ins().fdiv(fa, fb),
            _ => {
                let m = self.b.ins().fmul(fa, fc);
                let v = match op {
                    Fmadd | Fmadds | Fnmadd | Fnmadds => self.b.ins().fadd(m, fb),
                    _ => self.b.ins().fsub(m, fb),
                };
                if matches!(op, Fnmadd | Fnmadds | Fnmsub | Fnmsubs) { self.b.ins().fneg(v) } else { v }
            }
        }
    }

    // --- instructions ---

    /// Emits `i` as IR if it is one of the instructions handled here:
    /// Some(where the block goes next, for a branch).
    fn native(&mut self, i: &Insn) -> Option<Option<Next>> {
        use Op::*;
        let (rd, ra, rb) = (i.rd(), i.ra(), i.rb());
        match i.op {
            Addi | Addis => {
                let base = self.gpr0(ra);
                let imm = if i.op == Addi { i.simm() as i64 } else { (i.uimm() << 16) as i32 as i64 };
                let v = self.b.ins().iadd_imm_s(base, imm);
                self.set_gpr(rd, v);
            }
            Addic | AddicRc => {
                let a = self.gpr(ra);
                let v = self.b.ins().iadd_imm_s(a, i.simm() as i64);
                let ca = self.b.ins().icmp(IntCC::UnsignedLessThan, v, a);
                self.set_gpr(rd, v);
                self.set_ca(ca);
                if i.op == AddicRc {
                    self.record(v);
                }
            }
            Subfic => {
                // !a + imm + 1 = imm - a, carrying out unless it borrows.
                let a = self.gpr(ra);
                let imm = self.iconst(i.simm() as u32);
                let v = self.b.ins().isub(imm, a);
                let ca = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, imm, a);
                self.set_gpr(rd, v);
                self.set_ca(ca);
            }
            Mulli => {
                let a = self.gpr(ra);
                let v = self.b.ins().imul_imm_s(a, i.simm() as i64);
                self.set_gpr(rd, v);
            }
            Add | Subf | Mullw | Mulhw | Mulhwu | Divw | Divwu => {
                let a = self.gpr(ra);
                let b = self.gpr(rb);
                let v = match i.op {
                    Add => self.b.ins().iadd(a, b),
                    Subf => self.b.ins().isub(b, a),
                    Mullw => self.b.ins().imul(a, b),
                    Mulhw => self.b.ins().smulhi(a, b),
                    Mulhwu => self.b.ins().umulhi(a, b),
                    Divw => {
                        // Dividing by zero or MIN / -1 gives -1 or 0 by sign,
                        // as the interpreter has it.
                        let zero = self.b.ins().icmp_imm_s(IntCC::Equal, b, 0);
                        let min = self.b.ins().icmp_imm_s(IntCC::Equal, a, i32::MIN as i64);
                        let m1 = self.b.ins().icmp_imm_s(IntCC::Equal, b, -1);
                        let ovf = self.b.ins().band(min, m1);
                        let bad = self.b.ins().bor(zero, ovf);
                        let one = self.iconst(1);
                        let d = self.b.ins().select(bad, one, b);
                        let q = self.b.ins().sdiv(a, d);
                        let sign = self.b.ins().sshr_imm_s(a, 31);
                        self.b.ins().select(bad, sign, q)
                    }
                    _ => {
                        let zero = self.b.ins().icmp_imm_s(IntCC::Equal, b, 0);
                        let one = self.iconst(1);
                        let d = self.b.ins().select(zero, one, b);
                        let q = self.b.ins().udiv(a, d);
                        let z = self.iconst(0);
                        self.b.ins().select(zero, z, q)
                    }
                };
                self.result(rd, v, i.rc());
            }
            Neg => {
                let a = self.gpr(ra);
                let v = self.b.ins().ineg(a);
                self.result(rd, v, i.rc());
            }
            Addc | Adde | Addze | Addme | Subfc | Subfe | Subfze | Subfme => {
                let a = self.gpr(ra);
                let x = if matches!(i.op, Addc | Adde | Addze | Addme) { a } else { self.b.ins().bnot(a) };
                let y = match i.op {
                    Addze | Subfze => self.iconst(0),
                    Addme | Subfme => self.iconst(u32::MAX),
                    _ => self.gpr(rb),
                };
                let cin = match i.op {
                    Addc => self.iconst(0),
                    Subfc => self.iconst(1),
                    _ => self.ca(),
                };
                let x = self.b.ins().uextend(types::I64, x);
                let y = self.b.ins().uextend(types::I64, y);
                let cin = self.b.ins().uextend(types::I64, cin);
                let s = self.b.ins().iadd(x, y);
                let s = self.b.ins().iadd(s, cin);
                let hi = self.b.ins().ushr_imm_s(s, 32);
                let ca = self.b.ins().icmp_imm_s(IntCC::NotEqual, hi, 0);
                self.set_ca(ca);
                let v = self.b.ins().ireduce(types::I32, s);
                self.result(rd, v, i.rc());
            }
            Cmpi | Cmpli => {
                let a = self.gpr(ra);
                let imm = if i.op == Cmpi { i.simm() as u32 } else { i.uimm() };
                let b = self.iconst(imm);
                self.compare(i.crfd(), a, b, i.op == Cmpi);
            }
            Cmp | Cmpl => {
                let a = self.gpr(ra);
                let b = self.gpr(rb);
                self.compare(i.crfd(), a, b, i.op == Cmp);
            }
            Ori | Oris | Xori | Xoris => {
                let s = self.gpr(rd);
                let imm = if matches!(i.op, Ori | Xori) { i.uimm() } else { i.uimm() << 16 };
                let v = if matches!(i.op, Ori | Oris) {
                    self.b.ins().bor_imm_s(s, imm as i32 as i64)
                } else {
                    self.b.ins().bxor_imm_s(s, imm as i32 as i64)
                };
                self.set_gpr(ra, v);
            }
            AndiRc | AndisRc => {
                let s = self.gpr(rd);
                let imm = if i.op == AndiRc { i.uimm() } else { i.uimm() << 16 };
                let v = self.b.ins().band_imm_s(s, imm as i32 as i64);
                self.result(ra, v, true);
            }
            And | Andc | Or | Orc | Xor | Nand | Nor | Eqv => {
                let s = self.gpr(rd);
                let b = self.gpr(rb);
                let v = match i.op {
                    And => self.b.ins().band(s, b),
                    Andc => self.b.ins().band_not(s, b),
                    Or => self.b.ins().bor(s, b),
                    Orc => self.b.ins().bor_not(s, b),
                    Xor => self.b.ins().bxor(s, b),
                    Nand => {
                        let v = self.b.ins().band(s, b);
                        self.b.ins().bnot(v)
                    }
                    Nor => {
                        let v = self.b.ins().bor(s, b);
                        self.b.ins().bnot(v)
                    }
                    _ => {
                        let v = self.b.ins().bxor(s, b);
                        self.b.ins().bnot(v)
                    }
                };
                self.result(ra, v, i.rc());
            }
            Slw | Srw => {
                let s = self.gpr(rd);
                let b = self.gpr(rb);
                let sh = self.b.ins().band_imm_s(b, 31);
                let v = if i.op == Slw { self.b.ins().ishl(s, sh) } else { self.b.ins().ushr(s, sh) };
                let big = self.b.ins().band_imm_s(b, 0x20);
                let z = self.iconst(0);
                let v = self.b.ins().select(big, z, v);
                self.result(ra, v, i.rc());
            }
            Srawi => {
                let s = self.gpr(rd);
                let sh = i.sh();
                let v = self.b.ins().sshr_imm_s(s, sh as i64);
                let neg = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, s, 0);
                let lost = self.b.ins().band_imm_s(s, ((1u64 << sh) - 1) as i64);
                let lost = self.b.ins().icmp_imm_s(IntCC::NotEqual, lost, 0);
                let ca = self.b.ins().band(neg, lost);
                self.set_ca(ca);
                self.result(ra, v, i.rc());
            }
            Sraw => {
                let s = self.gpr(rd);
                let b = self.gpr(rb);
                let sh = self.b.ins().band_imm_s(b, 63);
                let big = self.b.ins().icmp_imm_s(IntCC::UnsignedGreaterThan, sh, 31);
                let c31 = self.iconst(31);
                let shc = self.b.ins().select(big, c31, sh);
                let v = self.b.ins().sshr(s, shc);
                let one = self.iconst(1);
                let bit = self.b.ins().ishl(one, shc);
                let mk = self.b.ins().iadd_imm_s(bit, -1);
                let all = self.iconst(u32::MAX);
                let mk = self.b.ins().select(big, all, mk);
                let lost = self.b.ins().band(s, mk);
                let lost = self.b.ins().icmp_imm_s(IntCC::NotEqual, lost, 0);
                let neg = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, s, 0);
                let ca = self.b.ins().band(neg, lost);
                self.set_ca(ca);
                self.result(ra, v, i.rc());
            }
            Cntlzw => {
                let s = self.gpr(rd);
                let v = self.b.ins().clz(s);
                self.result(ra, v, i.rc());
            }
            Extsb | Extsh => {
                let s = self.gpr(rd);
                let t = if i.op == Extsb { types::I8 } else { types::I16 };
                let n = self.b.ins().ireduce(t, s);
                let v = self.b.ins().sextend(types::I32, n);
                self.result(ra, v, i.rc());
            }
            Rlwinm | Rlwnm | Rlwimi => {
                let s = self.gpr(rd);
                let r = match i.op {
                    Rlwnm => {
                        let b = self.gpr(rb);
                        let sh = self.b.ins().band_imm_s(b, 31);
                        self.b.ins().rotl(s, sh)
                    }
                    _ if i.sh() == 0 => s,
                    _ => self.b.ins().rotl_imm_s(s, i.sh() as i64),
                };
                let mk = mask(i.mb(), i.me());
                let mut v = self.b.ins().band_imm_s(r, mk as i32 as i64);
                if i.op == Rlwimi {
                    let a = self.gpr(ra);
                    let keep = self.b.ins().band_imm_s(a, !mk as i32 as i64);
                    v = self.b.ins().bor(v, keep);
                }
                self.result(ra, v, i.rc());
            }
            Lwz | Lwzu | Lwzx | Lwzux | Lhz | Lhzu | Lhzx | Lhzux | Lha | Lhau | Lhax | Lhaux | Lbz | Lbzu | Lbzx
            | Lbzux => {
                let x = matches!(i.op, Lwzx | Lwzux | Lhzx | Lhzux | Lhax | Lhaux | Lbzx | Lbzux);
                let ea = self.ea(i, x);
                let (size, signed) = match i.op {
                    Lwz | Lwzu | Lwzx | Lwzux => (4, false),
                    Lhz | Lhzu | Lhzx | Lhzux => (2, false),
                    Lha | Lhau | Lhax | Lhaux => (2, true),
                    _ => (1, false),
                };
                let v = self.load(ea, size, signed, i.addr);
                self.set_gpr(rd, v);
                if matches!(i.op, Lwzu | Lwzux | Lhzu | Lhzux | Lhau | Lhaux | Lbzu | Lbzux) {
                    self.set_gpr(ra, ea);
                }
            }
            Stw | Stwu | Stwx | Stwux | Sth | Sthu | Sthx | Sthux | Stb | Stbu | Stbx | Stbux => {
                let x = matches!(i.op, Stwx | Stwux | Sthx | Sthux | Stbx | Stbux);
                let ea = self.ea(i, x);
                let s = self.gpr(rd);
                let size = match i.op {
                    Stw | Stwu | Stwx | Stwux => 4,
                    Sth | Sthu | Sthx | Sthux => 2,
                    _ => 1,
                };
                self.store(ea, s, size, i.addr);
                if matches!(i.op, Stwu | Stwux | Sthu | Sthux | Stbu | Stbux) {
                    self.set_gpr(ra, ea);
                }
            }
            Lfs | Lfsu | Lfsx | Lfsux | Lfd | Lfdu | Lfdx | Lfdux => {
                let ea = self.ea(i, matches!(i.op, Lfsx | Lfsux | Lfdx | Lfdux));
                let single = matches!(i.op, Lfs | Lfsu | Lfsx | Lfsux);
                let update = matches!(i.op, Lfsu | Lfsux | Lfdu | Lfdux);
                self.fp_access(i, ea, if single { 4 } else { 8 }, None, |t, host| {
                    if single {
                        let v = t.load_single(host, 0);
                        t.set_pair(rd, v, v);
                    } else {
                        let v = t.b.ins().load(types::I64, t.fr, host, 0);
                        let v = t.b.ins().bswap(v);
                        let v = t.b.ins().bitcast(types::F64, MemFlagsData::new(), v);
                        t.set_fpr(rd, 0, v);
                    }
                    if update {
                        t.set_gpr(ra, ea);
                    }
                });
            }
            Stfs | Stfsu | Stfsx | Stfsux | Stfd | Stfdu | Stfdx | Stfdux | Stfiwx => {
                let ea = self.ea(i, matches!(i.op, Stfsx | Stfsux | Stfdx | Stfdux | Stfiwx));
                let update = matches!(i.op, Stfsu | Stfsux | Stfdu | Stfdux);
                let size = if matches!(i.op, Stfd | Stfdu | Stfdx | Stfdux) { 8 } else { 4 };
                let op = i.op;
                self.fp_access(i, ea, size, None, |t, host| {
                    if matches!(op, Stfs | Stfsu | Stfsx | Stfsux) {
                        let v = t.fpr(rd, 0);
                        t.store_single(host, 0, v);
                    } else {
                        // The bits of ps0: all of them, or the low word.
                        let v = t.b.ins().load(types::I64, t.fc, t.cpu, FPR + 16 * rd as i32);
                        let v = if size == 8 { v } else { t.b.ins().ireduce(types::I32, v) };
                        let v = t.b.ins().bswap(v);
                        t.b.ins().store(t.fr, v, host, 0);
                    }
                    if update {
                        t.set_gpr(ra, ea);
                    }
                });
            }
            PsqL | PsqLu | PsqLx | PsqLux | PsqSt | PsqStu | PsqStx | PsqStux => {
                let (w, q, ea) = match i.op {
                    PsqL | PsqLu | PsqSt | PsqStu => {
                        let (w, q, disp) = i.psq_d();
                        let base = self.gpr0(ra);
                        (w, q, self.b.ins().iadd_imm_s(base, disp as i64))
                    }
                    _ => {
                        let (w, q) = i.psq_x();
                        (w, q, self.ea(i, true))
                    }
                };
                let load = matches!(i.op, PsqL | PsqLu | PsqLx | PsqLux);
                let update = matches!(i.op, PsqLu | PsqLux | PsqStu | PsqStux);
                // Types 0-3 are floats, unscaled; the integer types take
                // the `ops` function.
                let gqr = self.spr_get(SPR_GQR0 + q as usize);
                let ty = if load { self.b.ins().ushr_imm_s(gqr, 16) } else { gqr };
                let ty = self.b.ins().band_imm_s(ty, 4);
                let float = self.b.ins().icmp_imm_s(IntCC::Equal, ty, 0);
                self.fp_access(i, ea, if w == 1 { 4 } else { 8 }, Some(float), |t, host| {
                    if load {
                        let p0 = t.load_single(host, 0);
                        let p1 = if w == 1 { t.fconst(1.0) } else { t.load_single(host, 4) };
                        t.set_pair(rd, p0, p1);
                    } else {
                        let p0 = t.fpr(rd, 0);
                        t.store_single(host, 0, p0);
                        if w == 0 {
                            let p1 = t.fpr(rd, 1);
                            t.store_single(host, 4, p1);
                        }
                    }
                    if update {
                        t.set_gpr(ra, ea);
                    }
                });
            }
            Fmr | Fneg | Fabs | Fnabs => {
                let v = self.fpr(rb, 0);
                let v = match i.op {
                    Fmr => v,
                    Fneg => self.b.ins().fneg(v),
                    Fabs => self.b.ins().fabs(v),
                    _ => {
                        let a = self.b.ins().fabs(v);
                        self.b.ins().fneg(a)
                    }
                };
                self.set_fpr(rd, 0, v);
            }
            Frsp | Fres => {
                let mut v = self.fpr(rb, 0);
                if i.op == Fres {
                    let one = self.fconst(1.0);
                    v = self.b.ins().fdiv(one, v);
                }
                let v = self.single(v);
                self.set_pair(rd, v, v);
            }
            Frsqrte => {
                let v = self.fpr(rb, 0);
                let s = self.b.ins().sqrt(v);
                let one = self.fconst(1.0);
                let v = self.b.ins().fdiv(one, s);
                self.set_fpr(rd, 0, v);
            }
            Fctiw | Fctiwz => {
                let v = self.fpr(rb, 0);
                let r = if i.op == Fctiwz {
                    v
                } else {
                    // Rounded per FPSCR[RN]: nearest-even, toward zero,
                    // +inf, -inf.
                    let fpscr = self.b.ins().load(types::I32, self.fc, self.cpu, FPSCR);
                    let rn = self.b.ins().band_imm_s(fpscr, 3);
                    let n = self.b.ins().nearest(v);
                    let t = self.b.ins().trunc(v);
                    let c = self.b.ins().ceil(v);
                    let f = self.b.ins().floor(v);
                    let is0 = self.b.ins().icmp_imm_s(IntCC::Equal, rn, 0);
                    let is1 = self.b.ins().icmp_imm_s(IntCC::Equal, rn, 1);
                    let is2 = self.b.ins().icmp_imm_s(IntCC::Equal, rn, 2);
                    let x = self.b.ins().select(is2, c, f);
                    let x = self.b.ins().select(is1, t, x);
                    self.b.ins().select(is0, n, x)
                };
                // Saturating, NaN to 0, as Rust's `as i32` after a clamp.
                let w = self.b.ins().fcvt_to_sint_sat(types::I32, r);
                let w = self.b.ins().uextend(types::I64, w);
                self.b.ins().store(self.fc, w, self.cpu, FPR + 16 * rd as i32);
            }
            Fsel => {
                let (fa, fb, fc) = (self.fpr(ra, 0), self.fpr(rb, 0), self.fpr(i.rc_reg(), 0));
                let zero = self.fconst(0.0);
                let ge = self.b.ins().fcmp(FloatCC::GreaterThanOrEqual, fa, zero);
                let v = self.b.ins().select(ge, fc, fb);
                self.set_fpr(rd, 0, v);
            }
            Fadd | Fsub | Fmul | Fdiv | Fadds | Fsubs | Fmuls | Fdivs | Fmadd | Fmsub | Fmadds | Fmsubs | Fnmadd
            | Fnmsub | Fnmadds | Fnmsubs => {
                let (fa, fb, fc) = (self.fpr(ra, 0), self.fpr(rb, 0), self.fpr(i.rc_reg(), 0));
                let v = self.fp_op(i.op, fa, fb, fc);
                if matches!(i.op, Fadds | Fsubs | Fmuls | Fdivs | Fmadds | Fmsubs | Fnmadds | Fnmsubs) {
                    let s = self.single(v);
                    self.set_pair(rd, s, s);
                } else {
                    self.set_fpr(rd, 0, v);
                }
            }
            Fcmpu | Fcmpo => {
                let (fa, fb) = (self.fpr(ra, 0), self.fpr(rb, 0));
                let f = self.fp_flags(fa, fb);
                self.set_cr_field(i.crfd(), f);
            }
            PsMr | PsNeg | PsAbs | PsNabs => {
                let op = i.op;
                let half = |t: &mut Self, k| {
                    let v = t.fpr(rb, k);
                    match op {
                        PsMr => v,
                        PsNeg => t.b.ins().fneg(v),
                        PsAbs => t.b.ins().fabs(v),
                        _ => {
                            let a = t.b.ins().fabs(v);
                            t.b.ins().fneg(a)
                        }
                    }
                };
                let (p0, p1) = (half(self, 0), half(self, 1));
                self.set_pair(rd, p0, p1);
            }
            PsMerge00 | PsMerge01 | PsMerge10 | PsMerge11 => {
                let (x, y) = match i.op {
                    PsMerge00 => (0, 0),
                    PsMerge01 => (0, 1),
                    PsMerge10 => (1, 0),
                    _ => (1, 1),
                };
                let (p0, p1) = (self.fpr(ra, x), self.fpr(rb, y));
                self.set_pair(rd, p0, p1);
            }
            PsAdd | PsSub | PsMul | PsDiv | PsMadd | PsMsub | PsNmadd | PsNmsub | PsMuls0 | PsMuls1 | PsMadds0
            | PsMadds1 | PsSel | PsRes | PsRsqrte => {
                let (op, rc) = (i.op, i.rc_reg());
                let half = |t: &mut Self, k: i32| {
                    let (fa, fb) = (t.fpr(ra, k), t.fpr(rb, k));
                    // The scalar forms take one slot of frC for both halves.
                    let fc = match op {
                        PsMuls0 | PsMadds0 => t.fpr(rc, 0),
                        PsMuls1 | PsMadds1 => t.fpr(rc, 1),
                        _ => t.fpr(rc, k),
                    };
                    let v = match op {
                        PsSel => {
                            let zero = t.fconst(0.0);
                            let ge = t.b.ins().fcmp(FloatCC::GreaterThanOrEqual, fa, zero);
                            t.b.ins().select(ge, fc, fb)
                        }
                        PsRes => {
                            let one = t.fconst(1.0);
                            t.b.ins().fdiv(one, fb)
                        }
                        PsRsqrte => {
                            let s = t.b.ins().sqrt(fb);
                            let one = t.fconst(1.0);
                            t.b.ins().fdiv(one, s)
                        }
                        PsMuls0 | PsMuls1 => t.b.ins().fmul(fa, fc),
                        PsMadds0 | PsMadds1 => t.fp_op(Fmadd, fa, fb, fc),
                        PsAdd => t.fp_op(Fadd, fa, fb, fc),
                        PsSub => t.fp_op(Fsub, fa, fb, fc),
                        PsMul => t.fp_op(Fmul, fa, fb, fc),
                        PsDiv => t.fp_op(Fdiv, fa, fb, fc),
                        PsMadd => t.fp_op(Fmadd, fa, fb, fc),
                        PsMsub => t.fp_op(Fmsub, fa, fb, fc),
                        PsNmadd => t.fp_op(Fnmadd, fa, fb, fc),
                        _ => t.fp_op(Fnmsub, fa, fb, fc),
                    };
                    t.single(v)
                };
                let (p0, p1) = (half(self, 0), half(self, 1));
                self.set_pair(rd, p0, p1);
            }
            PsSum0 | PsSum1 => {
                // One half is fa.ps0 + fb.ps1, the other comes from frC.
                let (fa, fb) = (self.fpr(ra, 0), self.fpr(rb, 1));
                let sum = self.b.ins().fadd(fa, fb);
                let sum = self.single(sum);
                if i.op == PsSum0 {
                    let c1 = self.fpr(i.rc_reg(), 1);
                    self.set_pair(rd, sum, c1);
                } else {
                    let c0 = self.fpr(i.rc_reg(), 0);
                    self.set_pair(rd, c0, sum);
                }
            }
            PsCmpu0 | PsCmpo0 | PsCmpu1 | PsCmpo1 => {
                let k = matches!(i.op, PsCmpu1 | PsCmpo1) as i32;
                let (fa, fb) = (self.fpr(ra, k), self.fpr(rb, k));
                let f = self.fp_flags(fa, fb);
                self.set_cr_field(i.crfd(), f);
            }
            Twi | Tw | Mtsr | Mtsrin | Sync | Isync | Eieio | Dcbf | Dcbi | Dcbst | Dcbt | Dcbtst | Icbi | Tlbie
            | Tlbia | Tlbsync => {}
            Mfspr if i.spr() != 22 => {
                let v = self.spr_get(i.spr() as usize);
                self.set_gpr(rd, v);
            }
            Mtspr if !matches!(i.spr(), 22 | 923) => {
                let v = self.gpr(rd);
                self.spr_set(i.spr() as usize, v);
            }
            Mfcr => {
                let v = self.cr();
                self.set_gpr(rd, v);
            }
            Mtcrf => {
                let mk = field_mask(i.crm());
                let cr = self.cr();
                let keep = self.b.ins().band_imm_s(cr, !mk as i32 as i64);
                let s = self.gpr(rd);
                let new = self.b.ins().band_imm_s(s, mk as i32 as i64);
                let v = self.b.ins().bor(keep, new);
                self.set_cr(v);
            }
            Mcrf => {
                let cr = self.cr();
                let v = self.b.ins().ushr_imm_s(cr, (28 - 4 * i.crfs()) as i64);
                let v = self.b.ins().band_imm_s(v, 0xf);
                self.set_cr_field(i.crfd(), v);
            }
            Crand | Crandc | Creqv | Crnand | Crnor | Cror | Crorc | Crxor => {
                let x = self.cr_bit(ra);
                let y = self.cr_bit(rb);
                let v = match i.op {
                    Crand => self.b.ins().band(x, y),
                    Crandc => self.b.ins().band_not(x, y),
                    Creqv => {
                        let v = self.b.ins().bxor(x, y);
                        self.b.ins().bxor_imm_s(v, 1)
                    }
                    Crnand => {
                        let v = self.b.ins().band(x, y);
                        self.b.ins().bxor_imm_s(v, 1)
                    }
                    Crnor => {
                        let v = self.b.ins().bor(x, y);
                        self.b.ins().bxor_imm_s(v, 1)
                    }
                    Cror => self.b.ins().bor(x, y),
                    Crorc => {
                        let ny = self.b.ins().bxor_imm_s(y, 1);
                        self.b.ins().bor(x, ny)
                    }
                    _ => self.b.ins().bxor(x, y),
                };
                let v = self.b.ins().band_imm_s(v, 1);
                let sh = 31 - rd;
                let cr = self.cr();
                let keep = self.b.ins().band_imm_s(cr, !(1u32 << sh) as i32 as i64);
                let v = self.b.ins().ishl_imm_s(v, sh as i64);
                let v = self.b.ins().bor(keep, v);
                self.set_cr(v);
            }
            Mfmsr => {
                let v = self.b.ins().load(types::I32, self.fc, self.cpu, MSR);
                self.set_gpr(rd, v);
            }
            B => {
                if i.lk() {
                    let lr = self.iconst(i.addr + 4);
                    self.spr_set(SPR_LR, lr);
                }
                return Some(Some(Next::To(i.branch_target().unwrap())));
            }
            Bc => {
                let cond = self.branch_cond(i, true);
                if i.lk() {
                    let lr = self.iconst(i.addr + 4);
                    self.spr_set(SPR_LR, lr);
                }
                let (t, f) = (i.branch_target().unwrap(), i.addr + 4);
                return Some(Some(match cond {
                    None => Next::To(t),
                    Some(c) => Next::Cond(c, t, f),
                }));
            }
            Bclr | Bcctr => {
                let lr = if i.op == Bclr {
                    let lr = self.spr_get(SPR_LR);
                    Some(self.b.ins().band_imm_s(lr, !3))
                } else {
                    None
                };
                let cond = self.branch_cond(i, i.op == Bclr);
                if i.lk() {
                    let link = self.iconst(i.addr + 4);
                    self.spr_set(SPR_LR, link);
                }
                let target = match lr {
                    Some(t) => t,
                    None => {
                        let ctr = self.spr_get(SPR_CTR);
                        self.b.ins().band_imm_s(ctr, !3)
                    }
                };
                let next = match cond {
                    None => target,
                    Some(c) => {
                        let fall = self.iconst(i.addr + 4);
                        self.b.ins().select(c, target, fall)
                    }
                };
                return Some(Some(Next::Dyn(next)));
            }
            _ => return None,
        }
        Some(None)
    }

    /// D-form `d(rA)` or X-form `rA + rB` (rA = r0 reads 0).
    fn ea(&mut self, i: &Insn, x: bool) -> Value {
        let base = self.gpr0(i.ra());
        if x {
            let b = self.gpr(i.rb());
            self.b.ins().iadd(base, b)
        } else {
            self.b.ins().iadd_imm_s(base, i.simm() as i64)
        }
    }

    /// The branch condition (decrementing CTR as BO says), or None if the
    /// branch is always taken.
    fn branch_cond(&mut self, i: &Insn, use_ctr: bool) -> Option<Value> {
        let bo = i.bo();
        let mut ok: Option<Value> = None;
        if use_ctr && bo & 4 == 0 {
            let ctr = self.spr_get(SPR_CTR);
            let ctr = self.b.ins().iadd_imm_s(ctr, -1);
            self.spr_set(SPR_CTR, ctr);
            let cc = if bo & 2 != 0 { IntCC::Equal } else { IntCC::NotEqual };
            ok = Some(self.b.ins().icmp_imm_s(cc, ctr, 0));
        }
        if bo & 16 == 0 {
            let bit = self.cr_bit(i.bi());
            let want = (bo & 8 != 0) as i64;
            let c = self.b.ins().icmp_imm_s(IntCC::Equal, bit, want);
            ok = Some(match ok {
                Some(o) => self.b.ins().band(o, c),
                None => c,
            });
        }
        ok
    }
}

fn is_memory(op: Op) -> bool {
    use Op::*;
    matches!(
        op,
        Lwz | Lwzu | Lwzx | Lwzux | Lhz | Lhzu | Lhzx | Lhzux | Lha | Lhau | Lhax | Lhaux | Lbz | Lbzu | Lbzx | Lbzux
            | Stw | Stwu | Stwx | Stwux | Sth | Sthu | Sthx | Sthux | Stb | Stbu | Stbx | Stbux
            | Lfs | Lfsu | Lfsx | Lfsux | Lfd | Lfdu | Lfdx | Lfdux
            | Stfs | Stfsu | Stfsx | Stfsux | Stfd | Stfdu | Stfdx | Stfdux | Stfiwx
            | PsqL | PsqLu | PsqLx | PsqLux | PsqSt | PsqStu | PsqStx | PsqStux
    )
}

/// PowerPC rotate mask from bit `mb` to bit `me` (big-endian numbering).
fn mask(mb: u32, me: u32) -> u32 {
    let a = u32::MAX >> mb;
    let b = u32::MAX << (31 - me);
    if mb <= me { a & b } else { a | b }
}

/// CR mask from an 8-bit field selector.
fn field_mask(fm: u32) -> u32 {
    (0..8).filter(|k| fm & (0x80 >> k) != 0).fold(0, |m, k| m | 0xf000_0000 >> (4 * k))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: u32 = 0x8000_3000;
    /// r31 holds this RAM address and is never written: loads and stores
    /// go through it.
    const DATA: u32 = 0x8000_8000;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 16) as u32
        }
    }

    /// A random non-branch instruction that only touches registers, CR,
    /// XER/LR/CTR, the FPRs, FPSCR, the GQRs and RAM at r31.
    fn random_insn(rng: &mut Rng) -> u32 {
        const PRIMARY: &[u32] = &[
            7, 8, 10, 11, 12, 13, 14, 15, 20, 21, 23, 24, 25, 26, 27, 28, 29, 31, 31, 31, 31, 19, 32, 34, 36, 38, 40, 42,
            44, 48, 50, 52, 54, 56, 60, 59, 59, 63, 63, 63, 4, 4, 4, 4,
        ];
        const XO31: &[u32] = &[
            0, 8, 10, 11, 32, 40, 24, 26, 28, 60, 104, 124, 136, 138, 200, 202, 232, 234, 235, 266, 284, 316, 412, 444,
            459, 476, 491, 536, 792, 824, 922, 954, 19, 144, 339, 467,
        ];
        const XO19: &[u32] = &[0, 33, 129, 193, 225, 257, 289, 417, 449];
        loop {
            let po = PRIMARY[rng.next() as usize % PRIMARY.len()];
            let mut w = po << 26 | (rng.next() & 0x03ff_ffff);
            match po {
                31 => w = (w & !0x7fe) | XO31[rng.next() as usize % XO31.len()] << 1,
                19 => w = (w & !0x7fe) | XO19[rng.next() as usize % XO19.len()] << 1,
                // Loads and stores: base r31, a small displacement.
                32..=55 => w = (w & !(31 << 16 | 0xffff)) | 31 << 16 | (rng.next() & 0xff),
                // Quantized: the same, with random W and GQR.
                56 | 60 => w = (w & !(31 << 16 | 0xffff)) | 31 << 16 | (rng.next() & 0xf0ff),
                _ => {}
            }
            let i = Insn::new(CODE, w);
            if ops::handler(i.op).is_none() || i.is_branch() {
                continue;
            }
            // Indexed FP accesses reach random addresses; update forms
            // write r31.
            if matches!(
                i.op,
                Op::PsqLx | Op::PsqLux | Op::PsqStx | Op::PsqStux | Op::PsqLu | Op::PsqStu | Op::Lfsu | Op::Lfdu | Op::Stfsu | Op::Stfdu
                    | Op::DcbzL
            ) {
                continue;
            }
            let (rd, ra) = (i.rd(), i.ra());
            let writes_31 = match i.op {
                Op::Ori | Op::Oris | Op::Xori | Op::Xoris | Op::AndiRc | Op::AndisRc | Op::And | Op::Andc | Op::Or
                | Op::Orc | Op::Xor | Op::Nand | Op::Nor | Op::Eqv | Op::Slw | Op::Srw | Op::Sraw | Op::Srawi
                | Op::Cntlzw | Op::Extsb | Op::Extsh | Op::Rlwinm | Op::Rlwnm | Op::Rlwimi => ra == 31,
                Op::Stw | Op::Stb | Op::Sth | Op::Cmp | Op::Cmpl | Op::Cmpi | Op::Cmpli | Op::Mtcrf => false,
                _ if i.word >> 26 == 19 => false,
                _ => rd == 31,
            };
            if writes_31 {
                continue;
            }
            if matches!(i.op, Op::Mfspr | Op::Mtspr) && !matches!(i.spr(), 1 | 8 | 9) {
                continue;
            }
            return w;
        }
    }

    fn state(c: &Cpu) -> ([u32; 32], u32, u32, u32, u32, u32, u32, Vec<[u64; 2]>) {
        let fpr = c.fpr.iter().map(|p| p.map(f64::to_bits)).collect();
        (c.gpr, c.cr, c.spr[1], c.spr[8], c.spr[9], c.pc, c.fpscr, fpr)
    }

    /// A random FPR half: small numbers, singles, specials and raw bits.
    fn random_fp(rng: &mut Rng) -> f64 {
        match rng.next() % 6 {
            0 => (rng.next() % 200) as f64 / 8.0 - 12.0,
            1 => f32::from_bits(rng.next()) as f64,
            2 => [0.0, -0.0, f64::INFINITY, f64::NEG_INFINITY, f64::NAN, 1e-310, 3e9, -3e9][rng.next() as usize % 8],
            _ => f64::from_bits((rng.next() as u64) << 32 | rng.next() as u64),
        }
    }

    /// Random blocks run the same translated as interpreted: the first
    /// ends in a random conditional branch to the third, the second falls
    /// into it.
    #[test]
    fn matches_interpreter() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for case in 0..2000 {
            let mut words: Vec<u32> = (0..12).map(|_| random_insn(&mut rng)).collect();
            // bc with random BO (CTR and CR forms) and BI, to word 20.
            let bo = [4u32, 12, 0, 2, 8, 10, 16, 18, 20][rng.next() as usize % 9];
            let bi = rng.next() % 32;
            words.push(16 << 26 | bo << 21 | bi << 16 | ((20 - 12) * 4));
            words.extend((0..7).map(|_| random_insn(&mut rng)));
            words.extend((0..8).map(|_| random_insn(&mut rng)));
            let end = CODE + 4 * words.len() as u32;

            let mut mem = Memory::default();
            for (k, w) in words.iter().enumerate() {
                mem.w32(CODE + 4 * k as u32, *w).unwrap();
            }
            for k in 0..128 {
                mem.w32(DATA + 4 * k, rng.next().wrapping_mul(0x9e37_79b9)).unwrap();
            }
            let mut cpu = Cpu::default();
            cpu.pc = CODE;
            for r in 0..31 {
                cpu.gpr[r] = match rng.next() % 4 {
                    0 => rng.next() % 64,
                    1 => (rng.next() as i32 % 100) as u32,
                    _ => rng.next(),
                };
            }
            cpu.gpr[31] = DATA;
            cpu.cr = rng.next();
            cpu.spr[1] = rng.next() & 0xe000_0000;
            cpu.spr[9] = rng.next() % 3;
            cpu.msr |= rumble_runtime::cpu::MSR_FP;
            cpu.fpscr = rng.next() & 3;
            for r in 0..32 {
                cpu.fpr[r] = [random_fp(&mut rng), random_fp(&mut rng)];
            }
            // Load and store types: floats and the integer types, scaled.
            for q in 0..8 {
                let ty = |rng: &mut Rng| [0, 0, 4, 5, 6, 7][rng.next() as usize % 6];
                let (lt, st) = (ty(&mut rng), ty(&mut rng));
                cpu.spr[SPR_GQR0 + q] = (rng.next() & 0x3f) << 24 | lt << 16 | (rng.next() & 0x3f) << 8 | st;
            }

            let block = |from: usize, to: usize| BlockPlan {
                start: CODE + 4 * from as u32,
                body: (from..to).map(|k| Insn::new(CODE + 4 * k as u32, words[k])).collect(),
            };
            let plan = [FuncPlan { start: CODE, blocks: vec![block(0, 13), block(13, 20), block(20, words.len())] }];

            let mut jit_mem = Memory::default();
            for (k, w) in words.iter().enumerate() {
                jit_mem.w32(CODE + 4 * k as u32, *w).unwrap();
            }
            for k in 0..128 {
                jit_mem.w32(DATA + 4 * k, mem.r32(DATA + 4 * k).unwrap()).unwrap();
            }
            let mut jit_cpu = Cpu::default();
            (jit_cpu.pc, jit_cpu.gpr, jit_cpu.cr) = (CODE, cpu.gpr, cpu.cr);
            jit_cpu.spr[1] = cpu.spr[1];
            jit_cpu.spr[9] = cpu.spr[9];
            (jit_cpu.msr, jit_cpu.fpscr, jit_cpu.fpr) = (cpu.msr, cpu.fpscr, cpu.fpr);
            for q in 0..8 {
                jit_cpu.spr[SPR_GQR0 + q] = cpu.spr[SPR_GQR0 + q];
            }
            let t = translate_plan(&plan, &mut jit_mem).unwrap();
            let Some(run) = t.blocks.jit_at(CODE) else { panic!("no block") };
            jit_cpu.budget = 1000;
            let status = unsafe { run(&mut jit_cpu, &mut jit_mem) };
            assert_eq!(status, 0, "case {case}: {:#?}", words.iter().map(|&w| Insn::new(0, w).to_string()).collect::<Vec<_>>());

            let mut steps = 0;
            while cpu.pc != end && steps < 100 {
                cpu.run(&mut mem, 1, &mut |_| {});
                steps += 1;
            }
            let listing: Vec<String> = words.iter().map(|&w| Insn::new(0, w).to_string()).collect();
            assert_eq!(state(&jit_cpu), state(&cpu), "case {case}: {listing:#?}");
            assert_eq!(1000 - jit_cpu.budget, steps, "case {case}: instructions counted");
            for k in 0..128 {
                assert_eq!(jit_mem.r32(DATA + 4 * k), mem.r32(DATA + 4 * k), "case {case}: RAM word {k}");
            }
        }
    }

    /// Every instruction the Rust generator compiles has a shim.
    #[test]
    fn every_handler_has_a_shim() {
        for w in 0..=u16::MAX as u32 {
            for word in [w << 16, w << 16 | 0x7fe, w << 16 | 0x214] {
                let op = rumble_recomp::ppc::decode(word);
                if let Some(name) = ops::handler(op) {
                    assert!(shim(name).is_some(), "no shim for {name}");
                }
            }
        }
    }
}
