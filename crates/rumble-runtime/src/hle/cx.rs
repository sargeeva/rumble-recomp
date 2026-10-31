//! CX (compression library) replacements. The game unpacks its archives
//! with these, some tens of megabytes at load time; in the interpreter that
//! takes minutes, natively a fraction of a second. The output is the same:
//! `rumble_formats::lh` was checked byte for byte against the game's own
//! decompression of its archives.

use super::Replacement;
use crate::cpu::Cpu;
use crate::mem::{Fault, Memory};
use rumble_formats::lh::{self, LhError};

pub static REPLACEMENTS: &[Replacement] = &[Replacement {
    name: "CXSecureUncompressLH",
    sig: &[
        0x9421ffd0, 0x7c0802a6, 0x90010034, 0x39610000, 0x48000001, 0x88030000,
        0x7c781b78, 0x7c992378, 0x7cba2b78, 0x54000636, 0x7cdb3378, 0x2c000040,
        0x3bc00000, 0x4182000c, 0x38600000, 0x48000000, 0x28040004, 0x4181000c,
        0x38600000, 0x48000000, 0x80a30000, 0x3ba30000, 0x3b860000, 0x54a3421e,
    ],
    run: cx_secure_uncompress_lh,
}];

/// SDK error codes: wrong compression type, source too short, malformed.
const CX_ERR_UNSUPPORTED: i32 = -1;
const CX_ERR_SRC_SHORTAGE: i32 = -2;
const CX_ERR_ILLEGAL_TABLE: i32 = -4;

/// `CXSecureUncompressLH(src, src_size, dst, work)`: 0 on success.
fn cx_secure_uncompress_lh(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let (src, src_size, dst) = (cpu.gpr[3], cpu.gpr[4], cpu.gpr[5]);
    let data = mem.read_bytes(src, src_size)?;
    let result = match lh::decompress(&data) {
        Ok(mut out) => {
            super::multiplay::unpacked(mem, dst, &mut out);
            mem.load(dst, &out)?;
            0
        }
        Err(LhError::BadHeader) => CX_ERR_UNSUPPORTED,
        Err(LhError::Truncated) => CX_ERR_SRC_SHORTAGE,
        Err(_) => CX_ERR_ILLEGAL_TABLE,
    };
    cpu.gpr[3] = result as u32;
    Ok(())
}
