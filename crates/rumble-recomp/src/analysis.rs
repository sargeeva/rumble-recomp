//! Whole-program analysis of a DOL: function discovery, jump-table
//! resolution and basic-block construction. The output is what the code
//! generator consumes.

use crate::ppc::{Insn, Op};
use rumble_formats::dol::{Dol, SectionKind};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedSource {
    Entry,
    Call,
    Prologue,
    /// First instruction after zero padding.
    AfterPadding,
    /// Target of a tail-call `b` that lands on a prologue.
    TailCall,
    DataPointer,
}

#[derive(Debug, Clone)]
pub struct JumpTable {
    /// Address of the `bctr`.
    pub site: u32,
    pub table: u32,
    pub targets: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Falls through or branches to blocks in the same function.
    Local,
    Return,
    TailCall(u32),
    /// `bctr` resolved through a jump table.
    Switch,
    /// `bctr` through a pointer loaded from memory: a C++ virtual or
    /// function-pointer call in tail position.
    IndirectTailCall,
    /// `bctr` we could not classify.
    IndirectJump,
    Rfi,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub start: u32,
    pub end: u32,
    pub succs: Vec<u32>,
    pub exit: Exit,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub start: u32,
    pub end: u32,
    pub source: SeedSource,
    pub blocks: Vec<Block>,
    pub calls: BTreeSet<u32>,
    pub indirect_calls: usize,
    pub illegal: usize,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub insns: usize,
    pub op_counts: BTreeMap<Op, usize>,
    pub jump_tables: usize,
    pub indirect_tail_calls: usize,
    pub unresolved_bctr: usize,
}

pub struct Program<'a> {
    pub dol: &'a Dol<'a>,
    pub functions: Vec<Function>,
    pub jump_tables: BTreeMap<u32, JumpTable>,
    pub stats: Stats,
}

impl<'a> Program<'a> {
    pub fn insn(&self, addr: u32) -> Option<Insn> {
        insn_at(self.dol, addr)
    }

    pub fn function_at(&self, addr: u32) -> Option<&Function> {
        let i = self.functions.partition_point(|f| f.start <= addr).checked_sub(1)?;
        let f = &self.functions[i];
        (addr < f.end).then_some(f)
    }
}

fn insn_at(dol: &Dol, addr: u32) -> Option<Insn> {
    dol.is_code(addr).then(|| Insn::new(addr, dol.read_u32(addr).unwrap()))
}

fn is_prologue(i: &Insn) -> bool {
    // stwu r1, -N(r1)  or  mflr r0
    (i.op == Op::Stwu && i.rs() == 1 && i.ra() == 1 && i.simm() < 0)
        || (i.op == Op::Mfspr && i.spr() == 8 && i.rd() == 0)
}

fn ends_function(i: &Insn) -> bool {
    i.word == 0
        || (i.op == Op::Bclr && i.is_unconditional() && !i.lk())
        || (i.op == Op::Bcctr && i.is_unconditional() && !i.lk())
        || (i.op == Op::B && !i.lk())
        || i.op == Op::Rfi
}

/// True if the CTR feeding the `bctr` at `site` was loaded with a plain
/// `lwz` (vtable slot or function pointer), i.e. not a jump table.
fn ctr_from_load(dol: &Dol, site: u32) -> bool {
    let mut reg = None;
    for back in 1..=8u32 {
        let Some(i) = site.checked_sub(back * 4).and_then(|a| insn_at(dol, a)) else { return false };
        match i.op {
            Op::Mtspr if i.spr() == 9 && reg.is_none() => reg = Some(i.rs()),
            Op::Lwz | Op::Lwzu if Some(i.rd()) == reg => return true,
            Op::Lwzx if Some(i.rd()) == reg => return false,
            _ if Some(i.rd()) == reg => return false,
            _ => {}
        }
    }
    false
}

/// Values of the small-data base registers (r2, r13), read from the
/// `lis/ori` pairs at the program entry point.
fn small_data_bases(dol: &Dol) -> [Option<u32>; 2] {
    let slot = |r: u32| match r {
        2 => Some(0),
        13 => Some(1),
        _ => None,
    };
    let mut regs = [None; 2];
    for k in 0..64 {
        let Some(i) = insn_at(dol, dol.entry + k * 4) else { break };
        match i.op {
            Op::Addis if i.ra() == 0 => {
                if let Some(n) = slot(i.rd()) {
                    regs[n] = Some(i.uimm() << 16);
                }
            }
            Op::Ori if i.ra() == i.rs() => {
                if let Some(n) = slot(i.ra()) {
                    regs[n] = regs[n].map(|v| v | i.uimm());
                }
            }
            Op::Addi if i.ra() == i.rd() => {
                if let Some(n) = slot(i.rd()) {
                    regs[n] = regs[n].map(|v| v.wrapping_add(i.simm() as u32));
                }
            }
            _ => {}
        }
    }
    regs
}

/// Recognizes the Metrowerks switch idiom ending in `bctr`:
/// `lis rT,hi; addi rT,rT,lo; ... lwzx rX,rT,rI; mtctr rX; bctr`, where the
/// `lis` may be far above, or rT may be the small-data base (r2/r13).
fn find_table_addr(dol: &Dol, bctr: u32, sda: [Option<u32>; 2]) -> Option<u32> {
    let mut ctr_reg = None;
    let mut base_reg = None;
    let mut lo: Option<i32> = None;
    for back in 1..=128u32 {
        if let Some(r) = base_reg {
            if lo.is_some() && (r == 2 || r == 13) {
                let base = sda[if r == 2 { 0 } else { 1 }]?;
                return Some(base.wrapping_add(lo.unwrap() as u32));
            }
        }
        let i = insn_at(dol, bctr.checked_sub(back * 4)?)?;
        if is_prologue(&i) && back > 4 {
            return None;
        }
        match i.op {
            Op::Mtspr if i.spr() == 9 && ctr_reg.is_none() => ctr_reg = Some(i.rs()),
            Op::Lwzx if base_reg.is_none() && Some(i.rd()) == ctr_reg => base_reg = Some(i.ra()),
            Op::Addi if base_reg.is_some() && Some(i.rd()) == base_reg && lo.is_none() => {
                lo = Some(i.simm());
                base_reg = Some(i.ra());
            }
            Op::Addis if base_reg.is_some() && Some(i.rd()) == base_reg && i.ra() == 0 => {
                return Some(((i.uimm() << 16) as i32).wrapping_add(lo.unwrap_or(0)) as u32);
            }
            // Any other write to the base register breaks the chain.
            _ if base_reg.is_some() && writes_gpr(&i, base_reg.unwrap()) => return None,
            _ => {}
        }
    }
    None
}

/// Conservative "does this instruction write GPR `r`" for backward scans.
fn writes_gpr(i: &Insn, r: u32) -> bool {
    use Op::*;
    match i.op {
        // Logical, shift and rotate forms write rA.
        Ori | Oris | Xori | Xoris | AndiRc | AndisRc | And | Andc | Or | Orc | Xor | Nand | Nor | Eqv | Slw
        | Srw | Sraw | Srawi | Cntlzw | Extsb | Extsh | Rlwinm | Rlwimi | Rlwnm => i.ra() == r,
        // Update-form integer loads write rD and rA.
        Lwzu | Lhzu | Lhau | Lbzu | Lwzux | Lhzux | Lhaux | Lbzux => i.rd() == r || i.ra() == r,
        // Update-form stores and FP/paired-single accesses write only rA.
        Stwu | Sthu | Stbu | Stwux | Sthux | Stbux | Lfsu | Lfdu | Lfsux | Lfdux | Stfsu | Stfdu | Stfsux
        | Stfdux | PsqLu | PsqLux | PsqStu | PsqStux => i.ra() == r,
        // Load multiple / load string fill rD upward.
        Lmw | Lswi | Lswx => r >= i.rd(),
        // Integer arithmetic, loads and register moves write rD.
        Addi | Addis | Addic | AddicRc | Subfic | Mulli | Add | Addc | Adde | Addze | Addme | Subf | Subfc
        | Subfe | Subfze | Subfme | Neg | Mullw | Mulhw | Mulhwu | Divw | Divwu | Lwz | Lhz | Lha | Lbz | Lwzx
        | Lhzx | Lhax | Lbzx | Lwbrx | Lhbrx | Lwarx | Eciwx | Mfspr | Mftb | Mfcr | Mfmsr | Mfsr | Mfsrin => {
            i.rd() == r
        }
        // Calls clobber the volatile registers r0 and r3-r12.
        B | Bc | Bclr | Bcctr if i.lk() => r == 0 || (3..=12).contains(&r),
        _ => false,
    }
}

pub fn analyze<'a>(dol: &'a Dol<'a>) -> Program<'a> {
    let mut stats = Stats::default();
    let mut seeds: BTreeMap<u32, SeedSource> = BTreeMap::new();
    seeds.insert(dol.entry, SeedSource::Entry);

    // Pass 1: decode everything; collect call targets and prologues that
    // directly follow the end of another function.
    let mut bctr_sites = Vec::new();
    for s in dol.text() {
        let mut prev_ended = true;
        for addr in (s.addr..s.end()).step_by(4) {
            let i = insn_at(dol, addr).unwrap();
            stats.insns += 1;
            *stats.op_counts.entry(i.op).or_default() += 1;
            if i.op == Op::B && i.lk() {
                if let Some(t) = i.branch_target().filter(|&t| dol.is_code(t)) {
                    seeds.insert(t, SeedSource::Call);
                }
            }
            if i.op == Op::Bcctr && i.is_unconditional() && !i.lk() {
                bctr_sites.push(addr);
            }
            if prev_ended && i.word != 0 && is_prologue(&i) {
                seeds.entry(addr).or_insert(SeedSource::Prologue);
            } else if i.word != 0 && addr > s.addr && dol.read_u32(addr - 4) == Some(0) {
                seeds.entry(addr).or_insert(SeedSource::AfterPadding);
            }
            if i.op == Op::B && !i.lk() {
                if let Some(t) = i.branch_target().filter(|&t| insn_at(dol, t).is_some_and(|t| is_prologue(&t))) {
                    seeds.entry(t).or_insert(SeedSource::TailCall);
                }
            }
            prev_ended = ends_function(&i);
        }
    }

    // Jump tables: their entries are code pointers in data that must not
    // be mistaken for function starts.
    let mut jump_tables = BTreeMap::new();
    let mut table_words = BTreeSet::new();
    let sda = small_data_bases(dol);
    for &site in &bctr_sites {
        let Some(table) = find_table_addr(dol, site, sda) else { continue };
        let mut targets = Vec::new();
        let mut a = table;
        // A switch's cases sit close to its bctr; stop at the first entry
        // that isn't nearby code.
        while let Some(t) = dol.read_u32(a).filter(|&t| dol.is_code(t) && t.abs_diff(site) < 0x10000) {
            table_words.insert(a);
            targets.push(t);
            a += 4;
        }
        if !targets.is_empty() {
            jump_tables.insert(site, JumpTable { site, table, targets });
        }
    }
    stats.jump_tables = jump_tables.len();

    // Code pointers stored in data (vtables, callbacks, OS handlers).
    for s in dol.sections.iter().filter(|s| s.kind == SectionKind::Data) {
        for a in (s.addr..s.end().saturating_sub(3)).step_by(4) {
            if table_words.contains(&a) {
                continue;
            }
            let Some(t) = dol.read_u32(a).filter(|&t| dol.is_code(t)) else { continue };
            let target = insn_at(dol, t).unwrap();
            let after_end = t
                .checked_sub(4)
                .and_then(|p| insn_at(dol, p))
                .map_or(true, |p| ends_function(&p));
            if after_end || is_prologue(&target) {
                seeds.entry(t).or_insert(SeedSource::DataPointer);
            }
        }
    }

    // Function extents: each seed runs to the next seed or section end,
    // minus trailing zero padding.
    let starts: Vec<(u32, SeedSource)> = seeds.into_iter().collect();
    let mut functions = Vec::with_capacity(starts.len());
    for (idx, &(start, source)) in starts.iter().enumerate() {
        let sec_end = dol.section_at(start).unwrap().end();
        let mut end = starts.get(idx + 1).map_or(sec_end, |n| n.0.min(sec_end));
        while end > start + 4 && dol.read_u32(end - 4) == Some(0) {
            end -= 4;
        }
        functions.push(build_function(dol, start, end, source, &jump_tables, &mut stats));
    }

    Program { dol, functions, jump_tables, stats }
}

fn build_function(
    dol: &Dol,
    start: u32,
    end: u32,
    source: SeedSource,
    tables: &BTreeMap<u32, JumpTable>,
    stats: &mut Stats,
) -> Function {
    let inside = |a: u32| a >= start && a < end;
    let mut leaders = BTreeSet::from([start]);
    let mut calls = BTreeSet::new();
    let (mut indirect_calls, mut illegal) = (0, 0);

    for addr in (start..end).step_by(4) {
        let i = insn_at(dol, addr).unwrap();
        if i.op == Op::Illegal {
            illegal += 1;
        }
        if i.lk() && matches!(i.op, Op::B | Op::Bc) {
            calls.extend(i.branch_target());
            continue;
        }
        if i.lk() && i.op == Op::Bcctr {
            indirect_calls += 1;
            continue;
        }
        if i.is_branch() && !(i.op == Op::Bclr && i.lk()) {
            if let Some(t) = i.branch_target().filter(|&t| inside(t)) {
                leaders.insert(t);
            }
            if let Some(jt) = tables.get(&addr) {
                leaders.extend(jt.targets.iter().copied().filter(|&t| inside(t)));
            }
            if addr + 4 < end {
                leaders.insert(addr + 4);
            }
        }
    }

    let leaders: Vec<u32> = leaders.into_iter().collect();
    let mut blocks = Vec::with_capacity(leaders.len());
    for (bi, &bstart) in leaders.iter().enumerate() {
        let bend = leaders.get(bi + 1).copied().unwrap_or(end);
        let last = insn_at(dol, bend - 4).unwrap();
        let fallthrough = || (bend < end).then_some(bend);
        let (exit, succs): (Exit, Vec<u32>) = match last.op {
            Op::B | Op::Bc if !last.lk() => {
                let t = last.branch_target().unwrap();
                let mut s: Vec<u32> = if last.is_unconditional() { vec![] } else { fallthrough().into_iter().collect() };
                if inside(t) {
                    s.push(t);
                    (Exit::Local, s)
                } else {
                    (Exit::TailCall(t), s)
                }
            }
            Op::Bclr if !last.lk() => {
                let s = if last.is_unconditional() { vec![] } else { fallthrough().into_iter().collect() };
                (Exit::Return, s)
            }
            Op::Bcctr if !last.lk() => match tables.get(&last.addr) {
                Some(jt) => (Exit::Switch, jt.targets.iter().copied().filter(|&t| inside(t)).collect()),
                None if ctr_from_load(dol, last.addr) => {
                    stats.indirect_tail_calls += 1;
                    (Exit::IndirectTailCall, vec![])
                }
                None => {
                    stats.unresolved_bctr += 1;
                    (Exit::IndirectJump, vec![])
                }
            },
            Op::Rfi => (Exit::Rfi, vec![]),
            _ => (Exit::Local, fallthrough().into_iter().collect()),
        };
        blocks.push(Block { start: bstart, end: bend, succs, exit });
    }

    Function { start, end, source, blocks, calls, indirect_calls, illegal }
}
