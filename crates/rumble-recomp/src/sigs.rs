//! Finding SDK functions by byte signature. The DOL has no symbols, and
//! mods rebuild it, so addresses differ between builds; the code of an SDK
//! function does not, apart from fields the linker fills in. A signature is
//! a function's leading instructions with those fields masked out.

use crate::analysis::Program;

/// Clears the parts of an instruction that depend on where things were
/// linked: branch targets, `lis`/`addi` immediates (address halves) and
/// data displacements. Stack-relative (r1) displacements are kept, since
/// they describe the function itself.
pub fn mask(word: u32) -> u32 {
    let ra = (word >> 16) & 31;
    match word >> 26 {
        // b / bl
        18 => word & 0xfc00_0003,
        // addi / addis (li / lis)
        14 | 15 => word & 0xffff_0000,
        // D-form integer, FP and paired-single loads and stores
        32..=57 | 60 | 61 if ra != 1 => word & 0xffff_0000,
        _ => word,
    }
}

/// The signature of the `len` instructions starting at `addr`.
pub fn signature(prog: &Program, addr: u32, len: usize) -> Option<Vec<u32>> {
    (0..len as u32).map(|k| prog.insn(addr + 4 * k).map(|i| mask(i.word))).collect()
}

/// Function starts whose leading instructions match `sig`.
pub fn find(prog: &Program, sig: &[u32]) -> Vec<u32> {
    prog.functions
        .iter()
        .filter(|f| (f.end - f.start) as usize >= 4 * sig.len())
        .filter(|f| signature(prog, f.start, sig.len()).as_deref() == Some(sig))
        .map(|f| f.start)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::mask;

    #[test]
    fn masks_link_dependent_fields() {
        assert_eq!(mask(0x4bfc_02d5), 0x4800_0001); // bl
        assert_eq!(mask(0x3c60_cd00), 0x3c60_0000); // lis r3, 0xcd00
        assert_eq!(mask(0x806d_b3a8), 0x806d_0000); // lwz r3, -0x4c58(r13)
        assert_eq!(mask(0x9001_0014), 0x9001_0014); // stw r0, 0x14(r1)
        assert_eq!(mask(0x7c08_02a6), 0x7c08_02a6); // mflr r0
    }
}
