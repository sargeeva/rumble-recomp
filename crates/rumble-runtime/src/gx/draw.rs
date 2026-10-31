//! Turns decoded GX primitives into self-contained draw commands for a
//! host renderer: vertices transformed to EFB clip space on the CPU
//! (position matrix, projection, viewport, depth range), the two color
//! channels and generated texture coordinates, plus the TEV, blend, depth
//! and scissor state and the textures the TEV stages use.
//!
//! Lighting is evaluated per vertex as GX does (`light_channel`). Lines and
//! points are not drawn yet.

use super::texture::{self, Texture, TextureKey};
use super::Gx;
use crate::mem::Ram;
use std::sync::Arc;

/// EFB size the game renders into.
pub const EFB_WIDTH: f32 = 640.0;
pub const EFB_HEIGHT: f32 = 528.0;

/// Floats per vertex: clip position (4), color 0 (4), color 1 (4), then
/// eight texture coordinates as (s, t, q).
/// Decoded textures not used for this many frames are dropped.
const DECODED_IDLE_FRAMES: u64 = 120;

pub const VERTEX_FLOATS: usize = 36;

/// Words in a draw's TEV uniform block (all 32-bit; f32 as bits):
///
/// | words  | contents                                                    |
/// |--------|-------------------------------------------------------------|
/// | 0-63   | 16 stages: color env, alpha env, tref, ksel (see below)     |
/// | 64-79  | TEV registers PREV, C0, C1, C2 as RGBA f32                  |
/// | 80-95  | konst colors K0-K3 as RGBA f32                              |
/// | 96-99  | stage count, alpha compare (BP 0xF3), 0, 0                  |
///
/// tref = texmap | texcoord << 3 | enable << 6 | channel << 7;
/// ksel = konst color select | konst alpha select << 8.
pub const TEV_WORDS: usize = 100;

#[derive(Debug, Clone)]
pub struct TexBinding {
    /// The decoded texture, shared by every texture with the same bytes.
    pub texture: Arc<Texture>,
    /// Where this one is read from. EFB copies never reach RAM, so the
    /// bytes behind different copies are often the same: the shared
    /// texture can't say which address a draw names, and copies are found
    /// by it.
    pub addr: u32,
    /// 0 clamp, 1 repeat, 2 mirror.
    pub wrap_s: u32,
    pub wrap_t: u32,
    pub linear: bool,
}

impl PartialEq for TexBinding {
    fn eq(&self, other: &Self) -> bool {
        self.texture.id == other.texture.id
            && self.addr == other.addr
            && (self.wrap_s, self.wrap_t, self.linear) == (other.wrap_s, other.wrap_t, other.linear)
    }
}

#[derive(Debug, Clone)]
pub struct Draw {
    /// Triangle list, `VERTEX_FLOATS` per vertex.
    pub vertices: Vec<f32>,
    pub textures: [Option<TexBinding>; 8],
    pub tev: [u32; TEV_WORDS],
    /// Raw BP 0x41 (blend mode, color/alpha update) and 0x40 (z mode).
    pub blend: u32,
    pub zmode: u32,
    /// Which faces are culled (BP 0x00 bits 14-15): 0 none, 1 back, 2
    /// front, 3 all. Front faces wind clockwise on screen (front and back
    /// already swapped for a mirrored viewport). Triangles keep the winding
    /// of the primitive they came from.
    pub cull: u32,
    /// x, y, width, height in EFB pixels.
    pub scissor: [u32; 4],
    /// Drawn through a viewport spanning the EFB's full width.
    ///
    /// With widescreen (`Gx::widescreen` = k, the EFB k times as wide), the
    /// game's 4:3 EFB sits centered in the wider one, and every draw keeps
    /// its place and shape there (clip-space x divided by k), as do
    /// copies and scissors of less than the full width. A full-width draw
    /// can also reach the sides: the game's cameras are made k times wider
    /// where it builds them (`hle::mtx`), so they draw, and don't cull, a
    /// wider view, and a 2D primitive covering the whole width (a
    /// backdrop, a fade) is stretched to fill. Its scissor,
    /// and a full-width copy, span the whole wider EFB.
    pub wide: bool,
}

/// An EFB copy: to the external framebuffer (a finished frame) or to a
/// texture in RAM, optionally clearing the EFB afterwards.
#[derive(Debug, Clone)]
pub struct EfbCopy {
    pub to_xfb: bool,
    /// x, y, width, height in EFB pixels.
    pub rect: [u32; 4],
    pub clear: Option<([f32; 4], f32)>,
    /// For a texture copy: the physical address it is written to (BP
    /// 0x4B), which a texture sampling it later names.
    pub dest: u32,
    /// Copied at half size (box filtered): the texture is half the rect.
    pub half: bool,
    /// The texture copy's pixel format (from control bits 3-6; see
    /// `copy.wgsl` in the renderer) and whether its intensity formats take
    /// luminance (bit 15) rather than red.
    pub format: u32,
    pub intensity: bool,
    /// Whether the EFB keeps alpha (pixel format RGBA6_Z24, BP 0x43); in
    /// the other formats alpha reads as opaque.
    pub efb_alpha: bool,
    /// Bytes from one row of tiles to the next in RAM (BP 0x4D, in 32-byte
    /// units), which can be wider than the copy.
    pub stride: u32,
}

impl EfbCopy {
    /// The size of the texture a copy to RAM makes.
    pub fn texture_size(&self) -> (u32, u32) {
        let (w, h) = (self.rect[2], self.rect[3]);
        if self.half { (w.div_ceil(2), h.div_ceil(2)) } else { (w, h) }
    }

    /// Bytes from the copy's start to its end in RAM.
    pub fn footprint(&self) -> u32 {
        copy_footprint(self.format, self.texture_size(), self.stride)
    }

    /// The RAM the copy covers.
    pub fn span(&self) -> CopySpan {
        CopySpan::new(self.dest, self.format, self.texture_size(), self.stride)
    }
}

/// The RAM an EFB copy covers: rows of tiles, `stride` bytes apart, each
/// `used` bytes long. With a stride wider than the copy (several images
/// side by side in one sheet, like the collection's icons), the gaps
/// between its rows belong to its neighbours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopySpan {
    pub start: u32,
    pub rows: u32,
    pub used: u32,
    pub stride: u32,
}

impl CopySpan {
    pub fn new(dest: u32, format: u32, size: (u32, u32), stride: u32) -> Self {
        let (tw, th, bytes, _) = copy_layout(format);
        let used = size.0.div_ceil(tw) * bytes;
        Self { start: dest, rows: size.1.div_ceil(th), used, stride: copy_row_bytes(format, size, stride).max(used) }
    }

    /// The byte ranges it covers, row by row.
    pub fn ranges(self) -> impl Iterator<Item = std::ops::Range<u32>> {
        (0..self.rows).map(move |r| {
            let a = self.start + r * self.stride;
            a..a + self.used
        })
    }

    pub fn contains(&self, addr: u32) -> bool {
        let Some(off) = addr.checked_sub(self.start) else { return false };
        off / self.stride < self.rows && off % self.stride < self.used
    }

    /// Whether any byte is covered by both.
    pub fn overlaps(&self, other: &CopySpan) -> bool {
        let (s, u) = (other.stride as i64, other.used as i64);
        self.ranges().any(|r| {
            // Rows k of `other` with start + k*s < r.end and
            // start + k*s + u > r.start.
            let base = other.start as i64;
            let lo = (r.start as i64 - u - base + 1).div_euclid(s) + i64::from((r.start as i64 - u - base + 1).rem_euclid(s) != 0);
            let hi = (r.end as i64 - 1 - base).div_euclid(s);
            lo.max(0) <= hi.min(other.rows as i64 - 1)
        })
    }
}

/// RAM layout of a copy format: (tile width, tile height, bytes per tile,
/// bits per pixel). GX textures are stored in tiles of 32 bytes (64 for
/// RGBA8, whose two halves sit in consecutive 32-byte blocks).
pub fn copy_layout(format: u32) -> (u32, u32, u32, u32) {
    match format {
        0 => (8, 8, 32, 4),
        1 | 2 | 7 | 8 | 9 | 10 => (8, 4, 32, 8),
        6 => (4, 4, 64, 32),
        _ => (4, 4, 32, 16),
    }
}

/// Bytes from one row of tiles to the next: the copy's stride, or its
/// width when the stride is unset.
pub fn copy_row_bytes(format: u32, size: (u32, u32), stride: u32) -> u32 {
    let (tw, _, bytes, _) = copy_layout(format);
    if stride > 0 { stride } else { size.0.div_ceil(tw) * bytes }
}

/// Bytes a copy covers in RAM.
pub fn copy_footprint(format: u32, size: (u32, u32), stride: u32) -> u32 {
    let (_, th, _, _) = copy_layout(format);
    size.1.div_ceil(th) * copy_row_bytes(format, size, stride)
}

#[derive(Debug, Clone)]
pub enum GpuCommand {
    Draw(Draw),
    Copy(EfbCopy),
    /// The copy made to this address is gone: the game has written other
    /// data over its RAM (copies stay on the GPU and never reach RAM, so
    /// the renderer can't see that itself).
    Forget(u32),
}

/// Words of a copy's RAM `span_hash` reads at most.
const SPAN_HASH_SAMPLES: u32 = 4096;

/// FNV-1a over the RAM a copy covers, for noticing writes over it: over
/// evenly spread 8-byte words of it (at most `SPAN_HASH_SAMPLES`, a few
/// hundred bytes apart for a full-screen copy), as hashing all of a big
/// copy each time it is made or looked at costs too much, and what
/// overwrites one (a texture loaded there) is bigger than the gaps.
fn span_hash(ram: &Ram, span: CopySpan) -> Option<u64> {
    let words = span.used.div_ceil(8);
    let step = (words * span.rows).div_ceil(SPAN_HASH_SAMPLES).max(1);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    // The word index across rows, so samples stay evenly spread.
    let mut next = 0;
    for (row, r) in span.ranges().enumerate() {
        let bytes = ram.bytes(r.start, (r.end - r.start) as usize)?;
        let first = row as u32 * words;
        while next < first + words {
            let at = ((next - first) * 8) as usize;
            let w = &bytes[at..(at + 8).min(bytes.len())];
            let mut b = [0u8; 8];
            b[..w.len()].copy_from_slice(w);
            h ^= u64::from_le_bytes(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
            next += step;
        }
    }
    Some(h)
}

fn f(bits: u32) -> f32 {
    f32::from_bits(bits)
}

/// One attribute component in a vertex format.
fn component(d: &[u8], format: u32, frac: u32) -> (f32, usize) {
    let scale = 1.0 / (1u32 << frac) as f32;
    match format {
        0 => (d[0] as f32 * scale, 1),
        1 => (d[0] as i8 as f32 * scale, 1),
        2 => (u16::from_be_bytes([d[0], d[1]]) as f32 * scale, 2),
        3 => (i16::from_be_bytes([d[0], d[1]]) as f32 * scale, 2),
        _ => (f32::from_be_bytes([d[0], d[1], d[2], d[3]]), 4),
    }
}

fn color(d: &[u8], format: u32) -> ([f32; 4], usize) {
    let n = |v: u32, bits: u32| v as f32 / ((1 << bits) - 1) as f32;
    let u16be = || u16::from_be_bytes([d[0], d[1]]) as u32;
    match format {
        0 => {
            let v = u16be();
            ([n(v >> 11, 5), n((v >> 5) & 0x3f, 6), n(v & 0x1f, 5), 1.0], 2)
        }
        1 => ([n(d[0] as u32, 8), n(d[1] as u32, 8), n(d[2] as u32, 8), 1.0], 3),
        2 => ([n(d[0] as u32, 8), n(d[1] as u32, 8), n(d[2] as u32, 8), 1.0], 4),
        3 => {
            let v = u16be();
            ([n(v >> 12, 4), n((v >> 8) & 0xf, 4), n((v >> 4) & 0xf, 4), n(v & 0xf, 4)], 2)
        }
        4 => {
            let v = (d[0] as u32) << 16 | (d[1] as u32) << 8 | d[2] as u32;
            ([n(v >> 18, 6), n((v >> 12) & 0x3f, 6), n((v >> 6) & 0x3f, 6), n(v & 0x3f, 6)], 3)
        }
        _ => ([n(d[0] as u32, 8), n(d[1] as u32, 8), n(d[2] as u32, 8), n(d[3] as u32, 8)], 4),
    }
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn normalize(a: [f32; 3]) -> [f32; 3] {
    let len = dot3(a, a).sqrt();
    if len > 1e-20 { scale(a, 1.0 / len) } else { a }
}

fn rgba8(v: u32) -> [f32; 4] {
    [(v >> 24) as f32 / 255.0, ((v >> 16) & 0xff) as f32 / 255.0, ((v >> 8) & 0xff) as f32 / 255.0, (v & 0xff) as f32 / 255.0]
}

/// A vertex as read from the stream, before transformation.
#[derive(Default, Clone, Copy)]
struct RawVertex {
    pos: [f32; 3],
    normal: [f32; 3],
    pos_mtx: u32,
    tex_mtx: [u32; 8],
    colors: [Option<[f32; 4]>; 2],
    tex: [[f32; 2]; 8],
}

/// Per-draw transform constants: the projection (XF 0x1020-0x1026), the
/// viewport (XF 0x101A-0x101F) and the scissor offset it is relative to.
pub(super) struct View {
    proj: [f32; 6],
    ortho: bool,
    vp: [f32; 6],
    offset: (f32, f32),
}

impl View {
    /// The cull mode as the renderer must apply it. The GPU tells front from
    /// back before the viewport, and the renderer after it: through a
    /// mirrored viewport (the game renders portraits into textures through
    /// one flipped vertically), front and back swap.
    fn cull(&self, mode: u32) -> u32 {
        let [wd, ht, ..] = self.vp;
        let mirrored = (wd < 0.0) != (ht > 0.0);
        match mode {
            1 | 2 if mirrored => 3 - mode,
            _ => mode,
        }
    }

    /// Whether the viewport spans the EFB's full width.
    fn full_width(&self) -> bool {
        let [wd, _, _, xorig, _, _] = self.vp;
        let left = xorig - self.offset.0 - wd;
        left.abs() < 1.0 && (2.0 * wd - EFB_WIDTH).abs() < 1.0
    }
}

/// Whether opcode `op` draws triangles (quads, lists, strips, fans), the
/// primitives drawn so far.
pub(super) fn is_triangles(op: u8) -> bool {
    matches!(op & 0xf8, 0x80 | 0x88 | 0x90 | 0x98 | 0xa0)
}

pub(super) fn xf_log_frame() -> Option<u64> {
    static AT: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *AT.get_or_init(|| std::env::var("RUMBLE_XF_LOG").ok().and_then(|v| v.parse().ok()))
}

/// BP register for texmap `n`: units 0-3 and 4-7 live in separate banks.
fn tex_reg(base_lo: usize, base_hi: usize, n: usize) -> usize {
    if n < 4 { base_lo + n } else { base_hi + n - 4 }
}

impl Gx {
    fn bits(v: u32, lo: u32, n: u32) -> u32 {
        (v >> lo) & ((1 << n) - 1)
    }

    /// Reads one vertex of format `fmt` from `d`, following indexed
    /// attributes into the vertex arrays in RAM.
    fn read_vertex<'a>(&self, ram: &'a Ram, fmt: usize, d: &'a [u8]) -> RawVertex {
        let b = Self::bits;
        let vcd_lo = self.cp[0x50];
        let vcd_hi = self.cp[0x60];
        let (va, vb, vc) = (self.cp[0x70 + fmt], self.cp[0x80 + fmt], self.cp[0x90 + fmt]);
        let mut v = RawVertex::default();
        let mut p = 0usize;
        // Default matrix indices when the vertex has none: the XF's copy of
        // MATINDEX (0x1018/0x1019), which is what the transform unit uses.
        // The CP's can hold something else: the podium on the ranking
        // screen loads its model's matrices into slot 0 and sets only the
        // XF's index to 0, leaving a stale 3 in the CP's.
        v.pos_mtx = self.xf[0x1018] & 0x3f;
        let (mat_a, mat_b) = (self.xf[0x1018], self.xf[0x1019]);
        for n in 0..8 {
            v.tex_mtx[n] = if n < 4 { b(mat_a, 6 + 6 * n as u32, 6) } else { b(mat_b, 6 * (n as u32 - 4), 6) };
        }
        if vcd_lo & 1 != 0 {
            v.pos_mtx = d[p] as u32 & 0x3f;
            p += 1;
        }
        for n in 0..8 {
            if vcd_lo & (2 << n) != 0 {
                v.tex_mtx[n] = d[p] as u32 & 0x3f;
                p += 1;
            }
        }

        // Where an attribute's data is: in the stream, or in an array.
        let source = |mode: u32, array: usize, direct: usize, p: &mut usize| -> Option<&'a [u8]> {
            match mode {
                1 => {
                    let s = &d[*p..*p + direct];
                    *p += direct;
                    Some(s)
                }
                2 | 3 => {
                    let idx = if mode == 2 {
                        *p += 1;
                        d[*p - 1] as u32
                    } else {
                        *p += 2;
                        u16::from_be_bytes([d[*p - 2], d[*p - 1]]) as u32
                    };
                    let addr = self.cp[0xa0 + array].wrapping_add(idx * self.cp[0xb0 + array]);
                    ram.bytes(addr, direct)
                }
                _ => None,
            }
        };
        let comp_size = |format: u32| match format {
            0 | 1 => 1,
            2 | 3 => 2,
            _ => 4,
        };

        // Position.
        let pos_n = 2 + b(va, 0, 1) as usize;
        let (pos_fmt, pos_frac) = (b(va, 1, 3), b(va, 4, 5));
        if let Some(s) = source(b(vcd_lo, 9, 2), 0, pos_n * comp_size(pos_fmt), &mut p) {
            let mut o = 0;
            for k in 0..pos_n {
                let (c, n) = component(&s[o..], pos_fmt, pos_frac);
                v.pos[k] = c;
                o += n;
            }
        }

        // Normal (plus binormal and tangent, which are skipped). Signed
        // fixed-point normals have a fixed scale: s8 /64, s16 /16384.
        let nbt = b(va, 9, 1) != 0;
        let normal_mode = b(vcd_lo, 11, 2);
        let normal_fmt = b(va, 10, 3);
        let normal_direct = if nbt { 9 } else { 3 } * comp_size(normal_fmt);
        if let Some(s) = source(normal_mode, 1, normal_direct, &mut p) {
            let frac = match normal_fmt {
                1 => 6,
                3 => 14,
                _ => 0,
            };
            let mut o = 0;
            for k in 0..3 {
                let (c, n) = component(&s[o..], normal_fmt, frac);
                v.normal[k] = c;
                o += n;
            }
        }
        if nbt && normal_mode >= 2 && b(va, 31, 1) != 0 {
            let _ = source(normal_mode, 1, normal_direct, &mut p);
            let _ = source(normal_mode, 1, normal_direct, &mut p);
        }

        // Colors.
        let color_size = |format: u32| match format {
            0 | 3 => 2,
            1 | 4 => 3,
            _ => 4,
        };
        for ch in 0..2 {
            let format = b(va, 14 + 4 * ch as u32, 3);
            if let Some(s) = source(b(vcd_lo, 13 + 2 * ch as u32, 2), 2 + ch, color_size(format), &mut p) {
                v.colors[ch] = Some(color(s, format).0);
            }
        }

        // Texture coordinates: (two-component flag, format, frac) per unit.
        let tex = [
            (b(va, 21, 1), b(va, 22, 3), b(va, 25, 5)),
            (b(vb, 0, 1), b(vb, 1, 3), b(vb, 4, 5)),
            (b(vb, 9, 1), b(vb, 10, 3), b(vb, 13, 5)),
            (b(vb, 18, 1), b(vb, 19, 3), b(vb, 22, 5)),
            (b(vb, 27, 1), b(vb, 28, 3), b(vc, 0, 5)),
            (b(vc, 5, 1), b(vc, 6, 3), b(vc, 9, 5)),
            (b(vc, 14, 1), b(vc, 15, 3), b(vc, 18, 5)),
            (b(vc, 23, 1), b(vc, 24, 3), b(vc, 27, 5)),
        ];
        for (n, (st, format, frac)) in tex.into_iter().enumerate() {
            let count = 1 + st as usize;
            if let Some(s) = source(b(vcd_hi, 2 * n as u32, 2), 4 + n, count * comp_size(format), &mut p) {
                let mut o = 0;
                for k in 0..count {
                    let (c, len) = component(&s[o..], format, frac);
                    v.tex[n][k] = c;
                    o += len;
                }
            }
        }
        v
    }

    /// Row `r` of the XF matrix starting at row index `idx`.
    fn mtx_row(&self, idx: u32, r: u32) -> [f32; 4] {
        let base = (idx * 4 + r * 4) as usize;
        let at = |k: usize| self.xf.get(base + k).copied().map_or(0.0, f);
        [at(0), at(1), at(2), at(3)]
    }

    pub(super) fn view(&self) -> View {
        View {
            proj: std::array::from_fn(|k| f(self.xf[0x1020 + k])),
            ortho: self.xf[0x1026] != 0,
            vp: std::array::from_fn(|k| f(self.xf[0x101a + k])),
            offset: self.screen_offset(),
        }
    }

    /// Fits a primitive's clip-space x to a widescreen EFB (`Draw::wide`).
    fn widen(&self, view: &View, wide: bool, verts: &mut [[f32; VERTEX_FLOATS]]) {
        let k = self.widescreen;
        // The game's full-screen cameras are made wide already
        // (`hle::mtx`): drawn full-width, they span the wider EFB as they
        // are, or rescaled to it if built for another width (the setting
        // changed, and the game has not rebuilt them yet). Through a
        // narrower viewport (the shadow map, drawn from the main camera)
        // they keep their place like any other draw.
        let pr = &view.proj;
        if wide && !view.ortho && pr[0] != 0.0 {
            if let Some(built) = crate::hle::mtx::widened_by(self, pr[2] / pr[0]) {
                if built != k {
                    for v in verts.iter_mut() {
                        v[0] *= built / k;
                    }
                }
                return;
            }
        }
        if k == 1.0 {
            return;
        }
        let fills = wide && view.ortho && {
            let xs = verts.iter().map(|v| v[0] / v[3]);
            let (lo, hi) = xs.fold((f32::MAX, f32::MIN), |(lo, hi), x| (lo.min(x), hi.max(x)));
            lo <= -0.99 && hi >= 0.99
        };
        if !fills {
            // HUD elements go to their side of the wider screen, as far
            // out as they were on the 4:3 one.
            let shift = if view.ortho { self.hud_anchor as f32 * (1.0 - 1.0 / k) } else { 0.0 };
            for v in verts.iter_mut() {
                v[0] = v[0] / k + shift * v[3];
            }
        }
    }

    fn transform(&self, view: &View, v: &RawVertex) -> [f32; VERTEX_FLOATS] {
        let dot = |r: [f32; 4], x: [f32; 4]| r[0] * x[0] + r[1] * x[1] + r[2] * x[2] + r[3] * x[3];
        let p = [v.pos[0], v.pos[1], v.pos[2], 1.0];
        let eye = [dot(self.mtx_row(v.pos_mtx, 0), p), dot(self.mtx_row(v.pos_mtx, 1), p), dot(self.mtx_row(v.pos_mtx, 2), p)];

        // Projection.
        let pr = &view.proj;
        let (cx, cy, cz, cw) = if !view.ortho {
            (pr[0] * eye[0] + pr[1] * eye[2], pr[2] * eye[1] + pr[3] * eye[2], pr[4] * eye[2] + pr[5], -eye[2])
        } else {
            (pr[0] * eye[0] + pr[1], pr[2] * eye[1] + pr[3], pr[4] * eye[2] + pr[5], 1.0)
        };

        // Viewport onto the EFB, kept in clip space so the host
        // interpolates perspective-correctly.
        let [wd, ht, zrange, xorig, yorig, farz] = view.vp;
        let (xoff, yoff) = view.offset;
        let x = 2.0 / EFB_WIDTH * (wd * cx + (xorig - xoff) * cw) - cw;
        let y = cw - 2.0 / EFB_HEIGHT * (ht * cy + (yorig - yoff) * cw);
        let z = (farz * cw + zrange * cz) / 16_777_216.0;

        let mut out = [0f32; VERTEX_FLOATS];
        out[..4].copy_from_slice(&[x, y, z, cw]);

        // Color channels: color and alpha lit separately (XF 0x100E and
        // 0x1010 control registers), against the view-space normal.
        let normal = self.view_normal(v);
        for ch in 0..2 {
            let vtx = v.colors[ch].unwrap_or([1.0; 4]);
            let (color_ctrl, alpha_ctrl) = (self.xf[0x100e + ch], self.xf[0x1010 + ch]);
            let c = self.light_channel(color_ctrl, ch, vtx, eye, normal);
            // Usually lit the same way as the color: no need to do it twice.
            let a = if alpha_ctrl == color_ctrl { c } else { self.light_channel(alpha_ctrl, ch, vtx, eye, normal) };
            out[4 + 4 * ch..8 + 4 * ch].copy_from_slice(&[c[0], c[1], c[2], a[3]]);
        }

        // Texture coordinate generation (XF 0x103F count, 0x1040+ config).
        let count = (self.xf[0x103f] as usize).min(8);
        for n in 0..8 {
            let mut tc = [0.0, 0.0, 1.0];
            if n < count {
                let tg = self.xf[0x1040 + n];
                let (proj, form, kind, row) = (Self::bits(tg, 1, 1), Self::bits(tg, 2, 1), Self::bits(tg, 4, 3), Self::bits(tg, 7, 5));
                match kind {
                    0 => {
                        // Source rows: model-space position, view-space
                        // normal, color 0, (binormals, not read), texture
                        // coordinates 0-7. AB11 takes two components, ABC1
                        // three.
                        let src = match row {
                            0 => v.pos,
                            1 => normal,
                            2 => {
                                let c = v.colors[0].unwrap_or([1.0; 4]);
                                [c[0], c[1], c[2]]
                            }
                            5..=12 => {
                                let t = v.tex[(row - 5) as usize];
                                [t[0], t[1], 1.0]
                            }
                            _ => [0.0, 0.0, 1.0],
                        };
                        let input = [src[0], src[1], if form == 1 { src[2] } else { 1.0 }, 1.0];
                        let m = v.tex_mtx[n];
                        tc = [
                            dot(self.mtx_row(m, 0), input),
                            dot(self.mtx_row(m, 1), input),
                            if proj == 1 { dot(self.mtx_row(m, 2), input) } else { 1.0 },
                        ];
                        // Dual texture transform (XF 0x1012): optionally
                        // normalize, then a post matrix (XF 0x500 + 4 * index,
                        // config in XF 0x1050 + n). Environment maps use it.
                        if self.xf[0x1012] & 1 != 0 {
                            let post = self.xf[0x1050 + n];
                            let mut c = tc;
                            if post & (1 << 8) != 0 {
                                c = normalize(c);
                            }
                            let base = 0x500 + (post & 0x3f) as usize * 4;
                            let row = |r: usize| std::array::from_fn::<f32, 4, _>(|k| f(self.xf[base + r * 4 + k]));
                            let x = [c[0], c[1], c[2], 1.0];
                            tc = [dot(row(0), x), dot(row(1), x), dot(row(2), x)];
                        }
                    }
                    2 | 3 => {
                        let c = &out[4 + 4 * (kind as usize - 2)..];
                        tc = [c[0], c[1], 1.0];
                    }
                    _ => {}
                }
            }
            out[12 + 3 * n..15 + 3 * n].copy_from_slice(&tc);
        }
        out
    }

    /// The vertex normal in view space: the normal matrix paired with the
    /// position matrix (XF 0x400 + 3 * index), normalized.
    fn view_normal(&self, v: &RawVertex) -> [f32; 3] {
        let base = 0x400 + (v.pos_mtx as usize % 32) * 3;
        let row = |r: usize| std::array::from_fn::<f32, 3, _>(|c| f(self.xf[base + r * 3 + c]));
        let n = v.normal;
        let t = [0, 1, 2].map(|r| {
            let m = row(r);
            m[0] * n[0] + m[1] * n[1] + m[2] * n[2]
        });
        normalize(t)
    }

    /// One GX lighting channel (color or alpha half) for a vertex:
    /// material * clamp(ambient + sum of enabled lights), or just the
    /// material when lighting is off. `ctrl` is the channel's XF control
    /// register; `pos` and `normal` are in view space.
    fn light_channel(&self, ctrl: u32, ch: usize, vtx: [f32; 4], pos: [f32; 3], normal: [f32; 3]) -> [f32; 4] {
        let pick = |vertex: bool, reg: u32| if vertex { vtx } else { rgba8(reg) };
        let mat = pick(ctrl & 1 != 0, self.xf[0x100c + ch]);
        if ctrl & 2 == 0 {
            return mat;
        }
        let mut acc = pick(ctrl & (1 << 6) != 0, self.xf[0x100a + ch]);
        let lights = ((ctrl >> 2) & 0xf) | ((ctrl >> 11) & 0xf) << 4;
        let diffuse = (ctrl >> 7) & 3;
        let (attn_select, attn_enable) = (ctrl & (1 << 9) != 0, ctrl & (1 << 10) != 0);
        for n in 0..8 {
            if lights & (1 << n) == 0 {
                continue;
            }
            let base = 0x600 + n * 16;
            let word = |k: usize| f(self.xf[base + k]);
            let color = rgba8(self.xf[base + 3]);
            let a = [word(4), word(5), word(6)];
            let k = [word(7), word(8), word(9)];
            let lpos = [word(10), word(11), word(12)];
            let ldir = [word(13), word(14), word(15)];

            let (to_light, attn) = if !attn_enable {
                (normalize(sub(lpos, pos)), 1.0)
            } else if attn_select {
                // Spot: angle and distance polynomials.
                let d = sub(lpos, pos);
                let dist2 = dot3(d, d);
                let dist = dist2.sqrt();
                let to_light = scale(d, 1.0 / dist.max(1e-20));
                let cos = dot3(to_light, ldir).max(0.0);
                let num = (a[0] + a[1] * cos + a[2] * cos * cos).max(0.0);
                let den = k[0] + k[1] * dist + k[2] * dist2;
                (to_light, if den.abs() > 1e-20 { num / den } else { 0.0 })
            } else {
                // Specular: the light "position" is a direction and "dir"
                // the half-angle vector.
                let to_light = normalize(lpos);
                let cos = if dot3(normal, to_light) >= 0.0 { dot3(normal, normalize(ldir)).max(0.0) } else { 0.0 };
                let num = (a[0] + a[1] * cos + a[2] * cos * cos).max(0.0);
                let den = k[0] + k[1] * cos + k[2] * cos * cos;
                (to_light, if den.abs() > 1e-20 { num / den } else { 0.0 })
            };
            let d = match diffuse {
                0 => 1.0,
                1 => dot3(to_light, normal),
                _ => dot3(to_light, normal).max(0.0),
            };
            for c in 0..4 {
                acc[c] += color[c] * d * attn;
            }
        }
        std::array::from_fn(|c| mat[c] * acc[c].clamp(0.0, 1.0))
    }

    /// The scissor offset (BP 0x59) the viewport and scissor are relative
    /// to, normally 342 in both axes.
    fn screen_offset(&self) -> (f32, f32) {
        let s = self.bp[0x59];
        if s == 0 {
            return (342.0, 342.0);
        }
        ((s & 0x3ff) as f32 * 2.0, ((s >> 10) & 0x3ff) as f32 * 2.0)
    }

    fn scissor(&self) -> [u32; 4] {
        let (xoff, yoff) = self.screen_offset();
        let (tl, br) = (self.bp[0x20], self.bp[0x21]);
        let x0 = ((tl >> 12) & 0x7ff) as f32 - xoff;
        let y0 = (tl & 0x7ff) as f32 - yoff;
        let x1 = ((br >> 12) & 0x7ff) as f32 - xoff + 1.0;
        let y1 = (br & 0x7ff) as f32 - yoff + 1.0;
        let clamp = |v: f32, max: f32| v.clamp(0.0, max) as u32;
        let (x0, y0) = (clamp(x0, EFB_WIDTH), clamp(y0, EFB_HEIGHT));
        let (x1, y1) = (clamp(x1, EFB_WIDTH), clamp(y1, EFB_HEIGHT));
        [x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0)]
    }

    fn tev_words(&self) -> [u32; TEV_WORDS] {
        let mut w = [0u32; TEV_WORDS];
        let stages = (Self::bits(self.bp[0x00], 10, 4) + 1) as usize;
        for i in 0..stages {
            let tref = (self.bp[0x28 + i / 2] >> (12 * (i as u32 & 1))) & 0x3ff;
            let ksel = self.bp[0xf6 + i / 2];
            let (kc, ka) = if i & 1 == 0 {
                (Self::bits(ksel, 4, 5), Self::bits(ksel, 9, 5))
            } else {
                (Self::bits(ksel, 14, 5), Self::bits(ksel, 19, 5))
            };
            w[4 * i] = self.bp[0xc0 + 2 * i];
            w[4 * i + 1] = self.bp[0xc1 + 2 * i];
            w[4 * i + 2] = tref;
            w[4 * i + 3] = kc | ka << 8;
        }
        for r in 0..4 {
            for c in 0..4 {
                w[64 + 4 * r + c] = self.tev_regs[r][c].to_bits();
                w[80 + 4 * r + c] = self.konst[r][c].to_bits();
            }
        }
        w[96] = stages as u32;
        w[97] = self.bp[0xf3];
        w
    }

    /// Texture bound to texmap `n`. Its source is hashed once per frame
    /// and decoded only when that content hasn't been seen recently.
    fn texture(&mut self, ram: &Ram, n: usize) -> Option<TexBinding> {
        let mode0 = self.bp[tex_reg(0x80, 0xa0, n)];
        let image0 = self.bp[tex_reg(0x88, 0xa8, n)];
        let image3 = self.bp[tex_reg(0x94, 0xb4, n)];
        let tlut = self.bp[tex_reg(0x98, 0xb8, n)];
        let key = TextureKey {
            addr: (image3 & 0x00ff_ffff) << 5,
            width: (image0 & 0x3ff) + 1,
            height: ((image0 >> 10) & 0x3ff) + 1,
            format: (image0 >> 20) & 0xf,
            tlut_offset: (tlut & 0x3ff) << 9,
            tlut_format: (tlut >> 10) & 3,
        };
        let texture = match self.frame_textures.get(&key) {
            Some(t) => t.clone(),
            None => {
                self.forget_overwritten_copies(ram, key.addr);
                let frame = self.stats.xfb_copies;
                let t = texture::source(ram, &self.tmem, &key).and_then(|src| match self.decoded.get_mut(&src.id) {
                    Some((t, used)) => {
                        *used = frame;
                        Some(t.clone())
                    }
                    None => {
                        let t = Arc::new(texture::decode_source(&src, &key)?);
                        self.decoded.insert(src.id, (t.clone(), frame));
                        Some(t)
                    }
                });
                if t.is_none() {
                    self.stats.bad_textures += 1;
                }
                self.frame_textures.insert(key, t.clone());
                t
            }
        }?;
        Some(TexBinding { texture, addr: key.addr, wrap_s: mode0 & 3, wrap_t: (mode0 >> 2) & 3, linear: mode0 & 0x10 != 0 })
    }

    /// Notes what the game-side hooks (`hle::mtx`) learn from a draw through
    /// `view`: the widescreen factors drawn with, the last full-width
    /// camera's projection and the shadow map's. Returns whether the draw
    /// is full-width (`Draw::wide`).
    pub(super) fn note_view(&mut self, view: &View) -> bool {
        let wide = view.full_width();
        if !self.widescreen_history.contains(&self.widescreen) {
            if self.widescreen_history.len() >= 4 {
                self.widescreen_history.remove(0);
            }
            self.widescreen_history.push(self.widescreen);
        }
        if wide && !view.ortho {
            self.full_width_proj = Some(view.proj);
        }
        // The shadow map: drawn top-down, orthographic, into 512x512.
        if view.ortho && (view.vp[0] - 256.0).abs() < 0.5 && (view.vp[1] + 256.0).abs() < 0.5 {
            self.shadow_proj = Some(view.proj);
        }
        wide
    }

    /// Records a primitive (opcode `op`, `count` vertices in `d`) as a
    /// triangle list, appended to the previous draw when the state matches.
    pub(super) fn record_draw(&mut self, ram: &Ram, op: u8, count: usize, d: &[u8], vsize: usize) {
        let kind = op & 0xf8;
        if !is_triangles(op) {
            self.stats.skipped_lines_points += 1;
            return;
        }
        let fmt = (op & 7) as usize;
        let mut verts = std::mem::take(&mut self.scratch_vertices);
        verts.clear();
        let view = self.view();
        verts.extend((0..count).map(|k| self.transform(&view, &self.read_vertex(ram, fmt, &d[k * vsize..]))));
        // With `RUMBLE_XF_LOG=FRAME`: the transform of each perspective draw
        // in that frame, for finding where a model's placement goes wrong.
        if xf_log_frame() == Some(self.stats.xfb_copies) && !view.ortho {
            let raw = self.read_vertex(ram, fmt, d);
            let rows: Vec<[f32; 4]> = (0..3).map(|r| self.mtx_row(raw.pos_mtx, r)).collect();
            let w = verts.iter().map(|v| v[3]).fold(f32::MAX, f32::min);
            println!("XF {count} verts, cp idx {:#x} xf idx {:#x} vcd {:#x}, min w {w:.2}, pos mtx {} rows {rows:?}, first pos {:?}, proj {:?}", self.cp[0x30], self.xf[0x1018], self.cp[0x50], raw.pos_mtx, raw.pos, view.proj);
        }
        let wide = self.note_view(&view);
        self.widen(&view, wide, &mut verts);

        // With no BP write since the last draw, its state still holds.
        let continues = !self.draw_state_changed
            && matches!(self.commands.last(), Some(GpuCommand::Draw(last)) if last.wide == wide);
        if !continues {
            self.draw_state_changed = false;
            let tev = self.tev_words();
            let mut textures: [Option<TexBinding>; 8] = Default::default();
            self.shadow_coords = 0;
            for i in 0..tev[96] as usize {
                let tref = tev[4 * i + 2];
                if tref & 0x40 != 0 {
                    let map = (tref & 7) as usize;
                    if textures[map].is_none() {
                        textures[map] = self.texture(ram, map);
                    }
                    if textures[map].as_ref().is_some_and(|t| self.shadow_maps.contains(&t.addr)) {
                        self.shadow_coords |= 1 << ((tref >> 3) & 7);
                    }
                }
            }
            let (blend, zmode, scissor) = (self.bp[0x41], self.bp[0x40], self.scissor());
            let cull = view.cull(Self::bits(self.bp[0x00], 14, 2));
            let same = matches!(self.commands.last(), Some(GpuCommand::Draw(last))
                if last.tev == tev && last.blend == blend && last.zmode == zmode && last.cull == cull
                    && last.scissor == scissor && last.wide == wide && last.textures == textures);
            if !same {
                self.commands.push(GpuCommand::Draw(Draw { vertices: Vec::new(), textures, tev, blend, zmode, cull, scissor, wide }));
            }
        }
        // The shadow map covers a light patch widened for widescreen
        // (`hle::mtx`); the floor's coordinates into it are narrowed to
        // match, about its middle.
        if self.shadow_coords != 0 {
            let k = self.widescreen;
            for v in verts.iter_mut() {
                for n in (0..8).filter(|n| self.shadow_coords & 1 << n != 0) {
                    let (s, q) = (v[12 + 3 * n], v[14 + 3 * n]);
                    v[12 + 3 * n] = 0.5 * q + (s - 0.5 * q) / k;
                }
            }
        }
        let Some(GpuCommand::Draw(draw)) = self.commands.last_mut() else { unreachable!() };
        let out = &mut draw.vertices;
        let mut put = |i: usize| out.extend_from_slice(&verts[i]);
        match kind {
            0x80 | 0x88 => {
                for q in 0..count / 4 {
                    for i in [0, 1, 2, 0, 2, 3] {
                        put(4 * q + i);
                    }
                }
            }
            0x90 => (0..count / 3 * 3).for_each(&mut put),
            // Every other strip triangle is flipped back to the strip's
            // winding.
            0x98 => {
                for i in 0..count.saturating_sub(2) {
                    let tri = if i % 2 == 0 { [i, i + 1, i + 2] } else { [i + 1, i, i + 2] };
                    tri.into_iter().for_each(&mut put);
                }
            }
            _ => {
                for i in 1..count.saturating_sub(1) {
                    [0, i, i + 1].into_iter().for_each(&mut put);
                }
            }
        }
        self.scratch_vertices = verts;
    }

    /// Records an EFB copy (BP 0x52); an XFB copy ends the frame.
    /// Tells the renderer to forget the copies covering `addr` whose RAM has
    /// changed since they were made: the game has put something else there
    /// (a texture loaded into a freed copy buffer), and a texture there
    /// must read RAM again rather than the old copy.
    fn forget_overwritten_copies(&mut self, ram: &Ram, addr: u32) {
        let commands = &mut self.commands;
        self.copies.retain(|&(span, hash)| {
            if span.contains(addr) && span_hash(ram, span) != Some(hash) {
                commands.push(GpuCommand::Forget(span.start));
                return false;
            }
            true
        });
    }

    pub(super) fn record_copy(&mut self, control: u32, ram: &Ram) {
        let (src, size) = (self.bp[0x49], self.bp[0x4a]);
        let rect = [src & 0x3ff, (src >> 10) & 0x3ff, (size & 0x3ff) + 1, ((size >> 10) & 0x3ff) + 1];
        let clear = (control & (1 << 11) != 0).then(|| {
            let (ar, gb) = (self.bp[0x4f], self.bp[0x50]);
            let c = |v: u32| (v & 0xff) as f32 / 255.0;
            ([c(ar), c(gb >> 8), c(gb), c(ar >> 8)], (self.bp[0x51] & 0xff_ffff) as f32 / 16_777_216.0)
        });
        let to_xfb = control & (1 << 14) != 0;
        let dest = (self.bp[0x4b] & 0x00ff_ffff) << 5;
        let half = control & (1 << 9) != 0;
        // Bits 3-6, stored with the halves of the format table swapped: bit
        // 3 selects formats 8-15, bits 4-6 the format within the half.
        let format = ((control >> 4) & 7) | ((control >> 3) & 1) << 3;
        let intensity = control & (1 << 15) != 0;
        let efb_alpha = self.bp[0x43] & 7 == 1;
        let stride = (self.bp[0x4d] & 0x3ff) * 32;
        let copy = EfbCopy { to_xfb, rect, clear, dest, half, format, intensity, efb_alpha, stride };
        // A shadow map (A8) drawn from a widened light: what samples it is
        // narrowed to match (`record_draw`).
        if std::mem::take(&mut self.light_widened) && format == 7 {
            self.shadow_maps.retain(|&d| d != dest);
            if self.shadow_maps.len() >= 4 {
                self.shadow_maps.remove(0);
            }
            self.shadow_maps.push(dest);
        } else if !to_xfb {
            self.shadow_maps.retain(|&d| d != dest);
        }
        if !to_xfb {
            // What RAM holds under the copy now, to notice it being
            // overwritten later (`forget_overwritten_copies`).
            let span = copy.span();
            self.copies.retain(|(s, _)| s.start != dest);
            if let Some(hash) = span_hash(ram, span) {
                self.copies.push((span, hash));
            }
        }
        self.commands.push(GpuCommand::Copy(copy));
        if to_xfb {
            let frame = std::mem::take(&mut self.commands);
            self.frame_textures.clear();
            let now = self.stats.xfb_copies;
            self.decoded.retain(|_, (_, used)| now - *used < DECODED_IDLE_FRAMES);
            if self.frames.len() >= MAX_QUEUED_FRAMES {
                self.frames.pop_front();
            }
            self.frames.push_back(frame);
        }
    }
}

/// Frames waiting for a renderer; older ones are dropped beyond this.
pub(super) const MAX_QUEUED_FRAMES: usize = 16;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Memory;

    /// Icons side by side in one sheet (a stride wider than each) don't
    /// overlap; one copied over another's rows does.
    #[test]
    fn copies_in_a_sheet_are_apart() {
        // 64x64 RGB565 icons (4x4 tiles of 32 bytes: 512 bytes a row of
        // tiles), six to a 3072-byte row.
        let icon = |dest| CopySpan::new(dest, 4, (64, 64), 3072);
        let (a, b) = (icon(0x1000), icon(0x1000 + 512));
        assert_eq!((a.rows, a.used, a.stride), (16, 512, 3072));
        assert!(!a.overlaps(&b) && !b.overlaps(&a));
        assert!(a.contains(0x1000 + 3072) && !a.contains(0x1000 + 512));
        assert!(a.overlaps(&icon(0x1000 + 3072 * 4)));
        assert!(a.overlaps(&CopySpan::new(0x1000 + 100, 4, (64, 64), 0)));
    }

    /// With an identity matrix and orthographic identity projection, the
    /// viewport maps (1, 1) to the EFB's right/top edge: clip (1, 1).
    #[test]
    fn viewport_maps_to_efb_clip_space() {
        let mut gx = Gx::default();
        for r in 0..3 {
            gx.xf[r * 4 + r] = 1f32.to_bits();
        }
        gx.xf[0x1020..0x1027].copy_from_slice(&[1f32, 0.0, 1.0, 0.0, 1.0, 0.0, 0.0].map(f32::to_bits));
        gx.xf[0x1026] = 1; // orthographic
        let viewport = [320.0, -264.0, 16_777_215.0, 342.0 + 320.0, 342.0 + 264.0, 16_777_215.0];
        gx.xf[0x101a..0x1020].copy_from_slice(&viewport.map(f32::to_bits));
        let v = RawVertex { pos: [1.0, 1.0, 0.0], ..Default::default() };
        let out = gx.transform(&gx.view(), &v);
        assert!((out[0] - 1.0).abs() < 1e-5 && (out[1] - 1.0).abs() < 1e-5, "{:?}", &out[..4]);
        assert!((out[2] - 1.0).abs() < 1e-5 && out[3] == 1.0, "{:?}", &out[..4]);

        let origin = gx.transform(&gx.view(), &RawVertex { pos: [-1.0, -1.0, 0.0], ..Default::default() });
        assert!((origin[0] + 1.0).abs() < 1e-5 && (origin[1] + 1.0).abs() < 1e-5, "{:?}", &origin[..4]);
    }

    /// Ambient plus one directional-style light (no attenuation, clamped
    /// diffuse) on a white register material.
    #[test]
    fn lighting_ambient_plus_diffuse() {
        let mut gx = Gx::default();
        gx.xf[0x100c] = 0xffff_ffff; // material white
        gx.xf[0x100a] = 0x4040_40ff; // ambient 0.25
        // Lighting on, light 0, clamped diffuse, no attenuation.
        let ctrl = 1 << 1 | 1 << 2 | 2 << 7;
        let light = 0x600;
        gx.xf[light + 3] = 0x8080_80ff; // light color 0.5
        for (k, v) in [0.0f32, 0.0, 10.0].iter().enumerate() {
            gx.xf[light + 10 + k] = v.to_bits(); // in front, along +z
        }
        let lit = |n: [f32; 3]| gx.light_channel(ctrl, 0, [1.0; 4], [0.0; 3], n);
        let facing = lit([0.0, 0.0, 1.0]);
        let away = lit([0.0, 0.0, -1.0]);
        assert!((facing[0] - (0.25 + 0.5)).abs() < 0.01, "{facing:?}");
        assert!((away[0] - 0.25).abs() < 0.01, "{away:?}");
        // Lighting off: just the material.
        assert_eq!(gx.light_channel(0, 0, [1.0; 4], [0.0; 3], [0.0; 3]), [1.0; 4]);
    }

    /// Without per-vertex indices, the matrices come from the XF's MATINDEX,
    /// not the CP's.
    #[test]
    fn default_matrix_indices() {
        let mut gx = Gx::default();
        gx.cp[0x30] = 3 | 60 << 6; // CP: position 3, tex0 60
        gx.xf[0x1018] = 6 | 36 << 6; // XF: position 6, tex0 36
        gx.xf[0x1019] = 42; // XF: tex4 42
        let v = gx.read_vertex(&Memory::default().ram(), 0, &[]);
        assert_eq!((v.pos_mtx, v.tex_mtx[0], v.tex_mtx[4]), (6, 36, 42));
    }
}