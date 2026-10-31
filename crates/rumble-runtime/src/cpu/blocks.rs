//! Compiled blocks: straight-line runs of guest instructions compiled to
//! host code, either ahead of time to Rust by `rumble-port recomp` (the
//! `rumble-blocks` crate) or at load time by the Cranelift translator
//! (`rumble-jit`). Both start from [`plan`], so they cut the game's code
//! into the same blocks: each ends at a branch, before an instruction `ops`
//! has no function for, or where another block starts. A guest function's
//! blocks share one host function, which chains between them while
//! [`Cpu::budget`](super::Cpu::budget) lasts.
//!
//! [`Cpu::run`](super::Cpu::run) runs compiled code when a block starts at
//! the PC and interprets everywhere else, so the two mix freely.

use super::{ops, uses_fpu, Cpu};
use crate::mem::{Fault, Memory};
use rumble_recomp::analysis::Program;
use rumble_recomp::ppc::{Insn, Op};
use std::collections::{BTreeSet, HashSet};

/// Runs from the block at `c.pc` until the budget, the function or a stop
/// point ends the chain, leaving the next PC in `c.pc` and taking each
/// block's length off `c.budget`. Runs nothing if the first block doesn't
/// fit or can't run.
pub type BlockFn = fn(&mut Cpu, &mut Memory) -> Result<(), Fault>;

/// The same for code translated at load time: returns 0, or on a fault
/// `1 << 32 | address` (`Fault::Unmapped`).
pub type JitFn = unsafe extern "C" fn(*mut Cpu, *mut Memory) -> u64;

/// A compiled block as the generator describes it.
pub struct Block {
    pub start: u32,
    /// Instructions in the block.
    pub len: u32,
    /// Whether any instruction in it uses the FPU, which traps while
    /// MSR[FP] is clear.
    pub fpu: bool,
    /// [`hash_words`] of the instructions it was compiled from.
    pub hash: u64,
    /// The guest function it belongs to, whose host function `run` is.
    pub func: u32,
    pub run: BlockFn,
}

/// FNV-1a over instruction words, to check a block against what is in RAM.
pub fn hash_words(words: impl IntoIterator<Item = u32>) -> u64 {
    words.into_iter().fold(0xcbf2_9ce4_8422_2325, |h, w| (h ^ w as u64).wrapping_mul(0x0100_0000_01b3))
}

/// Code addresses covered by the lookup table: cached MEM1.
const BASE: u32 = 0x8000_0000;

/// A guest function's blocks, in address order.
pub struct FuncPlan {
    pub start: u32,
    pub blocks: Vec<BlockPlan>,
}

pub struct BlockPlan {
    pub start: u32,
    pub body: Vec<Insn>,
}

impl BlockPlan {
    pub fn fpu(&self) -> bool {
        self.body.iter().any(|i| uses_fpu(i.word))
    }

    pub fn hash(&self) -> u64 {
        hash_words(self.body.iter().map(|i| i.word))
    }

    /// The OS-style wait loop the dispatcher skips: `lwz rX, d(rY)`,
    /// `cmpwi rX, 0`, and a conditional branch back to the load. Compiled
    /// code stops there with `Cpu::idle_branch` set.
    pub fn is_idle_loop(&self) -> bool {
        let [load, cmp, bc] = &self.body[..] else { return false };
        let rx = load.rd();
        load.op == Op::Lwz && cmp.word == (11 << 26 | rx << 16) && bc.op == Op::Bc && bc.word & 0xffff == 0xfff8
    }

    /// Whether the chain has to hand back to the dispatcher after this
    /// block (see [`stops_chain`]).
    pub fn stops_chain(&self) -> bool {
        self.body.iter().any(stops_chain)
    }
}

/// Whether the dispatcher has to look after this instruction: it changes
/// the interrupt enable (`mtmsr`, `rfi`, `sc`), or reads or sets the timer
/// state, which the dispatcher keeps current (timebase, DEC, locked-cache
/// DMA). Moves to LR, CTR, XER, the GQRs and SPRGs don't count.
pub fn stops_chain(i: &Insn) -> bool {
    match i.op {
        Op::Mtmsr | Op::Rfi | Op::Sc | Op::Mftb => true,
        Op::Mfspr => i.spr() == 22,
        Op::Mtspr => !matches!(i.spr(), 1 | 8 | 9 | 272..=275 | 912..=919),
        _ => false,
    }
}

/// Cuts the analysed program into blocks, grouped by function. A block
/// starts at every function and basic-block start the analysis found, and
/// after every branch (so a call's return lands on one). It runs straight
/// on until a branch (included), an instruction `ops` has no function for
/// (left to the interpreter), the next block's start, or `max_len`.
pub fn plan(prog: &Program, max_len: u32) -> Vec<FuncPlan> {
    let mut starts = BTreeSet::new();
    for f in &prog.functions {
        starts.insert(f.start);
        starts.extend(f.blocks.iter().map(|b| b.start));
        for addr in (f.start..f.end).step_by(4) {
            if prog.insn(addr).is_some_and(|i| i.is_branch()) {
                starts.insert(addr + 4);
            }
        }
    }

    let mut funcs: Vec<FuncPlan> = Vec::new();
    for &start in &starts {
        let Some(f) = prog.function_at(start) else { continue };
        let mut body = Vec::new();
        let mut addr = start;
        while addr < f.end && (body.len() as u32) < max_len {
            if addr != start && starts.contains(&addr) {
                break;
            }
            let Some(i) = prog.insn(addr) else { break };
            if ops::handler(i.op).is_none() {
                break;
            }
            body.push(i);
            addr += 4;
            if i.is_branch() {
                break;
            }
        }
        if body.is_empty() {
            continue;
        }
        let block = BlockPlan { start, body };
        match funcs.last_mut() {
            Some(fp) if fp.start == f.start => fp.blocks.push(block),
            _ => funcs.push(FuncPlan { start: f.start, blocks: vec![block] }),
        }
    }
    funcs
}

/// The functions with a block whose instructions differ from RAM (code the
/// runtime patched, such as replaced SDK functions), given each block's
/// start, length, hash and function. They are left out whole, since their
/// host function chains between all of their blocks.
pub fn differing_functions(mem: &mut Memory, blocks: impl IntoIterator<Item = (u32, u32, u64, u32)>) -> HashSet<u32> {
    let mut bad = HashSet::new();
    for (start, len, hash, func) in blocks {
        let words: Option<Vec<u32>> = (0..len).map(|n| mem.fetch(start + 4 * n).ok()).collect();
        if start < BASE || words.map(hash_words) != Some(hash) {
            bad.insert(func);
        }
    }
    bad
}

/// The blocks that match RAM, by address.
pub struct Blocks {
    table: Vec<Option<BlockFn>>,
    /// Translated code, by address like `table`; empty unless translated.
    /// Kept apart so the Rust blocks' lookup is a single load: the
    /// dispatcher does one for every chain.
    jit: Vec<Option<JitFn>>,
    /// Only run a block when it ends before the next timebase tick, so
    /// interrupts and device events land on the same instruction as in
    /// the interpreter. Used to check blocks against it.
    pub exact: bool,
    /// Whatever owns the code `jit` points into.
    _code: Option<Box<dyn std::any::Any>>,
}

/// A table by (pc - BASE) / 4 with `entries` at their addresses.
fn index<T: Copy>(entries: impl IntoIterator<Item = (u32, T)>) -> Vec<Option<T>> {
    let entries: Vec<_> = entries.into_iter().collect();
    let end = entries.iter().map(|e| e.0 + 4).max().unwrap_or(BASE);
    let mut table = vec![None; ((end - BASE) / 4) as usize];
    for (start, f) in entries {
        table[((start - BASE) / 4) as usize] = Some(f);
    }
    table
}

impl Blocks {
    /// Indexes `blocks`, leaving out every function with a block whose
    /// instructions differ from RAM ([`differing_functions`]). Returns the
    /// table and how many blocks were left out.
    pub fn new(blocks: &'static [Block], mem: &mut Memory) -> (Self, usize) {
        let bad = differing_functions(mem, blocks.iter().map(|b| (b.start, b.len, b.hash, b.func)));
        let skipped = blocks.iter().filter(|b| bad.contains(&b.func)).count();
        let table = index(blocks.iter().filter(|b| !bad.contains(&b.func)).map(|b| (b.start, b.run)));
        (Blocks { table, jit: Vec::new(), exact: false, _code: None }, skipped)
    }

    /// A table of translated functions by the block starts they run.
    /// `code` keeps the code they point into alive.
    pub fn jit(entries: Vec<(u32, JitFn)>, code: Box<dyn std::any::Any>) -> Self {
        Blocks { table: Vec::new(), jit: index(entries), exact: false, _code: Some(code) }
    }

    pub fn len(&self) -> usize {
        self.table.iter().filter(|e| e.is_some()).count() + self.jit.iter().filter(|e| e.is_some()).count()
    }

    #[inline(always)]
    pub fn at(&self, pc: u32) -> Option<BlockFn> {
        *self.table.get((pc.wrapping_sub(BASE) / 4) as usize)?
    }

    /// Whether there is translated code.
    #[inline(always)]
    pub fn translated(&self) -> bool {
        !self.jit.is_empty()
    }

    #[inline(always)]
    pub fn jit_at(&self, pc: u32) -> Option<JitFn> {
        *self.jit.get((pc.wrapping_sub(BASE) / 4) as usize)?
    }
}
