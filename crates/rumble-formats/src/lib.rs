//! File formats used by the Pokémon Rumble (WiiWare) port: BPS patches,
//! WAD packages, U8 archives and Wii SD-card saves.

pub mod bps;
pub mod brres;
pub mod dol;
pub mod ec;
pub mod lh;
pub mod lz11;
pub mod u8arc;
pub mod wad;
pub mod wiisave;

pub(crate) fn be_u16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

pub(crate) fn be_u32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes(b[off..off + 4].try_into().unwrap())
}

pub(crate) fn be_u64(b: &[u8], off: usize) -> u64 {
    u64::from_be_bytes(b[off..off + 8].try_into().unwrap())
}

pub(crate) fn align(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}
