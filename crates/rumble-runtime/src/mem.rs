//! Guest physical memory and the hardware-register (MMIO) boundary.

use crate::ai::Ai;
use crate::dsp::Dsp;
use crate::gx::Gx;
use crate::input::Input;
use crate::ios::Ios;
use crate::ipc::Ipc;
use crate::vi::Vi;
use std::collections::BTreeMap;

pub const MEM1_SIZE: usize = 24 << 20;
pub const MEM2_SIZE: usize = 64 << 20;
/// Locked L1 data cache ("dcbz_l" scratchpad) mapped at 0xE0000000.
pub const LOCKED_CACHE_SIZE: usize = 16 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Access to an address with no backing memory or device.
    Unmapped(u32),
}

pub struct Memory {
    mem1: Vec<u8>,
    mem2: Vec<u8>,
    locked_cache: Vec<u8>,
    /// Hardware-register accesses seen so far: (address, is_write) -> count.
    /// Registers with no device behind them read 0 and drop writes.
    pub mmio_log: BTreeMap<(u32, bool), u64>,
    pub dsp: Dsp,
    pub ipc: Ipc,
    pub ios: Ios,
    pub vi: Vi,
    pub ai: Ai,
    pub gx: Gx,
    pub input: Input,
    /// Sound the game has played (through the audio DMA) and the host has
    /// not taken yet: interleaved left/right samples at `audio_rate`. Kept
    /// to a second's worth when nobody takes it.
    pub audio: Vec<i16>,
    pub audio_rate: u32,
    /// Current timebase, for devices that compute state on access.
    pub now: u64,
    /// Processor Interface interrupt mask (0xCC003004).
    pi_mask: u32,
    /// Set when the interrupt input may have changed: the interrupt lines
    /// are device state, which only moves when devices advance or a
    /// hardware register is accessed. Lets the CPU skip recomputing it.
    pub irq_dirty: bool,
    /// Set on every hardware register access, for compiled code to stop
    /// chaining and let the dispatcher look (`cpu::blocks`).
    pub mmio_touched: bool,
    /// The hub's multiplayer pedestal, replaced as its files are unpacked.
    pub multiplay: crate::hle::multiplay::State,
}

/// PI interrupt cause bits.
const PI_DSP: u32 = 1 << 6;
const PI_VI: u32 = 1 << 8;
const PI_PE_TOKEN: u32 = 1 << 9;
const PI_PE_FINISH: u32 = 1 << 10;
const PI_IPC: u32 = 1 << 14;
/// Reset switch state: set while the button is released. Not an interrupt.
const PI_RESET_SWITCH: u32 = 1 << 16;

/// The CPU's write-gather pipe: stores of any width anywhere in this
/// 32-byte window feed the graphics FIFO.
const GATHER_PIPE: u32 = 0xcc00_8000;

/// Read-only view of RAM by physical (or cached/uncached) address, for the
/// GPU, which reads vertex arrays, display lists and textures while the rest
/// of `Memory` is in use.
pub struct Ram<'a> {
    mem1: &'a [u8],
    mem2: &'a [u8],
}

impl Ram<'_> {
    pub fn bytes(&self, addr: u32, len: usize) -> Option<&[u8]> {
        let a = (addr & 0x1fff_ffff) as usize;
        let (buf, off) = if a < 0x1000_0000 { (self.mem1, a) } else { (self.mem2, a - 0x1000_0000) };
        buf.get(off..off.checked_add(len)?)
    }

    pub fn u8(&self, addr: u32) -> u8 {
        self.bytes(addr, 1).map_or(0, |b| b[0])
    }

    pub fn u16(&self, addr: u32) -> u16 {
        self.bytes(addr, 2).map_or(0, |b| u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn u32(&self, addr: u32) -> u32 {
        self.bytes(addr, 4).map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

enum Region<'a> {
    Ram(&'a mut [u8]),
    Mmio,
}

impl Drop for Memory {
    fn drop(&mut self) {
        // The graphics worker reads RAM: stop it first.
        self.gx.stop_worker();
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            mem1: vec![0; MEM1_SIZE],
            mem2: vec![0; MEM2_SIZE],
            locked_cache: vec![0; LOCKED_CACHE_SIZE],
            mmio_log: BTreeMap::new(),
            dsp: Dsp::default(),
            ipc: Ipc::default(),
            ios: Ios::default(),
            vi: Vi::default(),
            ai: Ai::default(),
            gx: Gx::default(),
            input: Input::default(),
            now: 0,
            pi_mask: 0,
            irq_dirty: true,
            mmio_touched: false,
            multiplay: Default::default(),
            audio: Vec::new(),
            audio_rate: 32_000,
        }
    }
}

impl Memory {
    /// Maps an effective address (BAT-style fixed mapping used by the SDK)
    /// to backing storage, returning the slice starting at that address.
    fn region(&mut self, addr: u32, len: usize) -> Result<Region<'_>, Fault> {
        fn slice(buf: &mut [u8], off: usize, len: usize, addr: u32) -> Result<Region<'_>, Fault> {
            buf.get_mut(off..off + len).map(Region::Ram).ok_or(Fault::Unmapped(addr))
        }
        // Hollywood/Broadway hardware registers live at 0xCC000000-0xCDFFFFFF.
        if addr & 0xfe00_0000 == 0xcc00_0000 {
            return Ok(Region::Mmio);
        }
        match addr >> 28 {
            0x0 | 0x8 | 0xC => slice(&mut self.mem1, (addr & 0x0fff_ffff) as usize, len, addr),
            0x1 | 0x9 | 0xD => slice(&mut self.mem2, (addr & 0x0fff_ffff) as usize, len, addr),
            0xE => slice(&mut self.locked_cache, (addr & 0x0fff_ffff) as usize, len, addr),
            _ => Err(Fault::Unmapped(addr)),
        }
    }

    /// The cached RAM mirrors (0x80000000 MEM1, 0x90000000 MEM2), which
    /// nearly every access uses, without going through `region`.
    #[inline(always)]
    fn cached_ram(&mut self, addr: u32, len: usize) -> Option<&mut [u8]> {
        let off = (addr & 0x0fff_ffff) as usize;
        match addr >> 28 {
            0x8 => self.mem1.get_mut(off..off + len),
            0x9 => self.mem2.get_mut(off..off + len),
            _ => None,
        }
    }

    #[inline(always)]
    fn read<const N: usize>(&mut self, addr: u32) -> Result<[u8; N], Fault> {
        match self.cached_ram(addr, N) {
            Some(s) => Ok(s.try_into().unwrap()),
            None => self.read_slow(addr),
        }
    }

    #[cold]
    #[inline(never)]
    fn read_slow<const N: usize>(&mut self, addr: u32) -> Result<[u8; N], Fault> {
        match self.region(addr, N)? {
            Region::Ram(s) => Ok(s.try_into().unwrap()),
            Region::Mmio => {
                self.irq_dirty = true;
                self.mmio_touched = true;
                *self.mmio_log.entry((addr, false)).or_default() += 1;
                let mut v = [0; N];
                // Byte reads of device registers see nothing.
                if N == 2 {
                    v.copy_from_slice(&self.mmio_read16(addr).to_be_bytes());
                } else if N >= 4 {
                    for (k, word) in v.chunks_exact_mut(4).enumerate() {
                        word.copy_from_slice(&self.mmio_read32(addr + 4 * k as u32).to_be_bytes());
                    }
                }
                Ok(v)
            }
        }
    }

    #[inline(always)]
    fn write<const N: usize>(&mut self, addr: u32, v: [u8; N]) -> Result<(), Fault> {
        match self.cached_ram(addr, N) {
            Some(s) => Ok(s.copy_from_slice(&v)),
            None => self.write_slow(addr, v),
        }
    }

    #[cold]
    #[inline(never)]
    fn write_slow<const N: usize>(&mut self, addr: u32, v: [u8; N]) -> Result<(), Fault> {
        match self.region(addr, N)? {
            Region::Ram(s) => s.copy_from_slice(&v),
            Region::Mmio => {
                self.irq_dirty = true;
                self.mmio_touched = true;
                *self.mmio_log.entry((addr, true)).or_default() += 1;
                if addr & !0x1f == GATHER_PIPE {
                    self.gather(&v)?;
                } else if N == 2 {
                    self.mmio_write16(addr, u16::from_be_bytes([v[0], v[1]]));
                } else if N >= 4 {
                    for (k, word) in v.chunks_exact(4).enumerate() {
                        self.mmio_write32(addr + 4 * k as u32, u32::from_be_bytes(word.try_into().unwrap()));
                    }
                }
            }
        }
        Ok(())
    }

    /// Feeds the gather pipe: full bursts go to the graphics FIFO in RAM and,
    /// while the GPU is reading it, through the command decoder.
    fn gather(&mut self, bytes: &[u8]) -> Result<(), Fault> {
        self.gx.gather(bytes);
        while let Some(burst) = self.gx.next_burst() {
            if let Some(addr) = burst.ram {
                self.load(addr, &burst.data)?;
            }
            if burst.execute {
                let ram = Ram { mem1: &self.mem1, mem2: &self.mem2 };
                self.gx.run_fifo(&ram);
            }
        }
        Ok(())
    }

    /// PI and Hollywood registers are 32 bits wide; everything else is made
    /// of 16-bit registers.
    fn mmio_read32(&mut self, addr: u32) -> u32 {
        match addr {
            0xcc00_3000 => self.pi_cause(),
            0xcc00_3004 => self.pi_mask,
            0xcc00_300c..=0xcc00_3017 => self.gx.pi_read32(addr - 0xcc00_3000),
            0xcd00_0000..=0xcd00_003f => self.ipc.read32(addr - 0xcd00_0000),
            0xcd00_6c00..=0xcd00_6c0f => self.ai.read32(addr - 0xcd00_6c00, self.now),
            _ => (self.mmio_read16(addr) as u32) << 16 | self.mmio_read16(addr + 2) as u32,
        }
    }

    fn mmio_write32(&mut self, addr: u32, v: u32) {
        match addr {
            // Every cause bit modelled so far follows its source, so
            // acknowledging through PI has nothing to clear.
            0xcc00_3000 => {}
            0xcc00_3004 => self.pi_mask = v,
            0xcc00_300c..=0xcc00_3017 => self.gx.pi_write32(addr - 0xcc00_3000, v),
            0xcd00_6c00..=0xcd00_6c0f => self.ai.write32(addr - 0xcd00_6c00, v, self.now),
            0xcd00_0000..=0xcd00_003f => {
                if let Some(req) = self.ipc.write32(addr - 0xcd00_0000, v) {
                    let mut ios = std::mem::take(&mut self.ios);
                    let r = ios.execute(self, req);
                    self.ios = ios;
                    match r {
                        Ok(done) => done.into_iter().for_each(|r| self.ipc.push_reply(r)),
                        // A request IOS cannot even read never gets a reply.
                        Err(f) => self.ios.log.push(format!("request 0x{req:08x} unreadable: {f:?}")),
                    }
                }
            }
            _ => {
                self.mmio_write16(addr, (v >> 16) as u16);
                self.mmio_write16(addr + 2, v as u16);
            }
        }
    }

    fn pi_cause(&self) -> u32 {
        let mut cause = PI_RESET_SWITCH;
        if self.dsp.irq_line() {
            cause |= PI_DSP;
        }
        if self.vi.irq_line() {
            cause |= PI_VI;
        }
        if self.gx.pe_token_irq() {
            cause |= PI_PE_TOKEN;
        }
        if self.gx.pe_finish_irq() {
            cause |= PI_PE_FINISH;
        }
        if self.ipc.irq_line() {
            cause |= PI_IPC;
        }
        cause
    }

    /// Timebase at which a device next changes state on its own.
    pub fn next_event(&self) -> u64 {
        self.vi.next_event().min(self.dsp.next_event())
    }

    /// Brings time-driven devices up to timebase `now`.
    pub fn advance(&mut self, now: u64) {
        self.irq_dirty = true;
        self.vi.advance(now);
        self.dsp.sync(now, self.ai.dma_rate());
        self.dsp.advance();
        self.collect_audio();
    }

    /// Runs a pending AX command list and keeps the audio DMA's buffers.
    fn collect_audio(&mut self) {
        if let Some(list) = self.dsp.pending_cmdlist.take() {
            crate::ax::run(self, list);
        }
        if self.dsp.played.is_empty() {
            return;
        }
        self.audio_rate = self.ai.dma_rate() as u32;
        for (addr, len) in std::mem::take(&mut self.dsp.played) {
            if let Ok(b) = self.read_bytes(addr | 0x8000_0000, len) {
                self.audio.extend(b.chunks_exact(2).map(|s| i16::from_be_bytes([s[0], s[1]])));
            }
        }
        let max = self.audio_rate as usize * 2;
        if self.audio.len() > max {
            self.audio.drain(..self.audio.len() - max);
        }
    }

    /// Whether the CPU's external interrupt input is asserted.
    pub fn irq_pending(&self) -> bool {
        self.pi_cause() & self.pi_mask & !PI_RESET_SWITCH != 0
    }

    fn mmio_read16(&mut self, addr: u32) -> u16 {
        match addr {
            0xcc00_0000..=0xcc00_007f => self.gx.cp_read16(addr - 0xcc00_0000),
            0xcc00_1000..=0xcc00_107f => self.gx.pe_read16(addr - 0xcc00_1000),
            0xcc00_2000..=0xcc00_20ff => self.vi.read16(addr - 0xcc00_2000),
            0xcc00_5000..=0xcc00_51ff => {
                self.dsp.sync(self.now, self.ai.dma_rate());
                self.dsp.read16(addr - 0xcc00_5000)
            }
            _ => 0,
        }
    }

    fn mmio_write16(&mut self, addr: u32, v: u16) {
        match addr {
            0xcc00_0000..=0xcc00_007f => self.gx.cp_write16(addr - 0xcc00_0000, v),
            0xcc00_1000..=0xcc00_107f => self.gx.pe_write16(addr - 0xcc00_1000, v),
            0xcc00_2000..=0xcc00_20ff => self.vi.write16(addr - 0xcc00_2000, v),
            0xcc00_5000..=0xcc00_51ff => {
                self.dsp.sync(self.now, self.ai.dma_rate());
                self.dsp.write16(addr - 0xcc00_5000, v);
                self.collect_audio();
            }
            _ => {}
        }
    }

    /// Instruction fetch, with a fast path for cached MEM1 where the
    /// game's code lives.
    #[inline]
    pub fn fetch(&mut self, pc: u32) -> Result<u32, Fault> {
        let off = pc.wrapping_sub(0x8000_0000) as usize;
        match self.mem1.get(off..off + 4) {
            Some(b) => Ok(u32::from_be_bytes(b.try_into().unwrap())),
            None => self.r32(pc),
        }
    }

    #[inline(always)]
    pub fn r8(&mut self, a: u32) -> Result<u8, Fault> { Ok(self.read::<1>(a)?[0]) }
    #[inline(always)]
    pub fn r16(&mut self, a: u32) -> Result<u16, Fault> { self.read(a).map(u16::from_be_bytes) }
    #[inline(always)]
    pub fn r32(&mut self, a: u32) -> Result<u32, Fault> { self.read(a).map(u32::from_be_bytes) }
    #[inline(always)]
    pub fn r64(&mut self, a: u32) -> Result<u64, Fault> { self.read(a).map(u64::from_be_bytes) }
    #[inline(always)]
    pub fn w8(&mut self, a: u32, v: u8) -> Result<(), Fault> { self.write(a, [v]) }
    #[inline(always)]
    pub fn w16(&mut self, a: u32, v: u16) -> Result<(), Fault> { self.write(a, v.to_be_bytes()) }
    #[inline(always)]
    pub fn w32(&mut self, a: u32, v: u32) -> Result<(), Fault> { self.write(a, v.to_be_bytes()) }
    #[inline(always)]
    pub fn w64(&mut self, a: u32, v: u64) -> Result<(), Fault> { self.write(a, v.to_be_bytes()) }

    pub fn load(&mut self, addr: u32, bytes: &[u8]) -> Result<(), Fault> {
        match self.region(addr, bytes.len())? {
            Region::Ram(s) => Ok(s.copy_from_slice(bytes)),
            Region::Mmio => Err(Fault::Unmapped(addr)),
        }
    }

    /// Where MEM1 and MEM2 are in host memory, and their sizes, for code
    /// translated at load time to reach the cached mirrors directly. RAM is
    /// allocated once and never moves.
    pub fn ram_host(&mut self) -> [(*mut u8, usize); 2] {
        [(self.mem1.as_mut_ptr(), self.mem1.len()), (self.mem2.as_mut_ptr(), self.mem2.len())]
    }

    pub fn ram(&self) -> Ram<'_> {
        Ram { mem1: &self.mem1, mem2: &self.mem2 }
    }

    /// Builds draws on a thread of their own from now on (`gx::worker`).
    /// Call before the game starts drawing.
    pub fn start_gx_worker(&mut self) {
        // SAFETY: the worker reads RAM while the CPU writes it, as the GPU
        // does on the Wii. RAM is allocated once and never resized, and
        // `Drop` stops the worker before it is freed. A byte the worker
        // reads as it changes is stale or new, never torn into an invalid
        // value (every byte pattern is valid); the game itself makes sure
        // what a draw uses stays put until the draw is done, and the
        // emulator waits for the worker there (`worker::Worker::wait`).
        let ram = unsafe {
            Ram {
                mem1: std::slice::from_raw_parts(self.mem1.as_ptr(), self.mem1.len()),
                mem2: std::slice::from_raw_parts(self.mem2.as_ptr(), self.mem2.len()),
            }
        };
        self.gx.start_worker(ram);
    }

    /// Copies `len` bytes of RAM out, for devices working on guest buffers.
    pub fn read_bytes(&mut self, addr: u32, len: u32) -> Result<Vec<u8>, Fault> {
        match self.region(addr, len as usize)? {
            Region::Ram(s) => Ok(s.to_vec()),
            Region::Mmio => Err(Fault::Unmapped(addr)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mirrors_and_mmio() {
        let mut m = Memory::default();
        m.w32(0x8000_1000, 0xdead_beef).unwrap();
        assert_eq!(m.r32(0xC000_1000).unwrap(), 0xdead_beef);
        assert_eq!(m.r16(0x8000_1002).unwrap(), 0xbeef);
        m.w32(0x9000_0000, 7).unwrap();
        assert_eq!(m.r32(0xD000_0000).unwrap(), 7);
        assert_eq!(m.r32(0xCC00_2000).unwrap(), 0);
        assert_eq!(m.mmio_log[&(0xCC00_2000, false)], 1);
    }

    /// An open request sent through the IPC registers comes back as an IOS
    /// reply and raises the PPC's external interrupt once unmasked.
    #[test]
    fn ipc_round_trip() {
        let mut m = Memory::default();
        m.load(0x0000_2000, b"/dev/nothing ").unwrap();
        m.w32(0x8000_1000, 1).unwrap(); // open
        m.w32(0x8000_100c, 0x2000).unwrap();
        m.w32(0xcc00_3004, 1 << 14).unwrap();
        m.w32(0xcd00_0034, 1 << 30).unwrap();
        m.w32(0xcd00_0004, 0x38).unwrap();
        m.w32(0xcd00_0000, 0x1000).unwrap();
        m.w32(0xcd00_0004, 0x31).unwrap();
        assert!(m.irq_pending());
        assert_eq!(m.r32(0x8000_1000).unwrap(), 8);
        assert_eq!(m.r32(0x8000_1004).unwrap() as i32, -6);
        assert_eq!(m.r32(0x8000_1008).unwrap(), 1);
        m.w32(0xcc00_3004, 0).unwrap();
        assert!(!m.irq_pending());
    }
}