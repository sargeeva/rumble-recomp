//! Audio Interface control registers at 0xCD006C00: playback state, sample
//! rates and the sample counter. Sample data reaches the AI through the
//! audio DMA on the DSP side (`dsp.rs`), at the rate set here.

use crate::cpu::TB_HZ;

/// AICR bits.
const PSTAT: u32 = 1 << 0; // playing
const AISFR: u32 = 1 << 1; // counter (stream) rate: 0 = 32 kHz, 1 = 48 kHz
const AIINT: u32 = 1 << 3; // interrupt status, write 1 to clear
const SCRESET: u32 = 1 << 5; // reset the sample counter, self-clearing
const AIDFR: u32 = 1 << 6; // audio DMA rate: 0 = 48 kHz, 1 = 32 kHz

#[derive(Default)]
pub struct Ai {
    control: u32,
    volume: u32,
    interrupt_timing: u32,
    /// Counter value at timebase `since`. While playing it advances on
    /// every edge of a free-running sample clock, not from `since`.
    count: u32,
    since: u64,
}

impl Ai {
    fn rate(&self) -> u64 {
        if self.control & AISFR != 0 { 48_000 } else { 32_000 }
    }

    /// Sample rate of the audio DMA from the DSP side.
    pub fn dma_rate(&self) -> u64 {
        if self.control & AIDFR != 0 { 32_000 } else { 48_000 }
    }

    fn count(&self, now: u64) -> u32 {
        if self.control & PSTAT == 0 {
            return self.count;
        }
        let edges = |t: u64| t * self.rate() / TB_HZ;
        self.count.wrapping_add((edges(now) - edges(self.since.min(now))) as u32)
    }

    /// `off` is the register offset from 0xCD006C00.
    pub fn read32(&self, off: u32, now: u64) -> u32 {
        match off {
            0x0 => self.control,
            0x4 => self.volume,
            0x8 => self.count(now),
            0xc => self.interrupt_timing,
            _ => 0,
        }
    }

    pub fn write32(&mut self, off: u32, v: u32, now: u64) {
        match off {
            0x0 => {
                self.count = if v & SCRESET != 0 { 0 } else { self.count(now) };
                self.since = now;
                let int = self.control & AIINT & !v;
                self.control = (v & !(SCRESET | AIINT)) | int;
            }
            0x4 => self.volume = v,
            0xc => self.interrupt_timing = v,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AIInit's rate probe times the first tick after switching rates and
    /// retries while it is 28.5–34.5 µs. With a free-running clock the tick
    /// comes within one period, at the next edge.
    #[test]
    fn counts_at_the_selected_rate() {
        let mut ai = Ai::default();
        ai.write32(0, SCRESET, 0);
        ai.write32(0, PSTAT, 0);
        assert_eq!(ai.read32(8, TB_HZ / 1000), 32);

        let t0 = TB_HZ / 1000 + 100;
        ai.write32(0, PSTAT | AISFR, t0);
        let start = ai.read32(8, t0);
        let tick = (t0..).find(|&t| ai.read32(8, t) != start).unwrap() - t0;
        assert!(tick <= TB_HZ / 48_000, "{tick} ticks");
        assert_eq!(ai.read32(8, t0 + TB_HZ) - start, 48_000);

        ai.write32(0, 0, t0 + 10 * TB_HZ);
        assert_eq!(ai.read32(8, t0 + 20 * TB_HZ), ai.read32(8, t0 + 10 * TB_HZ));
    }
}
