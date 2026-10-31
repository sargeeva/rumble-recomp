//! Instruction semantics, one function per instruction (or family of
//! instructions sharing one shape). The interpreter dispatches to them
//! through [`exec`]; recompiled code calls them directly with constant
//! instruction words, so field extraction and the family's inner `match`
//! fold away and both paths share one definition of every instruction.
//!
//! Each function sees `c.pc` already advanced past the instruction, and
//! branches overwrite it.

use super::*;
use rumble_recomp::ppc::Op::*;

type R = Result<(), Fault>;

/// The `ops!` table maps opcodes to their function and generates [`exec`]
/// (the interpreter's dispatch) and [`handler`] (the function's name, for
/// the code generator).
macro_rules! ops {
    ($($f:ident: $($op:ident)|+;)*) => {
        /// Executes one instruction. Returns Ok(false) if it is not
        /// implemented.
        #[inline]
        pub fn exec(c: &mut Cpu, m: &mut Memory, i: &Insn) -> Result<bool, Fault> {
            match i.op {
                $($($op)|+ => $f(c, m, i)?,)*
                _ => return Ok(false),
            }
            Ok(true)
        }

        /// The name of the function in this module implementing `op`.
        pub fn handler(op: Op) -> Option<&'static str> {
            match op {
                $($($op)|+ => Some(stringify!($f)),)*
                _ => None,
            }
        }
    };
}

ops! {
    addi: Addi;
    addis: Addis;
    addic: Addic | AddicRc;
    subfic: Subfic;
    mulli: Mulli;
    add: Add;
    add_carry: Addc | Adde | Addze | Addme | Subfc | Subfe | Subfze | Subfme;
    subf: Subf;
    neg: Neg;
    mullw: Mullw;
    mulhw: Mulhw;
    mulhwu: Mulhwu;
    divw: Divw;
    divwu: Divwu;
    cmpi: Cmpi;
    cmpli: Cmpli;
    cmp: Cmp;
    cmpl: Cmpl;
    nop: Twi | Tw | Mtsr | Mtsrin | Sync | Isync | Eieio | Dcbf | Dcbi | Dcbst | Dcbt | Dcbtst | Icbi | Tlbie
        | Tlbia | Tlbsync;
    ori: Ori;
    oris: Oris;
    xori: Xori;
    xoris: Xoris;
    andi_rc: AndiRc;
    andis_rc: AndisRc;
    logical: And | Andc | Or | Orc | Xor | Nand | Nor | Eqv;
    slw: Slw;
    srw: Srw;
    sraw: Sraw | Srawi;
    cntlzw: Cntlzw;
    extsb: Extsb;
    extsh: Extsh;
    rlwinm: Rlwinm;
    rlwnm: Rlwnm;
    rlwimi: Rlwimi;
    load: Lwz | Lwzu | Lwzx | Lwzux | Lhz | Lhzu | Lhzx | Lhzux | Lha | Lhau | Lhax | Lhaux | Lbz | Lbzu | Lbzx
        | Lbzux | Lwbrx | Lhbrx | Lwarx;
    store: Stw | Stwu | Stwx | Stwux | Sth | Sthu | Sthx | Sthux | Stb | Stbu | Stbx | Stbux | Stwbrx | Sthbrx
        | StwcxRc;
    lmw: Lmw;
    stmw: Stmw;
    string: Lswi | Lswx | Stswi | Stswx;
    lfs: Lfs | Lfsu | Lfsx | Lfsux;
    lfd: Lfd | Lfdu | Lfdx | Lfdux;
    stfs: Stfs | Stfsu | Stfsx | Stfsux;
    stfd: Stfd | Stfdu | Stfdx | Stfdux;
    stfiwx: Stfiwx;
    psq_l: PsqL | PsqLu | PsqLx | PsqLux;
    psq_st: PsqSt | PsqStu | PsqStx | PsqStux;
    fmr: Fmr;
    fneg: Fneg;
    fabs: Fabs;
    fnabs: Fnabs;
    frsp: Frsp;
    fctiw: Fctiw;
    fsel: Fsel;
    fres: Fres;
    frsqrte: Frsqrte;
    fctiwz: Fctiwz;
    fp_arith: Fadd | Fsub | Fmul | Fdiv | Fadds | Fsubs | Fmuls | Fdivs | Fmadd | Fmsub | Fmadds | Fmsubs | Fnmadd
        | Fnmsub | Fnmadds | Fnmsubs;
    fcmp: Fcmpu | Fcmpo;
    ps_mr: PsMr;
    ps_neg: PsNeg;
    ps_abs: PsAbs;
    ps_nabs: PsNabs;
    ps_merge: PsMerge00 | PsMerge01 | PsMerge10 | PsMerge11;
    ps_arith: PsAdd | PsSub | PsMul | PsDiv | PsMadd | PsMsub | PsNmadd | PsNmsub | PsMuls0 | PsMuls1 | PsMadds0
        | PsMadds1 | PsSel | PsRes | PsRsqrte;
    ps_sum0: PsSum0;
    ps_sum1: PsSum1;
    ps_cmp: PsCmpu0 | PsCmpo0 | PsCmpu1 | PsCmpo1;
    mffs: Mffs;
    mcrfs: Mcrfs;
    mtfsf: Mtfsf;
    mtfsb0: Mtfsb0;
    mtfsb1: Mtfsb1;
    mtfsfi: Mtfsfi;
    b: B;
    bc: Bc;
    bclr: Bclr;
    bcctr: Bcctr;
    rfi: Rfi;
    sc: Sc;
    mcrf: Mcrf;
    cr_logic: Crand | Crandc | Creqv | Crnand | Crnor | Cror | Crorc | Crxor;
    mfcr: Mfcr;
    mtcrf: Mtcrf;
    mcrxr: Mcrxr;
    mfmsr: Mfmsr;
    mtmsr: Mtmsr;
    mfspr: Mfspr;
    mtspr: Mtspr;
    mftb: Mftb;
    mfsr: Mfsr | Mfsrin;
    dcbz: Dcbz | DcbzL;
}

#[inline(always)]
fn rd(i: &Insn) -> usize { i.rd() as usize }
#[inline(always)]
fn ra(i: &Insn) -> usize { i.ra() as usize }
#[inline(always)]
fn rb(i: &Insn) -> usize { i.rb() as usize }
#[inline(always)]
fn frc(i: &Insn) -> usize { i.rc_reg() as usize }

/// rA, or 0 when the field is r0.
#[inline(always)]
fn ra0(c: &Cpu, i: &Insn) -> u32 {
    if i.ra() == 0 { 0 } else { c.gpr[ra(i)] }
}

/// Writes rD, and CR0 when Rc is set.
#[inline(always)]
fn set_rd(c: &mut Cpu, i: &Insn, v: u32) {
    c.gpr[rd(i)] = v;
    if i.rc() { c.record(v) }
}

/// Writes rA (logical ops keep rS in the rD slot), and CR0 when Rc is set.
#[inline(always)]
fn set_ra(c: &mut Cpu, i: &Insn, v: u32) {
    c.gpr[ra(i)] = v;
    if i.rc() { c.record(v) }
}

/// CR field for an FP compare: lt, gt, eq or unordered.
#[inline(always)]
fn fp_flags(x: f64, y: f64) -> u32 {
    if x.is_nan() || y.is_nan() { 1 } else if x < y { 8 } else if x > y { 4 } else { 2 }
}

/// FPSCR or CR field mask from an 8-bit field selector.
#[inline(always)]
fn field_mask(fm: u32) -> u32 {
    let mut mk = 0u32;
    for k in 0..8 { if fm & (0x80 >> k) != 0 { mk |= 0xf000_0000 >> (4 * k) } }
    mk
}

// --- integer arithmetic ---

#[inline(always)]
pub fn addi(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.gpr[rd(i)] = ra0(c, i).wrapping_add(i.simm() as u32);
    Ok(())
}

#[inline(always)]
pub fn addis(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.gpr[rd(i)] = ra0(c, i).wrapping_add(i.uimm() << 16);
    Ok(())
}

#[inline(always)]
pub fn addic(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (v, ca) = c.gpr[ra(i)].overflowing_add(i.simm() as u32);
    c.gpr[rd(i)] = v;
    c.set_ca(ca);
    if i.op == AddicRc { c.record(v) }
    Ok(())
}

#[inline(always)]
pub fn subfic(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let v = !c.gpr[ra(i)] as u64 + i.simm() as u32 as u64 + 1;
    c.gpr[rd(i)] = v as u32;
    c.set_ca(v >> 32 != 0);
    Ok(())
}

#[inline(always)]
pub fn mulli(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.gpr[rd(i)] = (c.gpr[ra(i)] as i32).wrapping_mul(i.simm()) as u32;
    Ok(())
}

#[inline(always)]
pub fn add(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    set_rd(c, i, c.gpr[ra(i)].wrapping_add(c.gpr[rb(i)]));
    Ok(())
}

/// Adds and subtracts through the carry: rA (inverted for the subf forms)
/// plus rB, 0 or -1, plus a carry in of CA or 1.
#[inline(always)]
pub fn add_carry(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (a, b) = (c.gpr[ra(i)], c.gpr[rb(i)]);
    let (x, y, cin) = match i.op {
        Addc => (a, b, 0),
        Adde => (a, b, c.ca()),
        Addze => (a, 0, c.ca()),
        Addme => (a, u32::MAX, c.ca()),
        Subfc => (!a, b, 1),
        Subfe => (!a, b, c.ca()),
        Subfze => (!a, 0, c.ca()),
        _ => (!a, u32::MAX, c.ca()),
    };
    let v = x as u64 + y as u64 + cin as u64;
    c.set_ca(v >> 32 != 0);
    set_rd(c, i, v as u32);
    Ok(())
}

#[inline(always)]
pub fn subf(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    set_rd(c, i, c.gpr[rb(i)].wrapping_sub(c.gpr[ra(i)]));
    Ok(())
}

#[inline(always)]
pub fn neg(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    set_rd(c, i, c.gpr[ra(i)].wrapping_neg());
    Ok(())
}

#[inline(always)]
pub fn mullw(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    set_rd(c, i, (c.gpr[ra(i)] as i32).wrapping_mul(c.gpr[rb(i)] as i32) as u32);
    Ok(())
}

#[inline(always)]
pub fn mulhw(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    set_rd(c, i, ((c.gpr[ra(i)] as i32 as i64 * c.gpr[rb(i)] as i32 as i64) >> 32) as u32);
    Ok(())
}

#[inline(always)]
pub fn mulhwu(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    set_rd(c, i, ((c.gpr[ra(i)] as u64 * c.gpr[rb(i)] as u64) >> 32) as u32);
    Ok(())
}

#[inline(always)]
pub fn divw(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (a, b) = (c.gpr[ra(i)], c.gpr[rb(i)]);
    let v = if b == 0 || (a == 0x8000_0000 && b == u32::MAX) {
        if (a as i32) < 0 { u32::MAX } else { 0 }
    } else {
        (a as i32 / b as i32) as u32
    };
    set_rd(c, i, v);
    Ok(())
}

#[inline(always)]
pub fn divwu(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (a, b) = (c.gpr[ra(i)], c.gpr[rb(i)]);
    set_rd(c, i, if b == 0 { 0 } else { a / b });
    Ok(())
}

// --- compare ---

#[inline(always)]
pub fn cmpi(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let a = c.gpr[ra(i)] as i32;
    let f = c.cmp_flags(a < i.simm(), a > i.simm());
    c.set_cr_field(i.crfd(), f);
    Ok(())
}

#[inline(always)]
pub fn cmpli(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let a = c.gpr[ra(i)];
    let f = c.cmp_flags(a < i.uimm(), a > i.uimm());
    c.set_cr_field(i.crfd(), f);
    Ok(())
}

#[inline(always)]
pub fn cmp(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (a, b) = (c.gpr[ra(i)] as i32, c.gpr[rb(i)] as i32);
    let f = c.cmp_flags(a < b, a > b);
    c.set_cr_field(i.crfd(), f);
    Ok(())
}

#[inline(always)]
pub fn cmpl(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (a, b) = (c.gpr[ra(i)], c.gpr[rb(i)]);
    let f = c.cmp_flags(a < b, a > b);
    c.set_cr_field(i.crfd(), f);
    Ok(())
}

/// Traps, segment registers, and cache and sync ops that mean nothing on a
/// coherent host.
#[inline(always)]
pub fn nop(_: &mut Cpu, _: &mut Memory, _: &Insn) -> R {
    Ok(())
}

// --- logical / shift / rotate (rS is in the rD slot) ---

#[inline(always)]
pub fn ori(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.gpr[ra(i)] = c.gpr[rd(i)] | i.uimm();
    Ok(())
}

#[inline(always)]
pub fn oris(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.gpr[ra(i)] = c.gpr[rd(i)] | (i.uimm() << 16);
    Ok(())
}

#[inline(always)]
pub fn xori(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.gpr[ra(i)] = c.gpr[rd(i)] ^ i.uimm();
    Ok(())
}

#[inline(always)]
pub fn xoris(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.gpr[ra(i)] = c.gpr[rd(i)] ^ (i.uimm() << 16);
    Ok(())
}

#[inline(always)]
pub fn andi_rc(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let v = c.gpr[rd(i)] & i.uimm();
    c.gpr[ra(i)] = v;
    c.record(v);
    Ok(())
}

#[inline(always)]
pub fn andis_rc(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let v = c.gpr[rd(i)] & (i.uimm() << 16);
    c.gpr[ra(i)] = v;
    c.record(v);
    Ok(())
}

#[inline(always)]
pub fn logical(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (s, b) = (c.gpr[rd(i)], c.gpr[rb(i)]);
    let v = match i.op {
        And => s & b,
        Andc => s & !b,
        Or => s | b,
        Orc => s | !b,
        Xor => s ^ b,
        Nand => !(s & b),
        Nor => !(s | b),
        _ => !(s ^ b),
    };
    set_ra(c, i, v);
    Ok(())
}

#[inline(always)]
pub fn slw(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (s, b) = (c.gpr[rd(i)], c.gpr[rb(i)]);
    set_ra(c, i, if b & 0x20 != 0 { 0 } else { s << (b & 31) });
    Ok(())
}

#[inline(always)]
pub fn srw(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (s, b) = (c.gpr[rd(i)], c.gpr[rb(i)]);
    set_ra(c, i, if b & 0x20 != 0 { 0 } else { s >> (b & 31) });
    Ok(())
}

/// Arithmetic shift right; CA is set when a negative value loses one bits.
#[inline(always)]
pub fn sraw(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let sh = if i.op == Srawi { i.sh() } else { c.gpr[rb(i)] & 63 };
    let s = c.gpr[rd(i)] as i32;
    let v = if sh > 31 { s >> 31 } else { s >> sh };
    let lost = if sh > 31 { s != 0 } else { (s as u32) & ((1u32 << sh) - 1) != 0 };
    c.set_ca(s < 0 && lost);
    set_ra(c, i, v as u32);
    Ok(())
}

#[inline(always)]
pub fn cntlzw(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    set_ra(c, i, c.gpr[rd(i)].leading_zeros());
    Ok(())
}

#[inline(always)]
pub fn extsb(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    set_ra(c, i, c.gpr[rd(i)] as u8 as i8 as i32 as u32);
    Ok(())
}

#[inline(always)]
pub fn extsh(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    set_ra(c, i, c.gpr[rd(i)] as u16 as i16 as i32 as u32);
    Ok(())
}

#[inline(always)]
pub fn rlwinm(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    set_ra(c, i, c.gpr[rd(i)].rotate_left(i.sh()) & mask(i.mb(), i.me()));
    Ok(())
}

#[inline(always)]
pub fn rlwnm(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    set_ra(c, i, c.gpr[rd(i)].rotate_left(c.gpr[rb(i)] & 31) & mask(i.mb(), i.me()));
    Ok(())
}

#[inline(always)]
pub fn rlwimi(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let mk = mask(i.mb(), i.me());
    set_ra(c, i, (c.gpr[rd(i)].rotate_left(i.sh()) & mk) | (c.gpr[ra(i)] & !mk));
    Ok(())
}

// --- integer loads / stores ---

#[inline(always)]
pub fn load(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    let x = matches!(i.op, Lwzx | Lwzux | Lhzx | Lhzux | Lhax | Lhaux | Lbzx | Lbzux | Lwbrx | Lhbrx | Lwarx);
    let ea = if x { c.ea_x(i) } else { c.ea_d(i) };
    c.gpr[rd(i)] = match i.op {
        Lwz | Lwzu | Lwzx | Lwzux | Lwarx => m.r32(ea)?,
        Lhz | Lhzu | Lhzx | Lhzux => m.r16(ea)? as u32,
        Lha | Lhau | Lhax | Lhaux => m.r16(ea)? as i16 as i32 as u32,
        Lwbrx => m.r32(ea)?.swap_bytes(),
        Lhbrx => m.r16(ea)?.swap_bytes() as u32,
        _ => m.r8(ea)? as u32,
    };
    if matches!(i.op, Lwzu | Lwzux | Lhzu | Lhzux | Lhau | Lhaux | Lbzu | Lbzux) {
        c.gpr[ra(i)] = ea;
    }
    Ok(())
}

#[inline(always)]
pub fn store(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    let x = matches!(i.op, Stwx | Stwux | Sthx | Sthux | Stbx | Stbux | Stwbrx | Sthbrx | StwcxRc);
    let ea = if x { c.ea_x(i) } else { c.ea_d(i) };
    let s = c.gpr[rd(i)];
    match i.op {
        Stw | Stwu | Stwx | Stwux | StwcxRc => m.w32(ea, s)?,
        Sth | Sthu | Sthx | Sthux => m.w16(ea, s as u16)?,
        Stwbrx => m.w32(ea, s.swap_bytes())?,
        Sthbrx => m.w16(ea, (s as u16).swap_bytes())?,
        _ => m.w8(ea, s as u8)?,
    }
    if matches!(i.op, Stwu | Stwux | Sthu | Sthux | Stbu | Stbux) {
        c.gpr[ra(i)] = ea;
    }
    if i.op == StwcxRc {
        // Single core: reservations always hold.
        let f = c.cmp_flags(false, false) | 0b0010;
        c.set_cr_field(0, f);
    }
    Ok(())
}

#[inline(always)]
pub fn lmw(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    let mut ea = c.ea_d(i);
    for r in rd(i)..32 {
        c.gpr[r] = m.r32(ea)?;
        ea = ea.wrapping_add(4);
    }
    Ok(())
}

#[inline(always)]
pub fn stmw(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    let mut ea = c.ea_d(i);
    for r in rd(i)..32 {
        m.w32(ea, c.gpr[r])?;
        ea = ea.wrapping_add(4);
    }
    Ok(())
}

/// String loads/stores: bytes packed big-endian into consecutive registers
/// from rD, wrapping from r31 to r0. The immediate forms take the count from
/// NB (0 = 32), the indexed forms from XER[25:31]; the immediate forms'
/// address is rA or 0.
#[inline(always)]
pub fn string(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    let (mut ea, n) = if matches!(i.op, Lswi | Stswi) {
        let nb = i.rb();
        (ra0(c, i), if nb == 0 { 32 } else { nb })
    } else {
        (c.ea_x(i), c.xer() & 0x7f)
    };
    let load = matches!(i.op, Lswi | Lswx);
    let mut r = rd(i);
    for k in 0..n {
        let shift = 24 - 8 * (k % 4);
        if load {
            if k % 4 == 0 {
                c.gpr[r] = 0;
            }
            c.gpr[r] |= (m.r8(ea)? as u32) << shift;
        } else {
            m.w8(ea, (c.gpr[r] >> shift) as u8)?;
        }
        ea = ea.wrapping_add(1);
        if k % 4 == 3 {
            r = (r + 1) % 32;
        }
    }
    Ok(())
}

// --- FP loads / stores ---

#[inline(always)]
pub fn lfs(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    let ea = if matches!(i.op, Lfsx | Lfsux) { c.ea_x(i) } else { c.ea_d(i) };
    let v = f32::from_bits(m.r32(ea)?) as f64;
    c.fpr[rd(i)] = [v, v];
    if matches!(i.op, Lfsu | Lfsux) { c.gpr[ra(i)] = ea }
    Ok(())
}

#[inline(always)]
pub fn lfd(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    let ea = if matches!(i.op, Lfdx | Lfdux) { c.ea_x(i) } else { c.ea_d(i) };
    c.fpr[rd(i)][0] = f64::from_bits(m.r64(ea)?);
    if matches!(i.op, Lfdu | Lfdux) { c.gpr[ra(i)] = ea }
    Ok(())
}

#[inline(always)]
pub fn stfs(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    let ea = if matches!(i.op, Stfsx | Stfsux) { c.ea_x(i) } else { c.ea_d(i) };
    m.w32(ea, (c.fpr[rd(i)][0] as f32).to_bits())?;
    if matches!(i.op, Stfsu | Stfsux) { c.gpr[ra(i)] = ea }
    Ok(())
}

#[inline(always)]
pub fn stfd(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    let ea = if matches!(i.op, Stfdx | Stfdux) { c.ea_x(i) } else { c.ea_d(i) };
    m.w64(ea, c.fpr[rd(i)][0].to_bits())?;
    if matches!(i.op, Stfdu | Stfdux) { c.gpr[ra(i)] = ea }
    Ok(())
}

#[inline(always)]
pub fn stfiwx(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    m.w32(c.ea_x(i), c.fpr[rd(i)][0].to_bits() as u32)
}

/// Quantized load: one or two values of the GQR's load type, scaled, into
/// ps0/ps1 (ps1 = 1.0 when W is set).
#[inline(always)]
pub fn psq_l(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    let (w, q, ea) = match i.op {
        PsqL | PsqLu => { let (w, q, disp) = i.psq_d(); (w, q, ra0(c, i).wrapping_add(disp as u32)) }
        _ => { let (w, q) = i.psq_x(); (w, q, c.ea_x(i)) }
    };
    let gqr = c.spr[SPR_GQR0 + q as usize];
    let (ty, scale) = ((gqr >> 16) & 7, qscale(gqr >> 24));
    let (size, ld): (u32, fn(&mut Memory, u32) -> Result<f64, Fault>) = match ty {
        4 => (1, |m, a| Ok(m.r8(a)? as f64)),
        5 => (2, |m, a| Ok(m.r16(a)? as f64)),
        6 => (1, |m, a| Ok(m.r8(a)? as i8 as f64)),
        7 => (2, |m, a| Ok(m.r16(a)? as i16 as f64)),
        _ => (4, |m, a| Ok(f32::from_bits(m.r32(a)?) as f64)),
    };
    let s = if ty >= 4 { scale } else { 1.0 };
    let p0 = ld(m, ea)? * s;
    let p1 = if w == 1 { 1.0 } else { ld(m, ea.wrapping_add(size))? * s };
    c.fpr[rd(i)] = [p0, p1];
    if matches!(i.op, PsqLu | PsqLux) { c.gpr[ra(i)] = ea }
    Ok(())
}

/// Quantized store: ps0 (and ps1 unless W is set), scaled and clamped to
/// the GQR's store type.
#[inline(always)]
pub fn psq_st(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    let (w, q, ea) = match i.op {
        PsqSt | PsqStu => { let (w, q, disp) = i.psq_d(); (w, q, ra0(c, i).wrapping_add(disp as u32)) }
        _ => { let (w, q) = i.psq_x(); (w, q, c.ea_x(i)) }
    };
    let gqr = c.spr[SPR_GQR0 + q as usize];
    let (ty, scale) = (gqr & 7, 1.0 / qscale(gqr >> 8));
    let pair = c.fpr[rd(i)];
    let vals = if w == 1 { &pair[..1] } else { &pair[..] };
    let mut a2 = ea;
    for &v in vals {
        match ty {
            4 => { m.w8(a2, (v * scale).clamp(0.0, 255.0) as u8)?; a2 += 1 }
            5 => { m.w16(a2, (v * scale).clamp(0.0, 65535.0) as u16)?; a2 += 2 }
            6 => { m.w8(a2, (v * scale).clamp(-128.0, 127.0) as i8 as u8)?; a2 += 1 }
            7 => { m.w16(a2, (v * scale).clamp(-32768.0, 32767.0) as i16 as u16)?; a2 += 2 }
            _ => { m.w32(a2, (v as f32).to_bits())?; a2 += 4 }
        }
    }
    if matches!(i.op, PsqStu | PsqStux) { c.gpr[ra(i)] = ea }
    Ok(())
}

// --- FP arithmetic ---

#[inline(always)]
pub fn fmr(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.fpr[rd(i)][0] = c.fpr[rb(i)][0];
    Ok(())
}

#[inline(always)]
pub fn fneg(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.fpr[rd(i)][0] = -c.fpr[rb(i)][0];
    Ok(())
}

#[inline(always)]
pub fn fabs(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.fpr[rd(i)][0] = c.fpr[rb(i)][0].abs();
    Ok(())
}

#[inline(always)]
pub fn fnabs(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.fpr[rd(i)][0] = -c.fpr[rb(i)][0].abs();
    Ok(())
}

#[inline(always)]
pub fn frsp(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let v = c.fpr[rb(i)][0] as f32 as f64;
    c.fpr[rd(i)] = [v, v];
    Ok(())
}

/// Round per FPSCR[RN]: nearest-even, toward zero, +inf, -inf.
#[inline(always)]
pub fn fctiw(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let v = c.fpr[rb(i)][0];
    let r = match c.fpscr & 3 {
        0 => v.round_ties_even(),
        1 => v.trunc(),
        2 => v.ceil(),
        _ => v.floor(),
    };
    c.fpr[rd(i)][0] = f64::from_bits(r.clamp(i32::MIN as f64, i32::MAX as f64) as i32 as u32 as u64);
    Ok(())
}

#[inline(always)]
pub fn fsel(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (fa, fb, fc) = (c.fpr[ra(i)][0], c.fpr[rb(i)][0], c.fpr[frc(i)][0]);
    c.fpr[rd(i)][0] = if fa >= 0.0 { fc } else { fb };
    Ok(())
}

/// Estimates: exact values are within the architected precision.
#[inline(always)]
pub fn fres(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let v = (1.0 / c.fpr[rb(i)][0]) as f32 as f64;
    c.fpr[rd(i)] = [v, v];
    Ok(())
}

#[inline(always)]
pub fn frsqrte(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.fpr[rd(i)][0] = 1.0 / c.fpr[rb(i)][0].sqrt();
    Ok(())
}

#[inline(always)]
pub fn fctiwz(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.fpr[rd(i)][0] = f64::from_bits(c.fpr[rb(i)][0].clamp(i32::MIN as f64, i32::MAX as f64) as i32 as u32 as u64);
    Ok(())
}

/// Scalar add/sub/mul/div and fused multiply-adds; the single-precision
/// forms round to f32 and fill both halves.
#[inline(always)]
pub fn fp_arith(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (fa, fb, fc) = (c.fpr[ra(i)][0], c.fpr[rb(i)][0], c.fpr[frc(i)][0]);
    let v = match i.op {
        Fadd | Fadds => fa + fb,
        Fsub | Fsubs => fa - fb,
        Fmul | Fmuls => fa * fc,
        Fdiv | Fdivs => fa / fb,
        Fmadd | Fmadds => fa * fc + fb,
        Fmsub | Fmsubs => fa * fc - fb,
        Fnmadd | Fnmadds => -(fa * fc + fb),
        _ => -(fa * fc - fb),
    };
    if matches!(i.op, Fadds | Fsubs | Fmuls | Fdivs | Fmadds | Fmsubs | Fnmadds | Fnmsubs) {
        let s = v as f32 as f64;
        c.fpr[rd(i)] = [s, s];
    } else {
        c.fpr[rd(i)][0] = v;
    }
    Ok(())
}

#[inline(always)]
pub fn fcmp(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let f = fp_flags(c.fpr[ra(i)][0], c.fpr[rb(i)][0]);
    c.set_cr_field(i.crfd(), f);
    Ok(())
}

// --- paired singles ---

#[inline(always)]
pub fn ps_mr(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.fpr[rd(i)] = c.fpr[rb(i)];
    Ok(())
}

#[inline(always)]
pub fn ps_neg(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.fpr[rd(i)] = c.fpr[rb(i)].map(|v| -v);
    Ok(())
}

#[inline(always)]
pub fn ps_abs(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.fpr[rd(i)] = c.fpr[rb(i)].map(f64::abs);
    Ok(())
}

#[inline(always)]
pub fn ps_nabs(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.fpr[rd(i)] = c.fpr[rb(i)].map(|v| -v.abs());
    Ok(())
}

#[inline(always)]
pub fn ps_merge(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (fa, fb) = (c.fpr[ra(i)], c.fpr[rb(i)]);
    c.fpr[rd(i)] = match i.op {
        PsMerge00 => [fa[0], fb[0]],
        PsMerge01 => [fa[0], fb[1]],
        PsMerge10 => [fa[1], fb[0]],
        _ => [fa[1], fb[1]],
    };
    Ok(())
}

/// Element-wise paired-single arithmetic, rounded to f32.
#[inline(always)]
pub fn ps_arith(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (fa, fb, fc) = (c.fpr[ra(i)], c.fpr[rb(i)], c.fpr[frc(i)]);
    let f = |k: usize| -> f64 {
        (match i.op {
            PsAdd => fa[k] + fb[k],
            PsSub => fa[k] - fb[k],
            PsMul => fa[k] * fc[k],
            PsDiv => fa[k] / fb[k],
            PsMadd => fa[k] * fc[k] + fb[k],
            PsMsub => fa[k] * fc[k] - fb[k],
            PsNmadd => -(fa[k] * fc[k] + fb[k]),
            PsNmsub => -(fa[k] * fc[k] - fb[k]),
            // The scalar forms take one slot of frC for both halves.
            PsMuls0 => fa[k] * fc[0],
            PsMuls1 => fa[k] * fc[1],
            PsMadds0 => fa[k] * fc[0] + fb[k],
            PsMadds1 => fa[k] * fc[1] + fb[k],
            PsSel => if fa[k] >= 0.0 { fc[k] } else { fb[k] },
            PsRes => 1.0 / fb[k],
            _ => 1.0 / fb[k].sqrt(),
        }) as f32 as f64
    };
    c.fpr[rd(i)] = [f(0), f(1)];
    Ok(())
}

/// Cross sums: one half is fa.ps0 + fb.ps1, the other comes from frC.
#[inline(always)]
pub fn ps_sum0(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let fc = c.fpr[frc(i)];
    c.fpr[rd(i)] = [(c.fpr[ra(i)][0] + c.fpr[rb(i)][1]) as f32 as f64, fc[1]];
    Ok(())
}

#[inline(always)]
pub fn ps_sum1(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let fc = c.fpr[frc(i)];
    c.fpr[rd(i)] = [fc[0], (c.fpr[ra(i)][0] + c.fpr[rb(i)][1]) as f32 as f64];
    Ok(())
}

#[inline(always)]
pub fn ps_cmp(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let k = matches!(i.op, PsCmpu1 | PsCmpo1) as usize;
    let f = fp_flags(c.fpr[ra(i)][k], c.fpr[rb(i)][k]);
    c.set_cr_field(i.crfd(), f);
    Ok(())
}

// --- FPSCR ---

#[inline(always)]
pub fn mffs(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.fpr[rd(i)][0] = f64::from_bits(c.fpscr as u64);
    Ok(())
}

/// Copy an FPSCR field to CR; exception bits in it are cleared.
#[inline(always)]
pub fn mcrfs(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let sh = 28 - 4 * i.crfs();
    let v = (c.fpscr >> sh) & 0xf;
    c.set_cr_field(i.crfd(), v);
    c.fpscr &= !((0xf << sh) & 0x9ff8_0700);
    Ok(())
}

#[inline(always)]
pub fn mtfsf(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let mk = field_mask((i.word >> 17) & 0xff);
    c.fpscr = (c.fpscr & !mk) | (c.fpr[rb(i)][0].to_bits() as u32 & mk);
    Ok(())
}

#[inline(always)]
pub fn mtfsb0(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.fpscr &= !(1 << (31 - i.rd()));
    Ok(())
}

#[inline(always)]
pub fn mtfsb1(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.fpscr |= 1 << (31 - i.rd());
    Ok(())
}

#[inline(always)]
pub fn mtfsfi(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let sh = 28 - 4 * i.crfd();
    c.fpscr = (c.fpscr & !(0xf << sh)) | (((i.word >> 12) & 0xf) << sh);
    Ok(())
}

// --- branches ---

#[inline(always)]
pub fn b(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    if i.lk() { c.spr[SPR_LR] = c.pc }
    c.pc = i.branch_target().unwrap();
    Ok(())
}

#[inline(always)]
pub fn bc(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let taken = c.branch_cond(i, true);
    if i.lk() { c.spr[SPR_LR] = c.pc }
    if taken { c.pc = i.branch_target().unwrap() }
    Ok(())
}

#[inline(always)]
pub fn bclr(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let target = c.spr[SPR_LR] & !3;
    let taken = c.branch_cond(i, true);
    if i.lk() { c.spr[SPR_LR] = c.pc }
    if taken { c.pc = target }
    Ok(())
}

#[inline(always)]
pub fn bcctr(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let taken = c.branch_cond(i, false);
    if i.lk() { c.spr[SPR_LR] = c.pc }
    if taken { c.pc = c.spr[SPR_CTR] & !3 }
    Ok(())
}

#[inline(always)]
pub fn rfi(c: &mut Cpu, _: &mut Memory, _: &Insn) -> R {
    c.msr = c.spr[SPR_SRR1];
    c.pc = c.spr[SPR_SRR0] & !3;
    Ok(())
}

#[inline(always)]
pub fn sc(c: &mut Cpu, _: &mut Memory, _: &Insn) -> R {
    c.exception(0xc00);
    Ok(())
}

// --- condition register ---

#[inline(always)]
pub fn mcrf(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let v = (c.cr >> (28 - 4 * i.crfs())) & 0xf;
    c.set_cr_field(i.crfd(), v);
    Ok(())
}

#[inline(always)]
pub fn cr_logic(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let (x, y) = (c.cr_bit(i.ra()), c.cr_bit(i.rb()));
    let v = match i.op {
        Crand => x & y,
        Crandc => x & !y,
        Creqv => x == y,
        Crnand => !(x & y),
        Crnor => !(x | y),
        Cror => x | y,
        Crorc => x | !y,
        _ => x ^ y,
    };
    c.set_cr_bit(i.rd(), v);
    Ok(())
}

#[inline(always)]
pub fn mfcr(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.gpr[rd(i)] = c.cr;
    Ok(())
}

#[inline(always)]
pub fn mtcrf(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    let mk = field_mask(i.crm());
    c.cr = (c.cr & !mk) | (c.gpr[rd(i)] & mk);
    Ok(())
}

#[inline(always)]
pub fn mcrxr(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.set_cr_field(i.crfd(), c.xer() >> 28);
    c.spr[SPR_XER] &= 0x0fff_ffff;
    Ok(())
}

// --- system registers ---

#[inline(always)]
pub fn mfmsr(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.gpr[rd(i)] = c.msr;
    Ok(())
}

#[inline(always)]
pub fn mtmsr(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.msr = c.gpr[rd(i)];
    Ok(())
}

#[inline(always)]
pub fn mfspr(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.gpr[rd(i)] = match i.spr() as usize {
        SPR_DEC => c.dec(),
        n => c.spr[n],
    };
    Ok(())
}

/// Writes to DEC rearm the decrementer, and to DMA_L start locked-cache
/// DMA.
#[inline(always)]
pub fn mtspr(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    let v = c.gpr[rd(i)];
    match i.spr() as usize {
        SPR_DEC => c.set_dec(v),
        SPR_DMA_L => c.locked_cache_dma(m, v)?,
        n => c.spr[n] = v,
    }
    Ok(())
}

#[inline(always)]
pub fn mftb(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.gpr[rd(i)] = if i.spr() == 269 { (c.tb >> 32) as u32 } else { c.tb as u32 };
    Ok(())
}

/// Segment registers aren't used with the SDK's fixed mapping.
#[inline(always)]
pub fn mfsr(c: &mut Cpu, _: &mut Memory, i: &Insn) -> R {
    c.gpr[rd(i)] = 0;
    Ok(())
}

/// Zeroes the 32-byte cache line (dcbz_l does the same in the locked cache).
#[inline(always)]
pub fn dcbz(c: &mut Cpu, m: &mut Memory, i: &Insn) -> R {
    let ea = c.ea_x(i) & !31;
    for k in (0..32).step_by(8) { m.w64(ea + k, 0)? }
    Ok(())
}
