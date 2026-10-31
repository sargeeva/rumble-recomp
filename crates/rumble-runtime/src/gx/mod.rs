//! The graphics pipeline's front end: the CPU's write-gather pipe at
//! 0xCC008000, the FIFO it fills in RAM (PI registers 0xCC00300C-0x3017),
//! the command processor (CP, 0xCC000000) and the pixel engine's status
//! registers (PE, 0xCC001000).
//!
//! Stores to the gather pipe collect into 32-byte bursts, which go to RAM at
//! the FIFO write pointer. While the CP is linked to that FIFO and reading,
//! the GPU takes each burst straight away: commands are decoded and their
//! register writes recorded. When the CP is unlinked (the SDK recording a
//! display list), bursts only go to RAM.
//!
//! With `capture` on, primitives and EFB copies also become draw commands
//! (`draw.rs`), grouped into frames at each copy to the external
//! framebuffer, for a host renderer to take with [`Gx::take_frames`]. They
//! can be built on a thread of their own (`worker.rs`).

pub mod draw;
pub mod texture;
mod worker;

use crate::mem::Ram;
use draw::GpuCommand;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use texture::{Texture, TextureKey};

/// CP control register (0xCC000002) bits.
const CP_READ_ENABLE: u16 = 1 << 0;
const CP_LINK_ENABLE: u16 = 1 << 4;
/// CP status: FIFO read and command processing idle.
const CP_IDLE: u16 = (1 << 2) | (1 << 3);

/// PE control (0xCC00100A): interrupt enables and write-1-to-clear status.
const PE_TOKEN_ENABLE: u16 = 1 << 0;
const PE_FINISH_ENABLE: u16 = 1 << 1;
const PE_TOKEN_INT: u16 = 1 << 2;
const PE_FINISH_INT: u16 = 1 << 3;

/// BP registers with side effects.
const BP_COPY: u8 = 0x52;
const BP_LOAD_TLUT0: u8 = 0x64;
const BP_LOAD_TLUT1: u8 = 0x65;
const BP_DRAW_DONE: u8 = 0x45;
const BP_TOKEN: u8 = 0x47;
const BP_TOKEN_INT: u8 = 0x48;
const BP_MASK: u8 = 0xfe;
/// EFB copy: destination is the external framebuffer (a finished frame).
const COPY_TO_XFB: u32 = 1 << 14;

/// CP registers describing vertices.
const CP_VCD_LO: usize = 0x50;
const CP_VCD_HI: usize = 0x60;
const CP_VAT_A: usize = 0x70;
const CP_VAT_B: usize = 0x80;
const CP_VAT_C: usize = 0x90;

/// What the decoder has seen, for the `boot` report.
#[derive(Default, Debug)]
pub struct GxStats {
    pub bursts: u64,
    pub commands: u64,
    pub draws: u64,
    pub vertices: u64,
    pub display_lists: u64,
    pub xfb_copies: u64,
    pub draw_done: u64,
    pub tokens: u64,
    /// Bytes skipped because they were not a command, and the first one.
    pub bad_bytes: u64,
    pub first_bad: Option<(u8, u64)>,
    /// Textures that could not be decoded (unknown format, outside RAM).
    pub bad_textures: u64,
    /// Line and point primitives, which are not drawn yet.
    pub skipped_lines_points: u64,
    /// Time spent building draw commands (with capture on).
    pub draw_building: std::time::Duration,
}

/// A 32-byte burst leaving the gather pipe.
pub struct Burst {
    pub data: [u8; 32],
    /// Where it goes in the FIFO in RAM, if a FIFO is set up.
    pub ram: Option<u32>,
    /// Whether the GPU executes it (the CP is linked to the CPU FIFO).
    pub execute: bool,
}

enum Parsed {
    /// More bytes are needed for the next command.
    Incomplete,
    /// A command of this many bytes was executed.
    Done(usize),
}

/// TMEM: texture memory on the GPU. Only TLUTs (palettes) are loaded into
/// it; textures are read straight from RAM.
const TMEM_SIZE: usize = 1 << 20;

pub struct Gx {
    gather: Vec<u8>,
    /// CPU FIFO in RAM (physical addresses) and its wrap flag.
    pi_base: u32,
    pi_end: u32,
    pi_wptr: u32,
    pi_wrapped: bool,
    cp_regs: [u16; 0x40],
    pe_regs: [u16; 0x40],
    /// GPU register state the stream sets up.
    cp: [u32; 0x100],
    bp: [u32; 0x100],
    bp_mask: u32,
    xf: Vec<u32>,
    tmem: Vec<u8>,
    /// TEV color registers (PREV, C0-C2) and konst colors, RGBA.
    tev_regs: [[f32; 4]; 4],
    konst: [[f32; 4]; 4],
    /// FIFO bytes not yet decoded, and how far decoding has got.
    pending: Vec<u8>,
    pending_pos: usize,
    /// Whether to record draw commands for a renderer.
    pub capture: bool,
    /// How much wider than 4:3 the renderer's EFB is (1 for 4:3); see
    /// `draw::Draw::wide`.
    pub widescreen: f32,
    /// The projection (XF 0x1020-0x1025) of the last full-width perspective
    /// draw, and the cameras widened for it with the aspect ratio they ask
    /// for (`hle::mtx`).
    pub full_width_proj: Option<[f32; 6]>,
    pub wide_cameras: Vec<(u32, f32)>,
    /// The widescreen factors drawn with lately, to recognise a camera
    /// built for an earlier one.
    pub widescreen_history: Vec<f32>,
    /// The projection of the last shadow-map draw (orthographic, 512x512),
    /// the size (width, height) of light extents widened for it, whether
    /// one was since the last copy, and the shadow maps (copy addresses)
    /// drawn with a widened light (`hle::mtx`, `record_draw`).
    pub shadow_proj: Option<[f32; 6]>,
    pub wide_lights: Vec<[f32; 2]>,
    light_widened: bool,
    /// HUD layouts (`hle::lyt`): each one's address and the screen side its
    /// draws are moved to with widescreen (-1 left, 1 right).
    pub hud_layouts: Vec<(u32, i8)>,
    /// The side the draws being decoded belong to (0: none).
    hud_anchor: i8,
    /// Changes waiting for the decoder to reach the FIFO position they were
    /// made at, as the gather pipe holds back the last few bytes written.
    changes: VecDeque<(u64, worker::Change)>,
    /// FIFO bytes queued for decoding and decoded, from the start.
    fifo_queued: u64,
    fifo_decoded: u64,
    shadow_maps: Vec<u32>,
    /// Texture coordinates of the draw being recorded that sample one.
    shadow_coords: u8,
    commands: Vec<GpuCommand>,
    frames: VecDeque<Vec<GpuCommand>>,
    frame_textures: HashMap<TextureKey, Option<Arc<Texture>>>,
    /// Decoded textures by content hash, kept across frames, with the
    /// frame each was last used in.
    decoded: HashMap<u64, (Arc<Texture>, u64)>,
    /// EFB copies to RAM the renderer holds, with a hash of the RAM under
    /// them when made.
    copies: Vec<(draw::CopySpan, u64)>,
    /// Reused buffer for a primitive's transformed vertices.
    scratch_vertices: Vec<[f32; draw::VERTEX_FLOATS]>,
    /// Whether a BP register (all a draw's state but its vertices comes
    /// from them, TMEM included) was written since the last draw.
    draw_state_changed: bool,
    pub stats: GxStats,
    /// The thread building draws instead, if there is one.
    worker: Option<worker::Worker>,
}

impl Default for Gx {
    fn default() -> Self {
        Self {
            gather: Vec::with_capacity(64),
            pi_base: 0,
            pi_end: 0,
            pi_wptr: 0,
            pi_wrapped: false,
            cp_regs: [0; 0x40],
            pe_regs: [0; 0x40],
            cp: [0; 0x100],
            bp: [0; 0x100],
            bp_mask: 0x00ff_ffff,
            xf: vec![0; 0x1100],
            tmem: vec![0; TMEM_SIZE],
            tev_regs: [[0.0; 4]; 4],
            konst: [[0.0; 4]; 4],
            pending: Vec::new(),
            pending_pos: 0,
            capture: false,
            widescreen: 1.0,
            full_width_proj: None,
            wide_cameras: Vec::new(),
            widescreen_history: Vec::new(),
            shadow_proj: None,
            wide_lights: Vec::new(),
            light_widened: false,
            hud_layouts: Vec::new(),
            hud_anchor: 0,
            changes: VecDeque::new(),
            fifo_queued: 0,
            fifo_decoded: 0,
            shadow_maps: Vec::new(),
            shadow_coords: 0,
            commands: Vec::new(),
            frames: VecDeque::new(),
            frame_textures: HashMap::new(),
            decoded: HashMap::new(),
            copies: Vec::new(),
            scratch_vertices: Vec::new(),
            draw_state_changed: true,
            stats: GxStats::default(),
            worker: None,
        }
    }
}

fn be32(d: &[u8]) -> u32 {
    u32::from_be_bytes(d[..4].try_into().unwrap())
}

impl Gx {
    // --- registers -------------------------------------------------------

    /// `off` is the offset from 0xCC000000.
    pub fn cp_read16(&self, off: u32) -> u16 {
        match off {
            0x00 => CP_IDLE,
            // Read-write distance: the GPU keeps up, so the FIFO is empty.
            0x30 | 0x32 => 0,
            // Read pointer = write pointer.
            0x38 | 0x3a => self.cp_regs[(off as usize - 4) / 2],
            _ => self.cp_regs.get(off as usize / 2).copied().unwrap_or(0),
        }
    }

    pub fn cp_write16(&mut self, off: u32, v: u16) {
        if let Some(r) = self.cp_regs.get_mut(off as usize / 2) {
            *r = v;
        }
    }

    /// `off` is the offset from 0xCC001000.
    pub fn pe_read16(&self, off: u32) -> u16 {
        self.pe_regs.get(off as usize / 2).copied().unwrap_or(0)
    }

    pub fn pe_write16(&mut self, off: u32, v: u16) {
        if off == 0x0a {
            let status = self.pe_regs[5] & (PE_TOKEN_INT | PE_FINISH_INT) & !v;
            self.pe_regs[5] = status | (v & (PE_TOKEN_ENABLE | PE_FINISH_ENABLE));
        } else if let Some(r) = self.pe_regs.get_mut(off as usize / 2) {
            *r = v;
        }
    }

    /// `off` is the offset from 0xCC003000 (0x0C base, 0x10 end, 0x14
    /// write pointer).
    pub fn pi_read32(&self, off: u32) -> u32 {
        match off {
            0x0c => self.pi_base,
            0x10 => self.pi_end,
            0x14 => self.pi_wptr | if self.pi_wrapped { 1 << 29 } else { 0 },
            _ => 0,
        }
    }

    pub fn pi_write32(&mut self, off: u32, v: u32) {
        let addr = v & 0x1fff_ffe0;
        match off {
            0x0c => self.pi_base = addr,
            0x10 => self.pi_end = addr,
            0x14 => {
                self.pi_wptr = addr;
                self.pi_wrapped = false;
            }
            _ => {}
        }
    }

    /// PE token and finish interrupts (PI causes 0x200 and 0x400).
    pub fn pe_token_irq(&self) -> bool {
        self.pe_regs[5] & (PE_TOKEN_INT | PE_TOKEN_ENABLE) == PE_TOKEN_INT | PE_TOKEN_ENABLE
    }

    pub fn pe_finish_irq(&self) -> bool {
        self.pe_regs[5] & (PE_FINISH_INT | PE_FINISH_ENABLE) == PE_FINISH_INT | PE_FINISH_ENABLE
    }

    /// Finished frames (command lists ending in an XFB copy), oldest first.
    pub fn take_frames(&mut self) -> Vec<Vec<GpuCommand>> {
        match &self.worker {
            Some(w) => w.frames(),
            None => self.frames.drain(..).collect(),
        }
    }

    /// Builds draws on a thread of their own from now on, reading `ram`
    /// (see `worker.rs`). For a `Gx` that has decoded nothing yet.
    pub(crate) fn start_worker(&mut self, ram: Ram<'static>) {
        let gx = Gx { capture: true, widescreen: self.widescreen, wide_cameras: self.wide_cameras.clone(), ..Gx::default() };
        self.worker = Some(worker::Worker::start(gx, ram));
    }

    /// Stops the worker, if there is one.
    pub(crate) fn stop_worker(&mut self) {
        self.worker = None;
    }

    /// Whether draws are built here (not on a worker).
    fn builds_draws(&self) -> bool {
        self.capture && self.worker.is_none()
    }

    /// The next shadow map is drawn from a widened light (`hle::mtx`).
    pub fn shadow_light_widened(&mut self) {
        let at = self.fifo_queued + self.gather.len() as u64;
        match &self.worker {
            Some(w) => w.change(at, worker::Change::LightWidened),
            None => self.light_widened = true,
        }
    }

    // --- gather pipe and FIFO -------------------------------------------

    /// Adds bytes stored to the gather pipe.
    pub fn gather(&mut self, bytes: &[u8]) {
        self.gather.extend_from_slice(bytes);
    }

    fn linked(&self) -> bool {
        self.cp_regs[1] & (CP_READ_ENABLE | CP_LINK_ENABLE) == CP_READ_ENABLE | CP_LINK_ENABLE
    }

    /// Takes the next full burst, advancing the FIFO write pointer. An
    /// executed burst is queued for [`Gx::run_fifo`].
    pub fn next_burst(&mut self) -> Option<Burst> {
        if self.gather.len() < 32 {
            return None;
        }
        let mut data = [0; 32];
        data.copy_from_slice(&self.gather[..32]);
        self.gather.drain(..32);
        self.stats.bursts += 1;
        let ram = (self.pi_end != 0).then_some(self.pi_wptr);
        if ram.is_some() {
            self.pi_wptr += 32;
            if self.pi_wptr >= self.pi_end {
                self.pi_wptr = self.pi_base;
                self.pi_wrapped = true;
            }
        }
        let execute = self.linked();
        if execute {
            // The CP's write pointer follows the CPU's.
            self.cp_regs[0x34 / 2] = self.pi_wptr as u16;
            self.cp_regs[0x36 / 2] = (self.pi_wptr >> 16) as u16;
            self.pending.extend_from_slice(&data);
            self.fifo_queued += 32;
            if let Some(w) = &mut self.worker {
                w.bytes(&data, self.widescreen, &self.wide_cameras);
            }
        }
        Some(Burst { data, ram, execute })
    }

    /// From what the CPU writes next, draws belong to the HUD on screen side
    /// `anchor` (-1 left, 1 right; 0: not the HUD).
    pub fn set_hud_anchor(&mut self, anchor: i8) {
        let at = self.fifo_queued + self.gather.len() as u64;
        match &self.worker {
            Some(w) => w.change(at, worker::Change::Anchor(anchor)),
            None => self.changes.push_back((at, worker::Change::Anchor(anchor))),
        }
    }

    /// Decodes queued FIFO commands until the data runs out.
    pub fn run_fifo(&mut self, ram: &Ram) {
        loop {
            while let Some(&(at, change)) = self.changes.front() {
                if at > self.fifo_decoded {
                    break;
                }
                match change {
                    worker::Change::Anchor(anchor) => self.hud_anchor = anchor,
                    worker::Change::LightWidened => self.light_widened = true,
                }
                self.changes.pop_front();
            }
            let data = std::mem::take(&mut self.pending);
            let parsed = self.parse(&data[self.pending_pos..], ram, false);
            self.pending = data;
            match parsed {
                Parsed::Incomplete => {
                    self.pending.drain(..self.pending_pos);
                    self.pending_pos = 0;
                    return;
                }
                Parsed::Done(len) => {
                    self.pending_pos += len;
                    self.fifo_decoded += len as u64;
                }
            }
        }
    }

    /// Runs a display list from RAM. Lists are self-contained; calls inside
    /// one are ignored.
    fn run_display_list(&mut self, ram: &Ram, addr: u32, size: u32) {
        self.stats.display_lists += 1;
        let Some(list) = ram.bytes(addr & 0x1fff_ffe0, size as usize) else {
            self.stats.bad_bytes += size as u64;
            return;
        };
        let mut pos = 0;
        while let Parsed::Done(len) = self.parse(&list[pos..], ram, true) {
            pos += len;
        }
    }

    // --- command decoding ------------------------------------------------

    /// Decodes and executes the command at the start of `d`.
    fn parse(&mut self, d: &[u8], ram: &Ram, in_list: bool) -> Parsed {
        let Some(&op) = d.first() else { return Parsed::Incomplete };
        let need = |n: usize| d.len() >= n;
        let len = match op {
            // NOP, invalidate vertex cache, performance metrics
            0x00 | 0x44 | 0x48 => 1,
            0x08 => {
                if !need(6) {
                    return Parsed::Incomplete;
                }
                if self.worker.is_none() && draw::xf_log_frame() == Some(self.stats.xfb_copies) && d[1] & 0xf0 == 0x30 {
                    println!("CP write {:#x} = {:#x} list {in_list}", d[1], be32(&d[2..]));
                }
                self.cp[d[1] as usize] = be32(&d[2..]);
                6
            }
            0x10 => {
                if !need(5) {
                    return Parsed::Incomplete;
                }
                let header = be32(&d[1..]);
                let count = ((header >> 16) & 0xffff) as usize + 1;
                if !need(5 + 4 * count) {
                    return Parsed::Incomplete;
                }
                let base = (header & 0xffff) as usize;
                if self.worker.is_none() && draw::xf_log_frame() == Some(self.stats.xfb_copies) && (base < 0x400 || (0x1018..0x101a).contains(&base)) {
                    let v: Vec<f32> = (0..count).map(|k| f32::from_bits(be32(&d[5 + 4 * k..]))).collect();
                    println!("XF write {base:#x} list {in_list} {v:?}");
                }
                for k in 0..count {
                    if let Some(r) = self.xf.get_mut(base + k) {
                        *r = be32(&d[5 + 4 * k..]);
                    }
                }
                5 + 4 * count
            }
            // Indexed XF loads: position/normal/texture/light matrices from
            // CP arrays 12-15 in RAM.
            0x20 | 0x28 | 0x30 | 0x38 => {
                if !need(5) {
                    return Parsed::Incomplete;
                }
                self.load_indexed(ram, 12 + ((op - 0x20) >> 3) as usize, be32(&d[1..]));
                5
            }
            0x40 => {
                if !need(9) {
                    return Parsed::Incomplete;
                }
                if !in_list {
                    self.run_display_list(ram, be32(&d[1..]), be32(&d[5..]));
                }
                9
            }
            0x61 => {
                if !need(5) {
                    return Parsed::Incomplete;
                }
                self.bp_write(be32(&d[1..]), ram);
                5
            }
            0x80..=0xbf => {
                if !need(3) {
                    return Parsed::Incomplete;
                }
                let count = u16::from_be_bytes([d[1], d[2]]) as usize;
                let vsize = self.vertex_size((op & 7) as usize);
                let len = 3 + count * vsize;
                if !need(len) {
                    return Parsed::Incomplete;
                }
                self.stats.draws += 1;
                self.stats.vertices += count as u64;
                if self.builds_draws() && vsize > 0 {
                    let t = std::time::Instant::now();
                    self.record_draw(ram, op, count, &d[3..len], vsize);
                    self.stats.draw_building += t.elapsed();
                } else if self.capture && vsize > 0 && draw::is_triangles(op) {
                    // The worker builds it; the hooks here still need this.
                    let view = self.view();
                    self.note_view(&view);
                }
                len
            }
            _ => {
                // Not a command: the stream is out of step. Skip the byte.
                self.stats.bad_bytes += 1;
                if self.stats.first_bad.is_none() {
                    self.stats.first_bad = Some((op, self.stats.commands));
                }
                return Parsed::Done(1);
            }
        };
        self.stats.commands += 1;
        Parsed::Done(len)
    }

    /// Copies `index`'s entry of CP array `array` into XF memory. The
    /// command word holds the index (high 16 bits), the word count minus
    /// one (4 bits) and the XF address (12 bits).
    fn load_indexed(&mut self, ram: &Ram, array: usize, v: u32) {
        let index = v >> 16;
        let count = ((v >> 12) & 0xf) as usize + 1;
        let addr = (v & 0xfff) as usize;
        let src = self.cp[0xa0 + array].wrapping_add(index.wrapping_mul(self.cp[0xb0 + array]));
        if self.worker.is_none() && draw::xf_log_frame() == Some(self.stats.xfb_copies) {
            let vals: Vec<f32> = (0..count).map(|k| f32::from_bits(ram.u32(src + 4 * k as u32))).collect();
            println!("XF indexed {array} index {index} -> {addr:#x} from {src:#x} {vals:?}");
        }
        for k in 0..count {
            if let Some(r) = self.xf.get_mut(addr + k) {
                *r = ram.u32(src + 4 * k as u32);
            }
        }
    }

    fn bp_write(&mut self, v: u32, ram: &Ram) {
        self.draw_state_changed = true;
        let reg = (v >> 24) as u8;
        let value = v & 0x00ff_ffff;
        if reg == BP_MASK {
            self.bp_mask = value;
            return;
        }
        let old = self.bp[reg as usize];
        self.bp[reg as usize] = (old & !self.bp_mask) | (value & self.bp_mask);
        self.bp_mask = 0x00ff_ffff;
        match reg {
            BP_DRAW_DONE if value & 0xff == 2 => {
                // The game takes this as the GPU being done with what it
                // drew from: the worker must be.
                if let Some(w) = &mut self.worker {
                    w.wait(self.fifo_decoded);
                }
                self.stats.draw_done += 1;
                self.pe_regs[5] |= PE_FINISH_INT;
            }
            BP_TOKEN | BP_TOKEN_INT => {
                self.stats.tokens += 1;
                self.pe_regs[0x0e / 2] = value as u16;
                if reg == BP_TOKEN_INT {
                    self.pe_regs[5] |= PE_TOKEN_INT;
                }
            }
            BP_COPY => {
                if value & COPY_TO_XFB != 0 {
                    self.stats.xfb_copies += 1;
                }
                if self.builds_draws() {
                    self.record_copy(value, ram);
                }
            }
            BP_LOAD_TLUT1 => {
                let src = (self.bp[BP_LOAD_TLUT0 as usize] & 0x00ff_ffff) << 5;
                let dst = ((value & 0x3ff) << 9) as usize;
                let len = ((value >> 10) & 0x7ff) as usize * 32;
                if let (Some(data), true) = (ram.bytes(src, len), dst + len <= TMEM_SIZE) {
                    self.tmem[dst..dst + len].copy_from_slice(data);
                }
            }
            // TEV color and konst registers: red/alpha in the low register,
            // blue/green in the high one; bit 23 selects konst.
            0xe0..=0xe7 => {
                let n = ((reg - 0xe0) / 2) as usize;
                let s11 = |x: u32| ((x << 21) as i32 >> 21) as f32 / 255.0;
                let (lo, hi) = (s11(value), s11(value >> 12));
                let target = if value & (1 << 23) != 0 { &mut self.konst[n] } else { &mut self.tev_regs[n] };
                if reg & 1 == 0 {
                    (target[0], target[3]) = (lo, hi);
                } else {
                    (target[2], target[1]) = (lo, hi);
                }
            }
            _ => {}
        }
    }

    /// Bytes per vertex for vertex format `fmt`, from the vertex descriptor
    /// and attribute format registers.
    fn vertex_size(&self, fmt: usize) -> usize {
        let vcd_lo = self.cp[CP_VCD_LO];
        let vcd_hi = self.cp[CP_VCD_HI];
        let (a, b, c) = (self.cp[CP_VAT_A + fmt], self.cp[CP_VAT_B + fmt], self.cp[CP_VAT_C + fmt]);
        let bits = |v: u32, lo: u32, n: u32| (v >> lo) & ((1 << n) - 1);
        let comp = |format: u32| match format {
            0 | 1 => 1, // u8, s8
            2 | 3 => 2, // u16, s16
            _ => 4,     // f32
        };
        // Per attribute: 0 absent, 1 direct, 2 8-bit index, 3 16-bit index.
        let attr = |mode: u32, direct: usize| match mode {
            1 => direct,
            2 => 1,
            3 => 2,
            _ => 0,
        };

        // Position/normal matrix index and eight texture matrix indices.
        let mut size = (0..9).filter(|&k| vcd_lo & (1 << k) != 0).count();

        let pos = 2 + bits(a, 0, 1) as usize;
        size += attr(bits(vcd_lo, 9, 2), pos * comp(bits(a, 1, 3)));

        let nbt = bits(a, 9, 1) != 0;
        let normal_mode = bits(vcd_lo, 11, 2);
        let normal_direct = if nbt { 9 } else { 3 } * comp(bits(a, 10, 3));
        let mut normal = attr(normal_mode, normal_direct);
        if nbt && normal_mode >= 2 && bits(a, 31, 1) != 0 {
            normal *= 3; // separate indices for normal, binormal, tangent
        }
        size += normal;

        let color = |format: u32| match format {
            0 | 3 => 2, // RGB565, RGBA4444
            1 | 4 => 3, // RGB888, RGBA6666
            _ => 4,     // RGB888x, RGBA8888
        };
        size += attr(bits(vcd_lo, 13, 2), color(bits(a, 14, 3)));
        size += attr(bits(vcd_lo, 15, 2), color(bits(a, 18, 3)));

        // Texture coordinates: (two-component flag, format) per unit.
        let tex = [
            (bits(a, 21, 1), bits(a, 22, 3)),
            (bits(b, 0, 1), bits(b, 1, 3)),
            (bits(b, 9, 1), bits(b, 10, 3)),
            (bits(b, 18, 1), bits(b, 19, 3)),
            (bits(b, 27, 1), bits(b, 28, 3)),
            (bits(c, 5, 1), bits(c, 6, 3)),
            (bits(c, 14, 1), bits(c, 15, 3)),
            (bits(c, 23, 1), bits(c, 24, 3)),
        ];
        for (k, (st, format)) in tex.into_iter().enumerate() {
            let direct = (1 + st as usize) * comp(format);
            size += attr(bits(vcd_hi, 2 * k as u32, 2), direct);
        }
        size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Memory;

    const PIPE: u32 = 0xcc00_8000;

    fn feed(m: &mut Memory, bytes: &[u8]) {
        for &b in bytes {
            m.w8(PIPE, b).unwrap();
        }
    }

    /// A FIFO at physical 0x100000 with the CP linked and reading.
    fn linked() -> Memory {
        let mut m = Memory::default();
        m.w32(0xcc00_300c, 0x0010_0000).unwrap();
        m.w32(0xcc00_3010, 0x0010_0000 + 0x1000 - 4).unwrap();
        m.w32(0xcc00_3014, 0x0010_0000).unwrap();
        m.w16(0xcc00_0002, CP_READ_ENABLE | CP_LINK_ENABLE).unwrap();
        m
    }

    /// GXDrawDone: the draw-done BP write, padded out by GXFlush, raises
    /// the PE finish interrupt once enabled.
    #[test]
    fn draw_done_raises_finish() {
        let mut m = linked();
        m.w16(0xcc00_100a, PE_FINISH_ENABLE).unwrap();
        m.w32(0xcc00_3004, 1 << 10).unwrap();
        m.w8(PIPE, 0x61).unwrap();
        m.w32(PIPE, 0x4500_0002).unwrap();
        assert!(!m.irq_pending(), "still in the gather pipe");
        for _ in 0..8 {
            m.w32(PIPE, 0).unwrap();
        }
        assert!(m.irq_pending());
        assert_eq!(m.r8(0x8010_0000).unwrap(), 0x61, "burst written to the FIFO in RAM");
        m.w16(0xcc00_100a, PE_FINISH_ENABLE | PE_FINISH_INT).unwrap();
        assert!(!m.irq_pending());
        assert_eq!(m.gx.stats.bad_bytes, 0);
    }

    /// A frame: a triangle, a copy to the XFB and a draw-done.
    fn frame_stream() -> Vec<u8> {
        let mut s = Vec::new();
        s.extend([0x08, 0x50]);
        s.extend((1u32 << 9).to_be_bytes()); // pos direct
        s.extend([0x08, 0x70]);
        s.extend((1u32 | (4 << 1)).to_be_bytes()); // pos xyz f32
        s.extend([0x90, 0x00, 0x03]); // triangles, 3 vertices
        for v in [[0.0f32, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]] {
            v.iter().for_each(|c| s.extend(c.to_be_bytes()));
        }
        s.extend([0x61, BP_COPY]);
        s.extend(&COPY_TO_XFB.to_be_bytes()[1..]);
        s.extend([0x61, BP_DRAW_DONE, 0x00, 0x00, 0x02]);
        s.resize(s.len().next_multiple_of(32) + 32, 0);
        s
    }

    /// Draws built on a worker thread come out as when built in place.
    #[test]
    fn worker_builds_the_same_frames() {
        let frames = |threaded: bool| {
            let mut m = linked();
            m.gx.capture = true;
            if threaded {
                m.start_gx_worker();
            }
            let s = frame_stream();
            feed(&mut m, &s);
            feed(&mut m, &s);
            m.gx.take_frames().iter().map(|f| format!("{f:?}")).collect::<Vec<_>>()
        };
        let in_place = frames(false);
        assert_eq!(in_place.len(), 2);
        assert!(in_place[0].contains("Draw"), "{}", in_place[0]);
        assert_eq!(frames(true), in_place);
    }

    /// A draw's length comes from the vertex format: position xyz f32 and
    /// an 8-bit color index is 13 bytes a vertex.
    #[test]
    fn draws_are_sized_from_the_vertex_format() {
        let mut m = linked();
        let mut s = Vec::new();
        s.extend([0x08, 0x50]);
        s.extend(((1u32 << 9) | (2 << 13)).to_be_bytes()); // pos direct, color0 index8
        s.extend([0x08, 0x70]);
        s.extend((1u32 | (4 << 1)).to_be_bytes()); // pos xyz f32
        s.extend([0x90, 0x00, 0x03]); // triangles, 3 vertices
        s.extend([0u8; 39]);
        s.extend([0x61, BP_TOKEN_INT, 0x00, 0x12, 0x34]);
        s.resize(s.len().next_multiple_of(32), 0);
        m.w16(0xcc00_100a, PE_TOKEN_ENABLE).unwrap();
        feed(&mut m, &s);
        let st = &m.gx.stats;
        assert_eq!((st.draws, st.vertices, st.bad_bytes), (1, 3, 0));
        assert!(m.gx.pe_token_irq());
        assert_eq!(m.r16(0xcc00_100e).unwrap(), 0x1234);
    }

    /// A display list called from the FIFO runs in order, before the
    /// commands after the call.
    #[test]
    fn display_lists_run_from_ram() {
        let mut m = linked();
        let mut list = vec![0x61, BP_TOKEN, 0x00, 0x00, 0x07];
        list.resize(32, 0);
        m.load(0x0020_0000, &list).unwrap();
        let mut s = vec![0x40];
        s.extend(0x0020_0000u32.to_be_bytes());
        s.extend(32u32.to_be_bytes());
        s.extend([0x61, BP_TOKEN, 0x00, 0x00, 0x08]);
        s.resize(32, 0);
        feed(&mut m, &s);
        assert_eq!(m.gx.stats.display_lists, 1);
        assert_eq!(m.gx.stats.tokens, 2);
        assert_eq!(m.r16(0xcc00_100e).unwrap(), 8);
    }

    /// While unlinked (recording a display list), bursts reach RAM only.
    #[test]
    fn unlinked_fifo_is_not_executed() {
        let mut m = linked();
        m.w16(0xcc00_0002, 0).unwrap();
        m.w16(0xcc00_100a, PE_FINISH_ENABLE).unwrap();
        let mut s = vec![0x61, BP_DRAW_DONE, 0, 0, 2];
        s.resize(32, 0);
        feed(&mut m, &s);
        assert!(!m.gx.pe_finish_irq());
        assert_eq!(m.r8(0x8010_0000).unwrap(), 0x61);
        assert_eq!(m.r32(0xcc00_3014).unwrap(), 0x0010_0020);
    }

    /// An indexed XF load split across two bursts waits for its last bytes.
    #[test]
    fn indexed_load_across_bursts() {
        let mut m = linked();
        let mut s = vec![0u8; 31];
        s.extend([0x20, 0x00, 0x04, 0xb0, 0x00]); // load position matrix
        s.extend([0x61, BP_TOKEN, 0x00, 0x00, 0x09]);
        s.resize(64, 0);
        feed(&mut m, &s);
        assert_eq!((m.gx.stats.bad_bytes, m.gx.stats.tokens), (0, 1));
        assert_eq!(m.r16(0xcc00_100e).unwrap(), 9);
    }

    /// LOAD_INDX_A copies a position matrix from CP array 12 in RAM into
    /// XF memory.
    #[test]
    fn indexed_load_fetches_matrix_from_ram() {
        let mut m = linked();
        for k in 0..12u32 {
            m.w32(0x8020_0000 + 0x30 + 4 * k, 0x100 + k).unwrap();
        }
        let mut s = Vec::new();
        s.extend([0x08, 0xac]);
        s.extend(0x0020_0000u32.to_be_bytes()); // array 12 base
        s.extend([0x08, 0xbc]);
        s.extend(0x30u32.to_be_bytes()); // stride
        s.push(0x20);
        s.extend((1u32 << 16 | 11 << 12 | 0x0c).to_be_bytes()); // index 1, 12 words, XF 0x0c
        s.resize(32, 0);
        feed(&mut m, &s);
        assert_eq!(&m.gx.xf[0x0c..0x18], &(0..12).map(|k| 0x100 + k).collect::<Vec<u32>>()[..]);
    }

    /// A palette in MEM2 loads whole: the source address keeps its high
    /// bits and the count is in 32-byte units.
    #[test]
    fn tlut_load_from_mem2() {
        let mut m = linked();
        for k in 0..256u32 {
            m.w16(0x9230_0000 + 2 * k, 0x8000 | k as u16).unwrap();
        }
        let mut s = vec![0x61, BP_LOAD_TLUT0];
        s.extend(&((0x1230_0000u32 >> 5).to_be_bytes()[1..]));
        s.push(0x61);
        s.push(BP_LOAD_TLUT1);
        s.extend(&((16u32 << 10) | 0x200).to_be_bytes()[1..]); // 16 x 32 bytes to TMEM 0x40000
        s.resize(32, 0);
        feed(&mut m, &s);
        let tmem = |k: usize| u16::from_be_bytes([m.gx.tmem[0x40000 + 2 * k], m.gx.tmem[0x40000 + 2 * k + 1]]);
        assert_eq!((tmem(0), tmem(255)), (0x8000, 0x80ff));
    }
}