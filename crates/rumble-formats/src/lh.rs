//! Nintendo CX "LH" compression (type 0x40): LZ77 whose literals/match
//! lengths and match distances are Huffman coded. Used by the game's
//! `*.cx` files.
//!
//! Layout: 4-byte header (0x4X, 24-bit LE size; size 0 means a 32-bit LE
//! size follows), then an MSB-first bit stream holding a 9-bit-entry
//! length tree, a 5-bit-entry distance tree, and the symbols.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum LhError {
    #[error("not LH-compressed data")]
    BadHeader,
    #[error("LH stream is truncated")]
    Truncated,
    #[error("LH stream is malformed")]
    Malformed,
}

pub fn is_lh(data: &[u8]) -> bool {
    data.len() >= 4 && data[0] >> 4 == 4
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u64,
    bits: u32,
}

impl BitReader<'_> {
    fn read(&mut self, n: u32) -> Result<u32, LhError> {
        while self.bits < n {
            let b = *self.data.get(self.pos).ok_or(LhError::Truncated)?;
            self.pos += 1;
            self.acc = (self.acc << 8) | b as u64;
            self.bits += 8;
        }
        self.bits -= n;
        let v = (self.acc >> self.bits) as u32 & ((1u32 << n) - 1);
        self.acc &= (1u64 << self.bits) - 1;
        Ok(v)
    }
}

/// A Huffman tree stored as a flat array of `width`-bit entries. Entry 1 is
/// the root. The top two bits of an internal entry flag whether its 0/1
/// child is a leaf; the remaining bits give the distance to the child pair.
struct Tree {
    entries: Vec<u16>,
    width: u32,
}

impl Tree {
    /// `size_bits` is the width of the section-size field preceding the
    /// entries; the section is `(size + 1) * 4` bytes including that field.
    fn read(r: &mut BitReader, size_bits: u32, width: u32) -> Result<Self, LhError> {
        let mut size = r.read(size_bits)?;
        if size_bits == 16 {
            size = (size as u16).swap_bytes() as u32;
        }
        let mut remaining = (size + 1) * 32 - size_bits;
        let mut entries = vec![0];
        while remaining >= width {
            entries.push(r.read(width)? as u16);
            remaining -= width;
        }
        if remaining > 0 {
            r.read(remaining)?;
        }
        Ok(Tree { entries, width })
    }

    fn decode(&self, r: &mut BitReader) -> Result<u16, LhError> {
        let leaf0 = 1u16 << (self.width - 1);
        let leaf1 = 1u16 << (self.width - 2);
        let mut i = 1usize;
        loop {
            let e = *self.entries.get(i).ok_or(LhError::Malformed)?;
            let bit = r.read(1)? as usize;
            let child = (i & !1) + 2 * ((e & (leaf1 - 1)) as usize + 1) + bit;
            let value = *self.entries.get(child).ok_or(LhError::Malformed)?;
            if e & if bit == 0 { leaf0 } else { leaf1 } != 0 {
                return Ok(value);
            }
            i = child;
        }
    }
}

pub fn decompress(data: &[u8]) -> Result<Vec<u8>, LhError> {
    if !is_lh(data) {
        return Err(LhError::BadHeader);
    }
    let mut size = u32::from_le_bytes([data[1], data[2], data[3], 0]) as usize;
    let mut start = 4;
    if size == 0 {
        let ext = data.get(4..8).ok_or(LhError::Truncated)?;
        size = u32::from_le_bytes(ext.try_into().unwrap()) as usize;
        start = 8;
    }

    let mut r = BitReader { data, pos: start, acc: 0, bits: 0 };
    let lengths = Tree::read(&mut r, 16, 9)?;
    let distances = Tree::read(&mut r, 8, 5)?;

    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        let sym = lengths.decode(&mut r)?;
        if sym < 0x100 {
            out.push(sym as u8);
            continue;
        }
        let len = (sym & 0xff) as usize + 3;
        let extra = distances.decode(&mut r)? as u32;
        let back = match extra {
            0 | 1 => extra,
            n => r.read(n - 1)? | (1 << (n - 1)),
        } as usize
            + 1;
        let from = out.len().checked_sub(back).ok_or(LhError::Malformed)?;
        for k in 0..len.min(size - out.len()) {
            out.push(out[from + k]);
        }
    }
    Ok(out)
}
