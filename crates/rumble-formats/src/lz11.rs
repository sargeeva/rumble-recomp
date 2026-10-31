//! Nintendo LZ77 type 0x11 ("LZ11"), used for the compressed main DOL.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Lz11Error {
    #[error("not LZ11-compressed data")]
    BadHeader,
    #[error("LZ11 stream is truncated or malformed")]
    Malformed,
}

pub fn is_lz11(data: &[u8]) -> bool {
    data.len() >= 4 && data[0] == 0x11
}

pub fn decompress(data: &[u8]) -> Result<Vec<u8>, Lz11Error> {
    if !is_lz11(data) {
        return Err(Lz11Error::BadHeader);
    }
    let mut size = u32::from_le_bytes([data[1], data[2], data[3], 0]) as usize;
    let mut p = 4;
    if size == 0 {
        let ext = data.get(4..8).ok_or(Lz11Error::Malformed)?;
        size = u32::from_le_bytes(ext.try_into().unwrap()) as usize;
        p = 8;
    }
    let byte = |i: usize| data.get(i).copied().ok_or(Lz11Error::Malformed);

    let mut out = Vec::with_capacity(size);
    while out.len() < size {
        let flags = byte(p)?;
        p += 1;
        for bit in (0..8).rev() {
            if out.len() >= size {
                break;
            }
            if flags & (1 << bit) == 0 {
                out.push(byte(p)?);
                p += 1;
                continue;
            }
            // The high nibble of the first byte selects a 2/3/4-byte token.
            let b0 = byte(p)? as usize;
            let (len, disp_hi, disp_lo) = match b0 >> 4 {
                0 => {
                    let b1 = byte(p + 1)? as usize;
                    p += 3;
                    (((b0 & 0xf) << 4 | b1 >> 4) + 0x11, b1 & 0xf, byte(p - 1)?)
                }
                1 => {
                    let (b1, b2) = (byte(p + 1)? as usize, byte(p + 2)? as usize);
                    p += 4;
                    (((b0 & 0xf) << 12 | b1 << 4 | b2 >> 4) + 0x111, b2 & 0xf, byte(p - 1)?)
                }
                n => {
                    p += 2;
                    (n + 1, b0 & 0xf, byte(p - 1)?)
                }
            };
            let disp = (disp_hi << 8 | disp_lo as usize) + 1;
            let from = out.len().checked_sub(disp).ok_or(Lz11Error::Malformed)?;
            for k in 0..len.min(size - out.len()) {
                out.push(out[from + k]);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn literals_and_overlapping_copy() {
        // "ab" as literals, then a 2-byte token: length 6, distance 2.
        let data = [0x11, 8, 0, 0, 0b0010_0000, b'a', b'b', 0x50, 0x01];
        assert_eq!(super::decompress(&data).unwrap(), b"abababab");
    }
}
