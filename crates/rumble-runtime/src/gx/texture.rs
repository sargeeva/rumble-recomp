//! GX texture formats, decoded to RGBA8. Textures are stored in tiles
//! (blocks) of 32 bytes, 64 for RGBA8; palette formats look their colors up
//! in a TLUT loaded into TMEM.

use crate::mem::Ram;

pub const I4: u32 = 0x0;
pub const I8: u32 = 0x1;
pub const IA4: u32 = 0x2;
pub const IA8: u32 = 0x3;
pub const RGB565: u32 = 0x4;
pub const RGB5A3: u32 = 0x5;
pub const RGBA8: u32 = 0x6;
pub const C4: u32 = 0x8;
pub const C8: u32 = 0x9;
pub const C14X2: u32 = 0xa;
pub const CMPR: u32 = 0xe;

/// TLUT entry formats.
const TLUT_IA8: u32 = 0;
const TLUT_RGB565: u32 = 1;

/// A decoded texture. `id` identifies its contents, so renderers can keep
/// one GPU copy per distinct image.
#[derive(Debug)]
pub struct Texture {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    /// Where it was decoded from, for diagnostics.
    pub key: TextureKey,
    pub rgba: Vec<u8>,
}

/// Where a texture's data and palette come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureKey {
    pub addr: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub tlut_offset: u32,
    pub tlut_format: u32,
}

/// Block width, height and bytes per block.
fn block(format: u32) -> Option<(u32, u32, u32)> {
    Some(match format {
        I4 | C4 | CMPR => (8, 8, 32),
        I8 | IA4 | C8 => (8, 4, 32),
        IA8 | RGB565 | RGB5A3 | C14X2 => (4, 4, 32),
        RGBA8 => (4, 4, 64),
        _ => return None,
    })
}

/// Bytes of texture data for an image of this size and format.
pub fn data_size(format: u32, width: u32, height: u32) -> Option<usize> {
    let (bw, bh, bytes) = block(format)?;
    Some((width.div_ceil(bw) * height.div_ceil(bh) * bytes) as usize)
}

fn rgb565(v: u16) -> [u8; 4] {
    let (r, g, b) = ((v >> 11) & 0x1f, (v >> 5) & 0x3f, v & 0x1f);
    [(r << 3 | r >> 2) as u8, (g << 2 | g >> 4) as u8, (b << 3 | b >> 2) as u8, 255]
}

fn rgb5a3(v: u16) -> [u8; 4] {
    if v & 0x8000 != 0 {
        let (r, g, b) = ((v >> 10) & 0x1f, (v >> 5) & 0x1f, v & 0x1f);
        [(r << 3 | r >> 2) as u8, (g << 3 | g >> 2) as u8, (b << 3 | b >> 2) as u8, 255]
    } else {
        let (a, r, g, b) = ((v >> 12) & 7, (v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
        [(r * 17) as u8, (g * 17) as u8, (b * 17) as u8, (a << 5 | a << 2 | a >> 1) as u8]
    }
}

fn ia8(v: u16) -> [u8; 4] {
    let (a, i) = ((v >> 8) as u8, v as u8);
    [i, i, i, a]
}

fn palette(tlut: &[u8], format: u32, index: u32) -> [u8; 4] {
    let k = (index as usize * 2) % tlut.len().max(2);
    let v = u16::from_be_bytes([tlut.get(k).copied().unwrap_or(0), tlut.get(k + 1).copied().unwrap_or(0)]);
    match format {
        TLUT_IA8 => ia8(v),
        TLUT_RGB565 => rgb565(v),
        _ => rgb5a3(v),
    }
}

/// One DXT1-style 4x4 sub-block of a CMPR texture.
fn cmpr_block(d: &[u8], out: &mut [[u8; 4]; 16]) {
    let c0 = u16::from_be_bytes([d[0], d[1]]);
    let c1 = u16::from_be_bytes([d[2], d[3]]);
    let (p0, p1) = (rgb565(c0), rgb565(c1));
    let mix = |a: u8, b: u8, wa: u32, wb: u32| ((a as u32 * wa + b as u32 * wb) / (wa + wb)) as u8;
    let lerp = |wa, wb| [mix(p0[0], p1[0], wa, wb), mix(p0[1], p1[1], wa, wb), mix(p0[2], p1[2], wa, wb), 255];
    let colors = if c0 > c1 {
        [p0, p1, lerp(2, 1), lerp(1, 2)]
    } else {
        [p0, p1, lerp(1, 1), [0, 0, 0, 0]]
    };
    for y in 0..4 {
        let row = d[4 + y];
        for x in 0..4 {
            out[y * 4 + x] = colors[((row >> (6 - 2 * x)) & 3) as usize];
        }
    }
}

/// Decodes a texture, or returns None for an unknown format or data
/// outside RAM. `tlut` is the palette for C4/C8/C14X2.
pub fn decode(data: &[u8], key: &TextureKey, tlut: &[u8]) -> Option<Vec<u8>> {
    let (bw, bh, block_bytes) = block(key.format)?;
    let (w, h) = (key.width, key.height);
    let mut out = vec![0u8; (w * h * 4) as usize];
    let mut put = |x: u32, y: u32, px: [u8; 4]| {
        if x < w && y < h {
            let o = ((y * w + x) * 4) as usize;
            out[o..o + 4].copy_from_slice(&px);
        }
    };
    let blocks_x = w.div_ceil(bw);
    for by in 0..h.div_ceil(bh) {
        for bx in 0..blocks_x {
            let off = ((by * blocks_x + bx) * block_bytes) as usize;
            let b = data.get(off..off + block_bytes as usize)?;
            let (x0, y0) = (bx * bw, by * bh);
            match key.format {
                I4 | C4 => {
                    for k in 0..64u32 {
                        let v = (b[(k / 2) as usize] >> (if k % 2 == 0 { 4 } else { 0 })) & 0xf;
                        let px = if key.format == I4 {
                            [v * 17; 4]
                        } else {
                            palette(tlut, key.tlut_format, v as u32)
                        };
                        put(x0 + k % 8, y0 + k / 8, px);
                    }
                }
                I8 | C8 => {
                    for k in 0..32u32 {
                        let v = b[k as usize];
                        let px = if key.format == I8 { [v; 4] } else { palette(tlut, key.tlut_format, v as u32) };
                        put(x0 + k % 8, y0 + k / 8, px);
                    }
                }
                IA4 => {
                    for k in 0..32u32 {
                        let v = b[k as usize];
                        let (a, i) = ((v >> 4) * 17, (v & 0xf) * 17);
                        put(x0 + k % 8, y0 + k / 8, [i, i, i, a]);
                    }
                }
                IA8 | RGB565 | RGB5A3 | C14X2 => {
                    for k in 0..16u32 {
                        let v = u16::from_be_bytes([b[2 * k as usize], b[2 * k as usize + 1]]);
                        let px = match key.format {
                            IA8 => ia8(v),
                            RGB565 => rgb565(v),
                            RGB5A3 => rgb5a3(v),
                            _ => palette(tlut, key.tlut_format, (v & 0x3fff) as u32),
                        };
                        put(x0 + k % 4, y0 + k / 4, px);
                    }
                }
                RGBA8 => {
                    // Alpha/red pairs for the 16 pixels, then green/blue.
                    for k in 0..16usize {
                        let (a, r) = (b[2 * k], b[2 * k + 1]);
                        let (g, bl) = (b[32 + 2 * k], b[32 + 2 * k + 1]);
                        put(x0 + k as u32 % 4, y0 + k as u32 / 4, [r, g, bl, a]);
                    }
                }
                CMPR => {
                    let mut sub = [[0u8; 4]; 16];
                    for s in 0..4u32 {
                        cmpr_block(&b[(s * 8) as usize..(s * 8 + 8) as usize], &mut sub);
                        let (sx, sy) = (x0 + (s % 2) * 4, y0 + (s / 2) * 4);
                        for k in 0..16u32 {
                            put(sx + k % 4, sy + k / 4, sub[k as usize]);
                        }
                    }
                }
                _ => return None,
            }
        }
    }
    Some(out)
}

/// FNV-1a over the texture's description, data and palette.
fn hash(key: &TextureKey, data: &[u8], tlut: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        let words = bytes.chunks_exact(8);
        let rest = words.remainder();
        for w in words {
            h ^= u64::from_le_bytes(w.try_into().unwrap());
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        if !rest.is_empty() {
            let mut w = [0u8; 8];
            w[..rest.len()].copy_from_slice(rest);
            h ^= u64::from_le_bytes(w);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    eat(&[key.width, key.height, key.format, key.tlut_format].map(u32::to_le_bytes).concat());
    eat(data);
    eat(tlut);
    h
}

/// The bytes a texture is decoded from (its data and palette), and their
/// hash, which identifies the decoded texture.
pub struct Source<'a> {
    pub id: u64,
    data: &'a [u8],
    tlut: &'a [u8],
}

/// Finds and hashes the texture `key` describes, without decoding it.
pub fn source<'a>(ram: &'a Ram, tmem: &'a [u8], key: &TextureKey) -> Option<Source<'a>> {
    let size = data_size(key.format, key.width, key.height)?;
    let data = ram.bytes(key.addr, size)?;
    let tlut = match key.format {
        C4 | C8 | C14X2 => {
            let entries = match key.format {
                C4 => 16,
                C8 => 256,
                _ => 16384,
            };
            let start = key.tlut_offset as usize;
            tmem.get(start..(start + entries * 2).min(tmem.len())).unwrap_or(&[])
        }
        _ => &[],
    };
    Some(Source { id: hash(key, data, tlut), data, tlut })
}

/// Decodes a texture found by `source`.
pub fn decode_source(src: &Source, key: &TextureKey) -> Option<Texture> {
    let rgba = decode(src.data, key, src.tlut)?;
    Some(Texture { id: src.id, width: key.width, height: key.height, key: *key, rgba })
}

/// Reads and decodes the texture `key` describes.
pub fn load(ram: &Ram, tmem: &[u8], key: &TextureKey) -> Option<Texture> {
    decode_source(&source(ram, tmem, key)?, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(format: u32, width: u32, height: u32) -> TextureKey {
        TextureKey { addr: 0, width, height, format, tlut_offset: 0, tlut_format: 0 }
    }

    #[test]
    fn i8_tiles_are_8x4() {
        // Two 8x4 tiles side by side: the second tile's first byte lands
        // at (8, 0).
        let mut data = vec![0u8; 64];
        data[0] = 10;
        data[32] = 20;
        data[9] = 30; // tile 0, row 1, column 1
        let out = decode(&data, &key(I8, 16, 4), &[]).unwrap();
        let px = |x: usize, y: usize| out[(y * 16 + x) * 4];
        assert_eq!((px(0, 0), px(8, 0), px(1, 1)), (10, 20, 30));
    }

    #[test]
    fn rgba8_splits_ar_and_gb() {
        let mut data = vec![0u8; 64];
        data[0] = 0x80; // A of pixel 0
        data[1] = 0x11; // R
        data[32] = 0x22; // G
        data[33] = 0x33; // B
        let out = decode(&data, &key(RGBA8, 4, 4), &[]).unwrap();
        assert_eq!(&out[..4], &[0x11, 0x22, 0x33, 0x80]);
    }

    #[test]
    fn rgb5a3_both_modes() {
        assert_eq!(rgb5a3(0xffff), [255, 255, 255, 255]);
        assert_eq!(rgb5a3(0x7f00), [255, 0, 0, 255]);
        assert_eq!(rgb5a3(0x0000), [0, 0, 0, 0]);
    }

    #[test]
    fn cmpr_picks_from_four_colors() {
        // c0 = white, c1 = black (c0 > c1: opaque 4-color mode); first row
        // indices 0,1,2,3.
        let mut d = [0u8; 32];
        d[..8].copy_from_slice(&[0xff, 0xff, 0x00, 0x00, 0b00_01_10_11, 0, 0, 0]);
        let out = decode(&d, &key(CMPR, 8, 8), &[]).unwrap();
        let row: Vec<u8> = (0..4).map(|x| out[x * 4]).collect();
        assert_eq!(row, [255, 0, 170, 85]);
    }

    #[test]
    fn c8_uses_the_tlut() {
        let mut data = vec![0u8; 32];
        data[0] = 1;
        let tlut = [0x00, 0x00, 0xf8, 0x00]; // entry 1: RGB565 red
        let k = TextureKey { tlut_format: TLUT_RGB565, ..key(C8, 8, 4) };
        let out = decode(&data, &k, &tlut).unwrap();
        assert_eq!(&out[..4], &[255, 0, 0, 255]);
    }
}
