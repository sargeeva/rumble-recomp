//! Gekko/Broadway (PowerPC 750CL) instruction decoding and disassembly,
//! including the paired-single extension.
//!
//! Decoding yields an [`Op`] plus the raw word; operand fields are read
//! through accessors on [`Insn`] so the code generator and disassembler
//! share one source of truth.

macro_rules! ops {
    ($($v:ident => $m:literal),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum Op { $($v),* }
        impl Op {
            pub fn mnemonic(self) -> &'static str {
                match self { $(Op::$v => $m),* }
            }
        }
    };
}

ops! {
    Illegal => "illegal",
    // Integer arithmetic
    Addi => "addi", Addis => "addis", Addic => "addic", AddicRc => "addic.", Subfic => "subfic", Mulli => "mulli",
    Add => "add", Addc => "addc", Adde => "adde", Addze => "addze", Addme => "addme",
    Subf => "subf", Subfc => "subfc", Subfe => "subfe", Subfze => "subfze", Subfme => "subfme", Neg => "neg",
    Mullw => "mullw", Mulhw => "mulhw", Mulhwu => "mulhwu", Divw => "divw", Divwu => "divwu",
    // Compare / trap
    Cmpi => "cmpwi", Cmpli => "cmplwi", Cmp => "cmpw", Cmpl => "cmplw", Twi => "twi", Tw => "tw",
    // Logical / shift / rotate
    Ori => "ori", Oris => "oris", Xori => "xori", Xoris => "xoris", AndiRc => "andi.", AndisRc => "andis.",
    And => "and", Andc => "andc", Or => "or", Orc => "orc", Xor => "xor", Nand => "nand", Nor => "nor", Eqv => "eqv",
    Slw => "slw", Srw => "srw", Sraw => "sraw", Srawi => "srawi",
    Cntlzw => "cntlzw", Extsb => "extsb", Extsh => "extsh",
    Rlwinm => "rlwinm", Rlwimi => "rlwimi", Rlwnm => "rlwnm",
    // Integer loads/stores
    Lwz => "lwz", Lwzu => "lwzu", Lwzx => "lwzx", Lwzux => "lwzux",
    Lhz => "lhz", Lhzu => "lhzu", Lhzx => "lhzx", Lhzux => "lhzux",
    Lha => "lha", Lhau => "lhau", Lhax => "lhax", Lhaux => "lhaux",
    Lbz => "lbz", Lbzu => "lbzu", Lbzx => "lbzx", Lbzux => "lbzux",
    Stw => "stw", Stwu => "stwu", Stwx => "stwx", Stwux => "stwux",
    Sth => "sth", Sthu => "sthu", Sthx => "sthx", Sthux => "sthux",
    Stb => "stb", Stbu => "stbu", Stbx => "stbx", Stbux => "stbux",
    Lwbrx => "lwbrx", Lhbrx => "lhbrx", Stwbrx => "stwbrx", Sthbrx => "sthbrx",
    Lmw => "lmw", Stmw => "stmw", Lswi => "lswi", Lswx => "lswx", Stswi => "stswi", Stswx => "stswx",
    Lwarx => "lwarx", StwcxRc => "stwcx.", Eciwx => "eciwx", Ecowx => "ecowx",
    // FP loads/stores
    Lfs => "lfs", Lfsu => "lfsu", Lfsx => "lfsx", Lfsux => "lfsux",
    Lfd => "lfd", Lfdu => "lfdu", Lfdx => "lfdx", Lfdux => "lfdux",
    Stfs => "stfs", Stfsu => "stfsu", Stfsx => "stfsx", Stfsux => "stfsux",
    Stfd => "stfd", Stfdu => "stfdu", Stfdx => "stfdx", Stfdux => "stfdux", Stfiwx => "stfiwx",
    // Branches / system
    B => "b", Bc => "bc", Bclr => "bclr", Bcctr => "bcctr", Sc => "sc", Rfi => "rfi",
    Mcrf => "mcrf", Crand => "crand", Crandc => "crandc", Creqv => "creqv", Crnand => "crnand",
    Crnor => "crnor", Cror => "cror", Crorc => "crorc", Crxor => "crxor",
    Mfcr => "mfcr", Mtcrf => "mtcrf", Mcrxr => "mcrxr", Mfmsr => "mfmsr", Mtmsr => "mtmsr",
    Mfspr => "mfspr", Mtspr => "mtspr", Mftb => "mftb", Mfsr => "mfsr", Mtsr => "mtsr",
    Mfsrin => "mfsrin", Mtsrin => "mtsrin",
    Sync => "sync", Isync => "isync", Eieio => "eieio", Tlbie => "tlbie", Tlbia => "tlbia", Tlbsync => "tlbsync",
    Dcbf => "dcbf", Dcbi => "dcbi", Dcbst => "dcbst", Dcbt => "dcbt", Dcbtst => "dcbtst", Dcbz => "dcbz",
    DcbzL => "dcbz_l", Icbi => "icbi",
    // Floating point (double)
    Fadd => "fadd", Fsub => "fsub", Fmul => "fmul", Fdiv => "fdiv", Fsel => "fsel", Frsqrte => "frsqrte",
    Fmadd => "fmadd", Fmsub => "fmsub", Fnmadd => "fnmadd", Fnmsub => "fnmsub",
    Fcmpu => "fcmpu", Fcmpo => "fcmpo", Frsp => "frsp", Fctiw => "fctiw", Fctiwz => "fctiwz",
    Fmr => "fmr", Fneg => "fneg", Fabs => "fabs", Fnabs => "fnabs",
    Mffs => "mffs", Mtfsf => "mtfsf", Mtfsfi => "mtfsfi", Mtfsb0 => "mtfsb0", Mtfsb1 => "mtfsb1", Mcrfs => "mcrfs",
    // Floating point (single)
    Fadds => "fadds", Fsubs => "fsubs", Fmuls => "fmuls", Fdivs => "fdivs", Fres => "fres",
    Fmadds => "fmadds", Fmsubs => "fmsubs", Fnmadds => "fnmadds", Fnmsubs => "fnmsubs",
    // Paired singles
    PsqL => "psq_l", PsqLu => "psq_lu", PsqSt => "psq_st", PsqStu => "psq_stu",
    PsqLx => "psq_lx", PsqLux => "psq_lux", PsqStx => "psq_stx", PsqStux => "psq_stux",
    PsAdd => "ps_add", PsSub => "ps_sub", PsMul => "ps_mul", PsDiv => "ps_div",
    PsMadd => "ps_madd", PsMsub => "ps_msub", PsNmadd => "ps_nmadd", PsNmsub => "ps_nmsub",
    PsMadds0 => "ps_madds0", PsMadds1 => "ps_madds1", PsMuls0 => "ps_muls0", PsMuls1 => "ps_muls1",
    PsSum0 => "ps_sum0", PsSum1 => "ps_sum1", PsSel => "ps_sel", PsRes => "ps_res", PsRsqrte => "ps_rsqrte",
    PsMr => "ps_mr", PsNeg => "ps_neg", PsAbs => "ps_abs", PsNabs => "ps_nabs",
    PsMerge00 => "ps_merge00", PsMerge01 => "ps_merge01", PsMerge10 => "ps_merge10", PsMerge11 => "ps_merge11",
    PsCmpu0 => "ps_cmpu0", PsCmpo0 => "ps_cmpo0", PsCmpu1 => "ps_cmpu1", PsCmpo1 => "ps_cmpo1",
}

pub fn decode(w: u32) -> Op {
    use Op::*;
    let xo10 = (w >> 1) & 0x3ff;
    let xo9 = (w >> 1) & 0x1ff;
    let xo5 = (w >> 1) & 0x1f;
    match w >> 26 {
        3 => Twi,
        4 => match xo10 {
            0 => PsCmpu0, 32 => PsCmpo0, 40 => PsNeg, 64 => PsCmpu1, 72 => PsMr, 96 => PsCmpo1,
            136 => PsNabs, 264 => PsAbs, 528 => PsMerge00, 560 => PsMerge01, 592 => PsMerge10,
            624 => PsMerge11, 1014 => DcbzL,
            _ => match (w >> 1) & 0x3f {
                6 => PsqLx, 7 => PsqStx, 38 => PsqLux, 39 => PsqStux,
                _ => match xo5 {
                    10 => PsSum0, 11 => PsSum1, 12 => PsMuls0, 13 => PsMuls1, 14 => PsMadds0,
                    15 => PsMadds1, 18 => PsDiv, 20 => PsSub, 21 => PsAdd, 23 => PsSel, 24 => PsRes,
                    25 => PsMul, 26 => PsRsqrte, 28 => PsMsub, 29 => PsMadd, 30 => PsNmsub, 31 => PsNmadd,
                    _ => Illegal,
                },
            },
        },
        7 => Mulli, 8 => Subfic, 10 => Cmpli, 11 => Cmpi, 12 => Addic, 13 => AddicRc,
        14 => Addi, 15 => Addis, 16 => Bc, 17 if w & 2 != 0 => Sc, 18 => B,
        19 => match xo10 {
            0 => Mcrf, 16 => Bclr, 33 => Crnor, 50 => Rfi, 129 => Crandc, 150 => Isync, 193 => Crxor,
            225 => Crnand, 257 => Crand, 289 => Creqv, 417 => Crorc, 449 => Cror, 528 => Bcctr,
            _ => Illegal,
        },
        20 => Rlwimi, 21 => Rlwinm, 23 => Rlwnm,
        24 => Ori, 25 => Oris, 26 => Xori, 27 => Xoris, 28 => AndiRc, 29 => AndisRc,
        31 => match xo9 {
            8 => Subfc, 10 => Addc, 40 => Subf, 104 => Neg, 136 => Subfe, 138 => Adde,
            200 => Subfze, 202 => Addze, 232 => Subfme, 234 => Addme, 235 => Mullw, 266 => Add,
            459 => Divwu, 491 => Divw,
            11 if w & 0x400 == 0 => Mulhwu,
            75 if w & 0x400 == 0 => Mulhw,
            _ => match xo10 {
                0 => Cmp, 4 => Tw, 19 => Mfcr, 20 => Lwarx, 23 => Lwzx, 24 => Slw, 26 => Cntlzw,
                28 => And, 32 => Cmpl, 54 => Dcbst, 55 => Lwzux, 60 => Andc, 83 => Mfmsr, 86 => Dcbf,
                87 => Lbzx, 119 => Lbzux, 124 => Nor, 144 => Mtcrf, 146 => Mtmsr, 150 => StwcxRc,
                151 => Stwx, 183 => Stwux, 210 => Mtsr, 215 => Stbx, 242 => Mtsrin, 246 => Dcbtst,
                247 => Stbux, 278 => Dcbt, 279 => Lhzx, 284 => Eqv, 306 => Tlbie, 310 => Eciwx,
                311 => Lhzux, 316 => Xor, 339 => Mfspr, 343 => Lhax, 370 => Tlbia, 371 => Mftb,
                375 => Lhaux, 407 => Sthx, 412 => Orc, 438 => Ecowx, 439 => Sthux, 444 => Or,
                467 => Mtspr, 470 => Dcbi, 476 => Nand, 512 => Mcrxr, 533 => Lswx, 534 => Lwbrx,
                535 => Lfsx, 536 => Srw, 566 => Tlbsync, 567 => Lfsux, 595 => Mfsr, 597 => Lswi,
                598 => Sync, 599 => Lfdx, 631 => Lfdux, 659 => Mfsrin, 661 => Stswx, 662 => Stwbrx,
                663 => Stfsx, 695 => Stfsux, 725 => Stswi, 727 => Stfdx, 759 => Stfdux, 790 => Lhbrx,
                792 => Sraw, 824 => Srawi, 854 => Eieio, 918 => Sthbrx, 922 => Extsh, 954 => Extsb,
                982 => Icbi, 983 => Stfiwx, 1014 => Dcbz,
                _ => Illegal,
            },
        },
        32 => Lwz, 33 => Lwzu, 34 => Lbz, 35 => Lbzu, 36 => Stw, 37 => Stwu, 38 => Stb, 39 => Stbu,
        40 => Lhz, 41 => Lhzu, 42 => Lha, 43 => Lhau, 44 => Sth, 45 => Sthu, 46 => Lmw, 47 => Stmw,
        48 => Lfs, 49 => Lfsu, 50 => Lfd, 51 => Lfdu, 52 => Stfs, 53 => Stfsu, 54 => Stfd, 55 => Stfdu,
        56 => PsqL, 57 => PsqLu, 60 => PsqSt, 61 => PsqStu,
        59 => match xo5 {
            18 => Fdivs, 20 => Fsubs, 21 => Fadds, 24 => Fres, 25 => Fmuls,
            28 => Fmsubs, 29 => Fmadds, 30 => Fnmsubs, 31 => Fnmadds,
            _ => Illegal,
        },
        63 => match xo5 {
            18 => Fdiv, 20 => Fsub, 21 => Fadd, 23 => Fsel, 25 => Fmul, 26 => Frsqrte,
            28 => Fmsub, 29 => Fmadd, 30 => Fnmsub, 31 => Fnmadd,
            _ => match xo10 {
                0 => Fcmpu, 12 => Frsp, 14 => Fctiw, 15 => Fctiwz, 32 => Fcmpo, 38 => Mtfsb1,
                40 => Fneg, 64 => Mcrfs, 70 => Mtfsb0, 72 => Fmr, 134 => Mtfsfi, 136 => Fnabs,
                264 => Fabs, 583 => Mffs, 711 => Mtfsf,
                _ => Illegal,
            },
        },
        _ => Illegal,
    }
}

/// A decoded instruction at a known address.
#[derive(Debug, Clone, Copy)]
pub struct Insn {
    pub addr: u32,
    pub word: u32,
    pub op: Op,
}

fn sext(v: u32, bits: u32) -> i32 {
    ((v << (32 - bits)) as i32) >> (32 - bits)
}

impl Insn {
    #[inline]
    pub fn new(addr: u32, word: u32) -> Self {
        Insn { addr, word, op: decode(word) }
    }

    #[inline]
    pub fn rd(&self) -> u32 { (self.word >> 21) & 31 }
    #[inline]
    pub fn rs(&self) -> u32 { self.rd() }
    #[inline]
    pub fn ra(&self) -> u32 { (self.word >> 16) & 31 }
    #[inline]
    pub fn rb(&self) -> u32 { (self.word >> 11) & 31 }
    #[inline]
    pub fn rc_reg(&self) -> u32 { (self.word >> 6) & 31 }
    #[inline]
    pub fn crfd(&self) -> u32 { self.rd() >> 2 }
    #[inline]
    pub fn crfs(&self) -> u32 { self.ra() >> 2 }
    #[inline]
    pub fn simm(&self) -> i32 { self.word as u16 as i16 as i32 }
    #[inline]
    pub fn uimm(&self) -> u32 { self.word & 0xffff }
    #[inline]
    pub fn sh(&self) -> u32 { self.rb() }
    #[inline]
    pub fn mb(&self) -> u32 { (self.word >> 6) & 31 }
    #[inline]
    pub fn me(&self) -> u32 { (self.word >> 1) & 31 }
    #[inline]
    pub fn rc(&self) -> bool { self.word & 1 != 0 }
    #[inline]
    pub fn oe(&self) -> bool { self.word & 0x400 != 0 }
    #[inline]
    pub fn lk(&self) -> bool { self.word & 1 != 0 }
    #[inline]
    pub fn aa(&self) -> bool { self.word & 2 != 0 }
    #[inline]
    pub fn bo(&self) -> u32 { self.rd() }
    #[inline]
    pub fn bi(&self) -> u32 { self.ra() }
    #[inline]
    pub fn crm(&self) -> u32 { (self.word >> 12) & 0xff }
    #[inline]
    pub fn spr(&self) -> u32 { ((self.word >> 16) & 31) | (((self.word >> 11) & 31) << 5) }
    /// Paired-single quantized load/store: (W, I, displacement).
    #[inline]
    pub fn psq_d(&self) -> (u32, u32, i32) {
        ((self.word >> 15) & 1, (self.word >> 12) & 7, sext(self.word & 0xfff, 12))
    }
    #[inline]
    pub fn psq_x(&self) -> (u32, u32) {
        ((self.word >> 10) & 1, (self.word >> 7) & 7)
    }

    /// Absolute target of a direct branch (`b`/`bc`).
    #[inline]
    pub fn branch_target(&self) -> Option<u32> {
        let disp = match self.op {
            Op::B => sext(self.word & 0x03ff_fffc, 26),
            Op::Bc => sext(self.word & 0xfffc, 16),
            _ => return None,
        };
        Some(if self.aa() { disp as u32 } else { self.addr.wrapping_add(disp as u32) })
    }

    /// True if BO says "branch always" (ignoring CTR and CR).
    #[inline]
    pub fn is_unconditional(&self) -> bool {
        match self.op {
            Op::B => true,
            Op::Bc | Op::Bclr | Op::Bcctr => self.bo() & 0b10100 == 0b10100,
            _ => false,
        }
    }

    #[inline]
    pub fn is_branch(&self) -> bool {
        matches!(self.op, Op::B | Op::Bc | Op::Bclr | Op::Bcctr | Op::Rfi | Op::Sc)
    }
}

fn spr_name(spr: u32) -> Option<&'static str> {
    Some(match spr {
        1 => "xer", 8 => "lr", 9 => "ctr", 18 => "dsisr", 19 => "dar", 22 => "dec", 26 => "srr0",
        27 => "srr1", 272 => "sprg0", 273 => "sprg1", 274 => "sprg2", 275 => "sprg3",
        912..=919 => ["gqr0", "gqr1", "gqr2", "gqr3", "gqr4", "gqr5", "gqr6", "gqr7"][spr as usize - 912],
        920 => "hid2", 1008 => "hid0", 1009 => "hid1", 1017 => "l2cr", 1011 => "hid4",
        _ => return None,
    })
}

fn hex(v: i32) -> String {
    if v < 0 { format!("-0x{:x}", -(v as i64)) } else { format!("0x{v:x}") }
}

/// Simplified mnemonic (`beq`, `bdnz`, `blr`, ...), or None if the BO/BI
/// combination has no simplified form.
fn branch_mnemonic(i: &Insn) -> Option<String> {
    let (bo, bi) = (i.bo() & !1, i.bi());
    let suffix = match i.op {
        Op::Bclr => "lr",
        Op::Bcctr => "ctr",
        _ => "",
    };
    let link = if i.lk() { "l" } else { "" };
    let base = if bo & 0b10100 == 0b10100 {
        "b".to_string()
    } else if bo == 12 || bo == 4 {
        let c = ["lt", "gt", "eq", "so"][(bi & 3) as usize];
        let n = ["ge", "le", "ne", "ns"][(bi & 3) as usize];
        format!("b{}", if bo == 12 { c } else { n })
    } else if bo == 16 {
        "bdnz".into()
    } else if bo == 18 {
        "bdz".into()
    } else {
        return None;
    };
    Some(format!("{base}{suffix}{link}"))
}

impl std::fmt::Display for Insn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use Op::*;
        let i = self;
        let (d, a, b, c) = (i.rd(), i.ra(), i.rb(), i.rc_reg());
        let dot = if i.rc() { "." } else { "" };
        let m = i.op.mnemonic();
        let mem = |reg: &str| format!("{reg}{d}, {}(r{a})", hex(i.simm()));
        match i.op {
            Illegal => write!(f, ".word 0x{:08x}", i.word),
            // Common simplified mnemonics first.
            Addi if a == 0 => write!(f, "li r{d}, {}", hex(i.simm())),
            Addis if a == 0 => write!(f, "lis r{d}, 0x{:x}", i.uimm()),
            Ori if i.word == 0x6000_0000 => write!(f, "nop"),
            Or if i.rs() == b => write!(f, "mr{dot} r{a}, r{d}"),
            Mfspr if spr_name(i.spr()).is_some_and(|n| matches!(n, "lr" | "ctr" | "xer")) => {
                write!(f, "mf{} r{d}", spr_name(i.spr()).unwrap())
            }
            Mtspr if spr_name(i.spr()).is_some_and(|n| matches!(n, "lr" | "ctr" | "xer")) => {
                write!(f, "mt{} r{d}", spr_name(i.spr()).unwrap())
            }
            Addi | Addic | AddicRc | Subfic | Mulli => write!(f, "{m} r{d}, r{a}, {}", hex(i.simm())),
            Addis => write!(f, "{m} r{d}, r{a}, 0x{:x}", i.uimm()),
            Ori | Oris | Xori | Xoris | AndiRc | AndisRc => write!(f, "{m} r{a}, r{d}, 0x{:x}", i.uimm()),
            Cmpi => write!(f, "{m} cr{}, r{a}, {}", i.crfd(), hex(i.simm())),
            Cmpli => write!(f, "{m} cr{}, r{a}, 0x{:x}", i.crfd(), i.uimm()),
            Cmp | Cmpl => write!(f, "{m} cr{}, r{a}, r{b}", i.crfd()),
            Twi => write!(f, "{m} {d}, r{a}, {}", hex(i.simm())),
            Tw => write!(f, "{m} {d}, r{a}, r{b}"),
            Add | Addc | Adde | Subf | Subfc | Subfe | Mullw | Mulhw | Mulhwu | Divw | Divwu => {
                let o = if i.oe() && !matches!(i.op, Mulhw | Mulhwu) { "o" } else { "" };
                write!(f, "{m}{o}{dot} r{d}, r{a}, r{b}")
            }
            Neg | Addze | Addme | Subfze | Subfme => {
                write!(f, "{m}{}{dot} r{d}, r{a}", if i.oe() { "o" } else { "" })
            }
            And | Andc | Or | Orc | Xor | Nand | Nor | Eqv | Slw | Srw | Sraw => {
                write!(f, "{m}{dot} r{a}, r{d}, r{b}")
            }
            Srawi => write!(f, "{m}{dot} r{a}, r{d}, {}", i.sh()),
            Cntlzw | Extsb | Extsh => write!(f, "{m}{dot} r{a}, r{d}"),
            Rlwinm | Rlwimi => write!(f, "{m}{dot} r{a}, r{d}, {}, {}, {}", i.sh(), i.mb(), i.me()),
            Rlwnm => write!(f, "{m}{dot} r{a}, r{d}, r{b}, {}, {}", i.mb(), i.me()),
            Lwz | Lwzu | Lhz | Lhzu | Lha | Lhau | Lbz | Lbzu | Stw | Stwu | Sth | Sthu | Stb | Stbu | Lmw
            | Stmw => write!(f, "{m} {}", mem("r")),
            Lfs | Lfsu | Lfd | Lfdu | Stfs | Stfsu | Stfd | Stfdu => write!(f, "{m} {}", mem("f")),
            Lwzx | Lwzux | Lhzx | Lhzux | Lhax | Lhaux | Lbzx | Lbzux | Stwx | Stwux | Sthx | Sthux | Stbx
            | Stbux | Lwbrx | Lhbrx | Stwbrx | Sthbrx | Lswx | Stswx | Lwarx | StwcxRc | Eciwx | Ecowx => {
                write!(f, "{m} r{d}, r{a}, r{b}")
            }
            Lfsx | Lfsux | Lfdx | Lfdux | Stfsx | Stfsux | Stfdx | Stfdux | Stfiwx => {
                write!(f, "{m} f{d}, r{a}, r{b}")
            }
            Lswi | Stswi => write!(f, "{m} r{d}, r{a}, {}", if b == 0 { 32 } else { b }),
            B | Bc => {
                let t = i.branch_target().unwrap();
                let mn = if i.op == B {
                    Some(format!("b{}{}", if i.lk() { "l" } else { "" }, if i.aa() { "a" } else { "" }))
                } else {
                    branch_mnemonic(i)
                };
                let Some(mn) = mn else {
                    return write!(f, "bc{} {}, {}, 0x{t:08x}", if i.lk() { "l" } else { "" }, i.bo(), i.bi());
                };
                match i.op {
                    Bc if i.bi() >= 4 && (i.bo() & !1 == 12 || i.bo() & !1 == 4) => {
                        write!(f, "{mn} cr{}, 0x{t:08x}", i.bi() >> 2)
                    }
                    _ => write!(f, "{mn} 0x{t:08x}"),
                }
            }
            Bclr | Bcctr => {
                let Some(mn) = branch_mnemonic(i) else {
                    return write!(f, "{m}{} {}, {}", if i.lk() { "l" } else { "" }, i.bo(), i.bi());
                };
                if i.bi() >= 4 && (i.bo() & !1 == 12 || i.bo() & !1 == 4) {
                    write!(f, "{mn} cr{}", i.bi() >> 2)
                } else {
                    write!(f, "{mn}")
                }
            }
            Sc | Rfi | Sync | Isync | Eieio | Tlbia | Tlbsync => write!(f, "{m}"),
            Mcrf | Mcrfs => write!(f, "{m} cr{}, cr{}", i.crfd(), i.crfs()),
            Crand | Crandc | Creqv | Crnand | Crnor | Cror | Crorc | Crxor => write!(f, "{m} {d}, {a}, {b}"),
            Mfcr | Mfmsr => write!(f, "{m} r{d}"),
            Mtmsr => write!(f, "{m} r{d}"),
            Mtcrf => write!(f, "{m} 0x{:02x}, r{d}", i.crm()),
            Mcrxr => write!(f, "{m} cr{}", i.crfd()),
            Mfspr | Mftb => match spr_name(i.spr()) {
                Some(n) => write!(f, "{m} r{d}, {n}"),
                None => write!(f, "{m} r{d}, {}", i.spr()),
            },
            Mtspr => match spr_name(i.spr()) {
                Some(n) => write!(f, "{m} {n}, r{d}"),
                None => write!(f, "{m} {}, r{d}", i.spr()),
            },
            Mfsr => write!(f, "{m} r{d}, {}", a & 15),
            Mtsr => write!(f, "{m} {}, r{d}", a & 15),
            Mfsrin | Mtsrin => write!(f, "{m} r{d}, r{b}"),
            Tlbie => write!(f, "{m} r{b}"),
            Dcbf | Dcbi | Dcbst | Dcbt | Dcbtst | Dcbz | DcbzL | Icbi => write!(f, "{m} r{a}, r{b}"),
            Fadd | Fsub | Fdiv | Fadds | Fsubs | Fdivs | PsAdd | PsSub | PsDiv => {
                write!(f, "{m}{dot} f{d}, f{a}, f{b}")
            }
            Fmul | Fmuls | PsMul | PsMuls0 | PsMuls1 => write!(f, "{m}{dot} f{d}, f{a}, f{c}"),
            Fmadd | Fmsub | Fnmadd | Fnmsub | Fmadds | Fmsubs | Fnmadds | Fnmsubs | Fsel | PsMadd | PsMsub
            | PsNmadd | PsNmsub | PsMadds0 | PsMadds1 | PsSum0 | PsSum1 | PsSel => {
                write!(f, "{m}{dot} f{d}, f{a}, f{c}, f{b}")
            }
            Frsqrte | Fres | Frsp | Fctiw | Fctiwz | Fmr | Fneg | Fabs | Fnabs | PsRes | PsRsqrte | PsMr | PsNeg
            | PsAbs | PsNabs => write!(f, "{m}{dot} f{d}, f{b}"),
            PsMerge00 | PsMerge01 | PsMerge10 | PsMerge11 => write!(f, "{m}{dot} f{d}, f{a}, f{b}"),
            Fcmpu | Fcmpo | PsCmpu0 | PsCmpo0 | PsCmpu1 | PsCmpo1 => {
                write!(f, "{m} cr{}, f{a}, f{b}", i.crfd())
            }
            Mffs => write!(f, "{m}{dot} f{d}"),
            Mtfsf => write!(f, "{m}{dot} 0x{:02x}, f{b}", (i.word >> 17) & 0xff),
            Mtfsfi => write!(f, "{m}{dot} cr{}, {}", i.crfd(), (i.word >> 12) & 15),
            Mtfsb0 | Mtfsb1 => write!(f, "{m}{dot} {d}"),
            PsqL | PsqLu | PsqSt | PsqStu => {
                let (w, q, disp) = i.psq_d();
                write!(f, "{m} f{d}, {}(r{a}), {w}, qr{q}", hex(disp))
            }
            PsqLx | PsqLux | PsqStx | PsqStux => {
                let (w, q) = i.psq_x();
                write!(f, "{m} f{d}, r{a}, r{b}, {w}, qr{q}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dis(addr: u32, w: u32) -> String {
        Insn::new(addr, w).to_string()
    }

    #[test]
    fn common_instructions() {
        assert_eq!(dis(0, 0x9421_ffe0), "stwu r1, -0x20(r1)");
        assert_eq!(dis(0, 0x7c08_02a6), "mflr r0");
        assert_eq!(dis(0, 0x4e80_0020), "blr");
        assert_eq!(dis(0, 0x4e80_0421), "bctrl");
        assert_eq!(dis(0, 0x3860_0001), "li r3, 0x1");
        assert_eq!(dis(0, 0x3c60_8000), "lis r3, 0x8000");
        assert_eq!(dis(0, 0x7c7f_1b78), "mr r31, r3");
        assert_eq!(dis(0, 0x6000_0000), "nop");
        assert_eq!(dis(0, 0x5463_103a), "rlwinm r3, r3, 2, 0, 29");
        assert_eq!(dis(0, 0x2c03_0000), "cmpwi cr0, r3, 0x0");
        assert_eq!(dis(0, 0xec21_002a), "fadds f1, f1, f0");
    }

    #[test]
    fn branches() {
        assert_eq!(dis(0x8000_0100, 0x4800_0011), "bl 0x80000110");
        assert_eq!(dis(0x8000_0100, 0x4bff_fffc), "b 0x800000fc");
        assert_eq!(dis(0x8000_0100, 0x4182_0010), "beq 0x80000110");
        assert_eq!(dis(0x8000_0100, 0x4082_fff0), "bne 0x800000f0");
        assert_eq!(dis(0x8000_0100, 0x4200_fff8), "bdnz 0x800000f8");
        assert_eq!(dis(0, 0x4d82_0020), "beqlr");
    }

    #[test]
    fn paired_singles() {
        // psq_l f1, 0x8(r3), 0, qr0
        assert_eq!(dis(0, 0xe023_0008), "psq_l f1, 0x8(r3), 0, qr0");
        // ps_add f0, f1, f2
        assert_eq!(dis(0, 0x1001_102a), "ps_add f0, f1, f2");
        // ps_merge00 f0, f1, f2
        assert_eq!(dis(0, 0x1001_1420), "ps_merge00 f0, f1, f2");
    }
}
