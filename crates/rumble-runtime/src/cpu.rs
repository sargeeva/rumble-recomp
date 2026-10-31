//! Gekko/Broadway interpreter. It is the reference the recompiled code is
//! checked against, and the fallback for code the recompiler misses.
//!
//! Coverage today: integer, branch, load/store, system-register and the
//! common FP / paired-single ops. Anything else stops execution with
//! [`Stop::Unimplemented`] so gaps show up immediately.

pub mod blocks;
pub mod ops;

use crate::mem::{Fault, Memory};
use rumble_recomp::ppc::{decode, Insn, Op};

/// The longest compiled block (`rumble-port recomp --max-len`), and the
/// most compiled code runs before handing back to the dispatcher.
const MAX_BLOCK: u64 = 64;
const MAX_CHAIN: u64 = 1 << 16;

/// Entries in the decode cache, indexed by PC.
const DECODE_CACHE: usize = 1 << 16;

/// Timebase ticks per second (bus clock / 4).
pub const TB_HZ: u64 = 60_750_000;
/// Broadway runs at 729 MHz, 12 core cycles per timebase tick; the
/// interpreter counts one cycle per instruction.
pub const INSTRUCTIONS_PER_TICK: u32 = 12;

pub const SPR_XER: usize = 1;
pub const SPR_LR: usize = 8;
pub const SPR_CTR: usize = 9;
pub const SPR_DEC: usize = 22;
pub const SPR_DMA_U: usize = 922;
pub const SPR_DMA_L: usize = 923;
pub const SPR_SRR0: usize = 26;
pub const SPR_SRR1: usize = 27;
pub const SPR_GQR0: usize = 912;

const MSR_EE: u32 = 1 << 15;
pub const MSR_FP: u32 = 1 << 13;

/// Whether an instruction touches the FPU (FP/paired-single arithmetic,
/// loads, stores and FPSCR access), which traps while MSR[FP] is clear.
pub fn uses_fpu(word: u32) -> bool {
    match word >> 26 {
        // Paired singles; dcbz_l (xo 1014) is a cache op.
        4 => (word >> 1) & 0x3ff != 1014,
        48..=57 | 59 | 60 | 61 | 63 => true,
        // Indexed FP loads/stores and stfiwx.
        31 => matches!((word >> 1) & 0x3ff, 535 | 567 | 599 | 631 | 663 | 695 | 727 | 759 | 983),
        _ => false,
    }
}
const XER_SO: u32 = 1 << 31;
const XER_CA: u32 = 1 << 29;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stop {
    Unimplemented { pc: u32, word: u32, op: Op },
    Fault { pc: u32, fault: Fault },
    StepLimit,
}

pub struct Cpu {
    pub pc: u32,
    pub gpr: [u32; 32],
    /// Each FPR holds the paired-single pair; ps0 doubles as the scalar.
    pub fpr: [[f64; 2]; 32],
    pub cr: u32,
    pub msr: u32,
    pub fpscr: u32,
    pub spr: Box<[u32; 1024]>,
    pub tb: u64,
    /// Instructions since the timebase last ticked.
    tick_frac: u32,
    dec_written: u64,
    /// Timebase at which DEC passes zero, or `u64::MAX` once it has.
    dec_fire: u64,
    dec_pending: bool,
    /// The interrupt input as last computed, and whether MSR[EE] was set
    /// then; it is computed again when either may have changed.
    irq_line: bool,
    irq_seen_ee: bool,
    /// Decoded instructions by PC, each with the word it was decoded from,
    /// so code that changes is decoded again.
    decoded: Box<[(u32, Op)]>,
    /// Recompiled blocks, run in place of the interpreter where they exist.
    pub blocks: Option<Box<blocks::Blocks>>,
    /// Instructions compiled code may still run before it has to hand
    /// back to the dispatcher (see `blocks`).
    pub budget: i32,
    /// Set by compiled code that stopped at an OS-style idle loop: the
    /// address of its branch, for the dispatcher's idle skip.
    pub idle_branch: u32,
    /// Instructions a high-level replacement ran itself beyond its trap
    /// (the function's own first ones), counted as the game would have.
    pub hle_steps: u32,
}

impl Default for Cpu {
    fn default() -> Self {
        Self {
            pc: 0,
            decoded: vec![(0, decode(0)); DECODE_CACHE].into_boxed_slice(),
            blocks: None,
            budget: 0,
            idle_branch: 0,
            hle_steps: 0,
            gpr: [0; 32],
            fpr: [[0.0; 2]; 32],
            cr: 0,
            msr: 0,
            fpscr: 0,
            spr: Box::new([0; 1024]),
            tb: 0,
            tick_frac: 0,
            dec_written: 0,
            dec_fire: u64::MAX,
            dec_pending: false,
            irq_line: false,
            irq_seen_ee: false,
        }
    }
}

/// PowerPC rotate mask from bit `mb` to bit `me` (big-endian numbering).
#[inline(always)]
fn mask(mb: u32, me: u32) -> u32 {
    let a = u32::MAX >> mb;
    let b = u32::MAX << (31 - me);
    if mb <= me { a & b } else { a | b }
}

/// Quantized-load/store scale: 2^-scale for loads, 2^scale for stores.
#[inline(always)]
fn qscale(bits: u32) -> f64 {
    let s = ((bits as i32) << 26) >> 26;
    2f64.powi(-s)
}

impl Cpu {
    #[inline(always)]
    fn xer(&self) -> u32 { self.spr[SPR_XER] }
    #[inline(always)]
    fn ca(&self) -> u32 { (self.xer() & XER_CA != 0) as u32 }
    #[inline(always)]
    fn set_ca(&mut self, c: bool) {
        self.spr[SPR_XER] = (self.xer() & !XER_CA) | if c { XER_CA } else { 0 };
    }

    #[inline(always)]
    fn set_cr_field(&mut self, field: u32, v: u32) {
        let sh = 28 - 4 * field;
        self.cr = (self.cr & !(0xf << sh)) | ((v & 0xf) << sh);
    }

    #[inline(always)]
    fn cmp_flags(&self, lt: bool, gt: bool) -> u32 {
        let so = (self.xer() & XER_SO != 0) as u32;
        (lt as u32) << 3 | (gt as u32) << 2 | ((!lt && !gt) as u32) << 1 | so
    }

    #[inline(always)]
    fn record(&mut self, v: u32) {
        let f = self.cmp_flags((v as i32) < 0, (v as i32) > 0);
        self.set_cr_field(0, f);
    }

    #[inline(always)]
    fn cr_bit(&self, b: u32) -> bool { self.cr & (1 << (31 - b)) != 0 }
    #[inline(always)]
    fn set_cr_bit(&mut self, b: u32, v: bool) {
        self.cr = (self.cr & !(1 << (31 - b))) | ((v as u32) << (31 - b));
    }

    /// Effective address for D-form (`d(rA)`, rA=0 means literal 0).
    #[inline(always)]
    fn ea_d(&self, i: &Insn) -> u32 {
        let base = if i.ra() == 0 { 0 } else { self.gpr[i.ra() as usize] };
        base.wrapping_add(i.simm() as u32)
    }

    #[inline(always)]
    fn ea_x(&self, i: &Insn) -> u32 {
        let base = if i.ra() == 0 { 0 } else { self.gpr[i.ra() as usize] };
        base.wrapping_add(self.gpr[i.rb() as usize])
    }

    /// Takes an exception: the return address (already in `pc`) goes to
    /// SRR0, the old MSR to SRR1, and execution continues at the vector
    /// with translation, interrupts and FP off. The guest's own handler,
    /// installed there by the SDK, runs and returns with `rfi`.
    fn exception(&mut self, vector: u32) {
        const MSR_ILE: u32 = 1 << 16;
        const MSR_ME: u32 = 1 << 12;
        const MSR_IP: u32 = 1 << 6;
        self.spr[SPR_SRR0] = self.pc;
        self.spr[SPR_SRR1] = self.msr & 0x87c0_ffff;
        let ile = (self.msr & MSR_ILE != 0) as u32;
        self.msr = (self.msr & (MSR_ILE | MSR_ME | MSR_IP)) | ile;
        self.pc = if self.msr & MSR_IP != 0 { 0xfff0_0000 } else { 0 } | vector;
    }

    /// DEC counts down once per timebase tick; `spr[SPR_DEC]` holds the value
    /// written at timebase `dec_written`.
    fn dec(&self) -> u32 {
        self.spr[SPR_DEC].wrapping_sub(self.tb.wrapping_sub(self.dec_written) as u32)
    }

    /// Locked-cache DMA, started by writing DMA_L with the trigger bit:
    /// copies 32-byte lines between RAM (DMA_U) and the locked cache
    /// (DMA_L), in the direction of the LD bit. NW4R builds its matrices in
    /// the locked cache and stores them to RAM this way for the GPU. The
    /// transfer completes at once, so the trigger bit reads back clear.
    fn locked_cache_dma(&mut self, m: &mut Memory, dma_l: u32) -> Result<(), Fault> {
        const TRIGGER: u32 = 1 << 1;
        const LOAD: u32 = 1 << 4;
        if dma_l & TRIGGER != 0 {
            let dma_u = self.spr[SPR_DMA_U];
            let lines = ((dma_u & 0x1f) << 2) | ((dma_l >> 2) & 3);
            let len = if lines == 0 { 128 } else { lines } * 32;
            let (mem_addr, cache_addr) = (dma_u & !0x1f, dma_l & !0x1f);
            let (src, dst) = if dma_l & LOAD != 0 { (mem_addr, cache_addr) } else { (cache_addr, mem_addr) };
            let data = m.read_bytes(src, len)?;
            m.load(dst, &data)?;
        }
        self.spr[SPR_DMA_L] = dma_l & !TRIGGER;
        Ok(())
    }

    /// The decrementer interrupt is requested when DEC goes from 0 to -1.
    fn set_dec(&mut self, v: u32) {
        self.spr[SPR_DEC] = v;
        self.dec_written = self.tb;
        self.dec_pending = false;
        self.dec_fire = if (v as i32) < 0 { u64::MAX } else { self.tb + v as u64 + 1 };
    }

    /// Runs up to `max_steps` instructions (a little more when the last
    /// one is a compiled block).
    pub fn run(&mut self, mem: &mut Memory, max_steps: u64, trace: &mut impl FnMut(u32)) -> (Stop, u64) {
        let mut n = 0;
        while n < max_steps {
            let pc = self.pc;
            // How many instructions ran, the address of the last, and
            // whether the timebase ticked. Devices, the decrementer and the
            // interrupt lines only change on a tick, so they are checked
            // then (and after an idle skip), not on every instruction.
            let compiled = match self.compiled(mem, pc) {
                Ok(c) => c,
                Err(fault) => return (Stop::Fault { pc: self.pc.wrapping_sub(4), fault }, n),
            };
            let (ran, last, mut tick) = match compiled {
                // With no idle loop to check, `last` is one no branch ends at.
                Some(ran) => (ran, std::mem::take(&mut self.idle_branch), self.count(ran, mem)),
                None => {
                    // The timebase moves before the instruction runs, so
                    // one completing a tick reads the new value.
                    let tick = self.count(1, mem);
                    match self.interpret(mem, pc) {
                        Ok(true) if self.hle_steps > 0 => {
                            let extra = std::mem::take(&mut self.hle_steps);
                            let tick = self.count(extra, mem) || tick;
                            (1 + extra, pc, tick)
                        }
                        Ok(true) => (1, pc, tick),
                        // FP unavailable was raised instead: the
                        // instruction runs again after the handler.
                        Ok(false) => {
                            n += 1;
                            trace(self.pc);
                            continue;
                        }
                        Err(stop) => return (stop, n),
                    }
                }
            };
            n += ran as u64;
            if self.pc == last.wrapping_sub(8) && self.is_idle_loop(mem, last) {
                self.skip_to_next_event(mem);
                tick = true;
            }
            if tick {
                if self.tb >= mem.next_event() {
                    mem.advance(self.tb);
                }
                if self.tb >= self.dec_fire {
                    self.dec_fire = u64::MAX;
                    self.dec_pending = true;
                }
                if self.msr & MSR_EE != 0 {
                    if mem.irq_dirty || !self.irq_seen_ee {
                        mem.irq_dirty = false;
                        self.irq_line = mem.irq_pending();
                    }
                    if self.irq_line {
                        self.exception(0x500);
                    } else if self.dec_pending {
                        self.dec_pending = false;
                        self.exception(0x900);
                    }
                }
                self.irq_seen_ee = self.msr & MSR_EE != 0;
            }
            if self.pc != last.wrapping_add(4) {
                trace(self.pc);
            }
        }
        (Stop::StepLimit, n)
    }

    /// Runs compiled code from `pc` if a block starts there, returning how
    /// many instructions ran: None if there is no block, or its first one
    /// can't run now (it would trap on the FPU, or in exact mode reach the
    /// next tick). It chains on until the next device event or decrementer
    /// expiry, so nothing on the timebase is missed; in exact mode only
    /// until the next tick, so interrupts land where the interpreter takes
    /// them.
    #[inline(always)]
    fn compiled(&mut self, mem: &mut Memory, pc: u32) -> Result<Option<u32>, Fault> {
        let Some(blocks) = self.blocks.as_ref() else { return Ok(None) };
        let Some(run) = blocks.at(pc) else {
            return if blocks.translated() { self.translated(mem, pc) } else { Ok(None) };
        };
        let budget = self.chain_budget(mem, blocks.exact);
        self.budget = budget;
        mem.mmio_touched = false;
        run(self, mem)?;
        let ran = (budget - self.budget) as u32;
        Ok((ran > 0).then_some(ran))
    }

    /// [`Self::compiled`] for code translated at load time (`--jit`). Out
    /// of line, so the Rust blocks' dispatch stays as it was.
    #[inline(never)]
    fn translated(&mut self, mem: &mut Memory, pc: u32) -> Result<Option<u32>, Fault> {
        let Some(blocks) = self.blocks.as_ref() else { return Ok(None) };
        let Some(run) = blocks.jit_at(pc) else { return Ok(None) };
        let budget = self.chain_budget(mem, blocks.exact);
        self.budget = budget;
        mem.mmio_touched = false;
        // SAFETY: translated code reads and writes the CPU and memory as
        // the `ops` functions it was translated from do.
        let status = unsafe { run(self, mem) };
        if status != 0 {
            return Err(Fault::Unmapped(status as u32));
        }
        let ran = (budget - self.budget) as u32;
        Ok((ran > 0).then_some(ran))
    }

    /// How many instructions a chain may run: until the next device event
    /// or decrementer expiry, or in exact mode the next tick.
    #[inline(always)]
    fn chain_budget(&self, mem: &Memory, exact: bool) -> i32 {
        if exact {
            (INSTRUCTIONS_PER_TICK - 1 - self.tick_frac) as i32
        } else {
            let deadline = mem.next_event().min(self.dec_fire);
            let ticks = deadline.saturating_sub(self.tb);
            let left = ticks.saturating_mul(INSTRUCTIONS_PER_TICK as u64).saturating_sub(self.tick_frac as u64);
            // Always room for the longest block; never so long that the
            // window's slices or input wait.
            left.clamp(MAX_BLOCK, MAX_CHAIN) as i32
        }
    }

    /// Counts `ran` instructions towards the timebase; true if it ticked.
    #[inline]
    fn count(&mut self, ran: u32, mem: &mut Memory) -> bool {
        self.tick_frac += ran;
        if self.tick_frac < INSTRUCTIONS_PER_TICK {
            return false;
        }
        self.tb = self.tb.wrapping_add((self.tick_frac / INSTRUCTIONS_PER_TICK) as u64);
        self.tick_frac %= INSTRUCTIONS_PER_TICK;
        mem.now = self.tb;
        true
    }

    /// Interprets the instruction at `pc`. Returns Ok(false) if it raised
    /// FP unavailable instead of running.
    #[inline]
    fn interpret(&mut self, mem: &mut Memory, pc: u32) -> Result<bool, Stop> {
        let word = mem.fetch(pc).map_err(|fault| Stop::Fault { pc, fault })?;
        let slot = &mut self.decoded[(pc as usize >> 2) % DECODE_CACHE];
        if slot.0 != word {
            *slot = (word, decode(word));
        }
        let i = Insn { addr: pc, word, op: slot.1 };
        // FP unavailable: the OS saves the interrupted thread's FPRs
        // lazily from this exception (interrupt handlers run with FP
        // off), then the instruction runs again.
        if self.msr & MSR_FP == 0 && uses_fpu(word) {
            self.pc = pc;
            self.exception(0x800);
            return Ok(false);
        }
        self.pc = pc.wrapping_add(4);
        match ops::exec(self, mem, &i) {
            Ok(true) => Ok(true),
            // A replaced SDK function: run it and return to the caller,
            // unless it went somewhere else (a guest callback).
            Ok(false) => match crate::hle::call(word, self, mem) {
                Ok(true) => {
                    // Replacements talk to devices directly.
                    mem.irq_dirty = true;
                    if self.pc == pc.wrapping_add(4) {
                        self.pc = self.spr[SPR_LR] & !3;
                    }
                    Ok(true)
                }
                Ok(false) => Err(Stop::Unimplemented { pc, word, op: i.op }),
                Err(fault) => Err(Stop::Fault { pc, fault }),
            },
            Err(fault) => Err(Stop::Fault { pc, fault }),
        }
    }

    /// A wait loop that only an interrupt can end: `lwz rX, d(rY)` from RAM,
    /// `cmpwi rX, 0`, and the conditional branch at `branch` back to the
    /// load, just taken. The OS idle loop is one.
    fn is_idle_loop(&self, mem: &mut Memory, branch: u32) -> bool {
        let (Ok(load), Ok(cmp), Ok(bc)) = (mem.r32(branch - 8), mem.r32(branch - 4), mem.r32(branch)) else {
            return false;
        };
        let rx = (load >> 21) & 31;
        let ry = (load >> 16) & 31;
        let addr = if ry == 0 { 0 } else { self.gpr[ry as usize] }.wrapping_add(load as u16 as i16 as i32 as u32);
        load >> 26 == 32
            && cmp == (11 << 26 | rx << 16)
            && bc >> 26 == 16
            && bc & 0xffff == 0xfff8
            && addr & 0xfe00_0000 != 0xcc00_0000
    }

    /// Fast-forwards time to the next device event or decrementer expiry,
    /// as a wait loop would spin until then anyway.
    fn skip_to_next_event(&mut self, mem: &mut Memory) {
        let next = mem.next_event().min(self.dec_fire);
        if next != u64::MAX && next > self.tb {
            self.tb = next;
            self.tick_frac = 0;
            mem.now = self.tb;
        }
    }

    #[inline(always)]
    fn branch_cond(&mut self, i: &Insn, use_ctr: bool) -> bool {
        let bo = i.bo();
        let mut ok = true;
        if use_ctr && bo & 4 == 0 {
            self.spr[SPR_CTR] = self.spr[SPR_CTR].wrapping_sub(1);
            ok &= (self.spr[SPR_CTR] != 0) ^ (bo & 2 != 0);
        }
        if bo & 16 == 0 {
            ok &= self.cr_bit(i.bi()) == (bo & 8 != 0);
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(words: &[u32]) -> Cpu {
        let mut m = Memory::default();
        for (k, w) in words.iter().enumerate() {
            m.w32(0x8000_3000 + 4 * k as u32, *w).unwrap();
        }
        let mut c = Cpu { pc: 0x8000_3000, ..Default::default() };
        c.run(&mut m, words.len() as u64, &mut |_| {});
        c
    }

    #[test]
    fn arithmetic_and_rotate() {
        // li r3,5; addi r3,r3,-7; rlwinm r4,r3,2,0,29; srawi r5,r3,1
        let c = run(&[0x3860_0005, 0x3863_fff9, 0x5464_103a, 0x7c65_0e70]);
        assert_eq!(c.gpr[3] as i32, -2);
        assert_eq!(c.gpr[4] as i32, -8);
        assert_eq!(c.gpr[5] as i32, -1);
    }

    #[test]
    fn compare_and_branch() {
        // li r3,1; cmpwi r3,1; beq +8; li r4,9; li r5,7
        let c = run(&[0x3860_0001, 0x2c03_0001, 0x4182_0008, 0x3880_0009, 0x38a0_0007]);
        assert_eq!(c.gpr[4], 0);
        assert_eq!(c.gpr[5], 7);
    }

    #[test]
    fn subfc_sets_carry() {
        // li r3,5; li r4,3; subfc r5,r4,r3 (r3-r4 = 2, no borrow -> CA=1)
        let c = run(&[0x3860_0005, 0x3880_0003, 0x7ca4_1810]);
        assert_eq!(c.gpr[5], 2);
        assert_eq!(c.ca(), 1);
    }

    /// A paired-single A-form instruction: opcode 4, frD, frA, frB, frC, xo.
    fn ps(d: u32, a: u32, b: u32, c: u32, xo: u32) -> u32 {
        4 << 26 | d << 21 | a << 16 | b << 11 | c << 6 | xo << 1
    }

    fn run_fp(words: &[u32], fpr: &[(usize, [f64; 2])]) -> Cpu {
        let mut m = Memory::default();
        for (k, w) in words.iter().enumerate() {
            m.w32(0x8000_3000 + 4 * k as u32, *w).unwrap();
        }
        let mut c = Cpu { pc: 0x8000_3000, msr: MSR_FP, ..Default::default() };
        for &(r, v) in fpr {
            c.fpr[r] = v;
        }
        c.run(&mut m, words.len() as u64, &mut |_| {});
        c
    }

    #[test]
    fn paired_single_scalar_forms() {
        let regs = [(1, [2.0, 3.0]), (2, [10.0, 20.0]), (3, [5.0, 7.0])];
        // ps_muls0 f4,f1,f3; ps_muls1 f5,f1,f3; ps_madds1 f6,f1,f3,f2
        let c = run_fp(&[ps(4, 1, 0, 3, 12), ps(5, 1, 0, 3, 13), ps(6, 1, 2, 3, 15)], &regs);
        assert_eq!(c.fpr[4], [10.0, 15.0]);
        assert_eq!(c.fpr[5], [14.0, 21.0]);
        assert_eq!(c.fpr[6], [24.0, 41.0]);
    }

    #[test]
    fn paired_single_sum_select_compare() {
        let regs = [(1, [2.0, -3.0]), (2, [10.0, 20.0]), (3, [5.0, 7.0])];
        // ps_sum0 f4,f1,f3,f2; ps_sum1 f5,f1,f3,f2; ps_sel f6,f1,f3,f2;
        // ps_cmpu1 cr1,f1,f2
        let cmpu1 = 4 << 26 | 1 << 23 | 1 << 16 | 2 << 11 | 64 << 1;
        let c = run_fp(&[ps(4, 1, 2, 3, 10), ps(5, 1, 2, 3, 11), ps(6, 1, 2, 3, 23), cmpu1], &regs);
        assert_eq!(c.fpr[4], [22.0, 7.0]);
        assert_eq!(c.fpr[5], [5.0, 22.0]);
        assert_eq!(c.fpr[6], [5.0, 20.0]);
        assert_eq!((c.cr >> 24) & 0xf, 8, "cr1: ps1 -3 < 20");
    }

    #[test]
    fn fsel_and_fctiw_rounding() {
        // fsel f4,f1,f3,f2 (f1 >= 0 ? f3 : f2); fctiw f5,f6 with RN = nearest.
        let fsel = 63 << 26 | 4 << 21 | 1 << 16 | 2 << 11 | 3 << 6 | 23 << 1;
        let fctiw = 63 << 26 | 5 << 21 | 6 << 11 | 14 << 1;
        let c = run_fp(&[fsel, fctiw], &[(1, [-0.5, 0.0]), (2, [7.0, 0.0]), (3, [9.0, 0.0]), (6, [2.5, 0.0])]);
        assert_eq!(c.fpr[4][0], 7.0);
        assert_eq!(c.fpr[5][0].to_bits() as u32, 2, "2.5 rounds to even");
    }

    #[test]
    fn string_store_then_load() {
        // stswi r3,r10,6 then lswi r20,r10,6: two registers, the second
        // only partly used and zero-filled.
        let mut m = Memory::default();
        let stswi = 31 << 26 | 3 << 21 | 10 << 16 | 6 << 11 | 725 << 1;
        let lswi = 31 << 26 | 20 << 21 | 10 << 16 | 6 << 11 | 597 << 1;
        m.w32(0x8000_3000, stswi).unwrap();
        m.w32(0x8000_3004, lswi).unwrap();
        let mut c = Cpu { pc: 0x8000_3000, ..Default::default() };
        c.gpr[3] = 0x1122_3344;
        c.gpr[4] = 0x5566_7788;
        c.gpr[10] = 0x8000_4000;
        c.run(&mut m, 2, &mut |_| {});
        assert_eq!(m.r32(0x8000_4000).unwrap(), 0x1122_3344);
        assert_eq!(m.r16(0x8000_4004).unwrap(), 0x5566);
        assert_eq!((c.gpr[20], c.gpr[21]), (0x1122_3344, 0x5566_0000));
    }

    /// With MSR[FP] clear, an FP instruction takes the FP-unavailable
    /// exception and is not executed; SRR0 points back at it.
    #[test]
    fn fp_unavailable_traps() {
        let mut m = Memory::default();
        m.w32(0x8000_3000, 63 << 26 | 1 << 21 | 2 << 11 | 72 << 1).unwrap(); // fmr f1,f2
        let mut c = Cpu { pc: 0x8000_3000, msr: 0x0000_0032, ..Default::default() };
        c.fpr[2] = [5.0, 5.0];
        c.run(&mut m, 1, &mut |_| {});
        assert_eq!((c.pc, c.spr[SPR_SRR0], c.fpr[1][0]), (0x800, 0x8000_3000, 0.0));
    }

    /// A load/compare/branch-back loop on RAM skips ahead to the next
    /// device event instead of spinning.
    #[test]
    fn idle_loop_skips_time() {
        let mut m = Memory::default();
        // lwz r0,0(r3); cmpwi r0,0; beq -8
        for (k, w) in [0x8003_0000u32, 0x2c00_0000, 0x4182_fff8].iter().enumerate() {
            m.w32(0x8000_3000 + 4 * k as u32, *w).unwrap();
        }
        let mut c = Cpu { pc: 0x8000_3000, ..Default::default() };
        c.gpr[3] = 0x8000_4000;
        c.run(&mut m, 30, &mut |_| {});
        assert!(c.tb > 1000, "timebase only reached {}", c.tb);
        assert_eq!(c.pc, 0x8000_3000);
    }

    /// mtspr rS, spr.
    fn mtspr(rs: u32, spr: u32) -> u32 {
        31 << 26 | rs << 21 | (spr & 31) << 16 | (spr >> 5) << 11 | 467 << 1
    }

    /// Locked-cache DMA both ways: two lines from RAM into the locked
    /// cache, then back out to another RAM address.
    #[test]
    fn locked_cache_dma_round_trip() {
        let mut m = Memory::default();
        for k in 0..16u32 {
            m.w32(0x8000_4000 + 4 * k, 0x1000 + k).unwrap();
        }
        let words = [mtspr(3, 922), mtspr(4, 923), mtspr(5, 922), mtspr(6, 923)];
        for (k, w) in words.iter().enumerate() {
            m.w32(0x8000_3000 + 4 * k as u32, *w).unwrap();
        }
        let mut c = Cpu { pc: 0x8000_3000, ..Default::default() };
        c.gpr[3] = 0x0000_4000; // RAM address, length high bits 0
        c.gpr[4] = 0xe000_0000 | 2 << 2 | 1 << 4 | 1 << 1; // 2 lines, load, trigger
        c.gpr[5] = 0x0000_5000;
        c.gpr[6] = 0xe000_0000 | 2 << 2 | 1 << 1; // 2 lines, store, trigger
        c.run(&mut m, 4, &mut |_| {});
        assert_eq!(m.r32(0xe000_0004).unwrap(), 0x1001);
        assert_eq!(m.r32(0x8000_503c).unwrap(), 0x100f);
        assert_eq!(c.spr[SPR_DMA_L] & 2, 0, "trigger reads back clear");
    }

    /// Code rewritten at an address already run is decoded again.
    #[test]
    fn decode_cache_sees_new_code() {
        let mut m = Memory::default();
        m.w32(0x8000_3000, 14 << 26 | 3 << 21 | 1).unwrap(); // li r3, 1
        let mut c = Cpu { pc: 0x8000_3000, ..Default::default() };
        c.run(&mut m, 1, &mut |_| {});
        assert_eq!(c.gpr[3], 1);
        m.w32(0x8000_3000, 21 << 26 | 3 << 21 | 3 << 16 | 1 << 11 | 31 << 1).unwrap(); // rlwinm r3, r3, 1, 0, 31
        c.pc = 0x8000_3000;
        c.run(&mut m, 1, &mut |_| {});
        assert_eq!(c.gpr[3], 2);
    }
}