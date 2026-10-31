//! Video Interface registers at 0xCC002000. Only the beam and the four
//! display interrupts are modelled: enough for the SDK's retrace handler,
//! which everything frame-paced waits on. Scanout itself comes from the GX
//! side later.

use crate::cpu::TB_HZ;

/// Register offsets (16-bit).
const DCR: u32 = 0x02; // display config; bits 8-9 video format
const VCT: u32 = 0x2c; // current line, 1-based
const HCT: u32 = 0x2e; // current pixel
const DI0: u32 = 0x30; // DI0-DI3 at 0x30/0x34/0x38/0x3C, high half first
const VICLK: u32 = 0x6c; // bit 0: 54 MHz (progressive)

/// High half of a display interrupt register.
const DI_STATUS: u16 = 0x8000;
const DI_ENABLE: u16 = 0x1000;
const DI_VCT: u16 = 0x03ff;

pub struct Vi {
    regs: [u16; 0x80],
    /// Current line of the frame, 1-based.
    line: u16,
    /// Timebase at which the beam reaches the next line.
    next_line: u64,
    /// Display interrupts raised so far (enabled ones only).
    pub interrupts: u64,
}

impl Default for Vi {
    fn default() -> Self {
        Self { regs: [0; 0x80], line: 1, next_line: 0, interrupts: 0 }
    }
}

impl Vi {
    /// `off` is the register offset from 0xCC002000.
    pub fn read16(&self, off: u32) -> u16 {
        match off {
            VCT => self.line,
            // Line-granular beam: report the start of the line.
            HCT => 1,
            _ => self.regs.get(off as usize / 2).copied().unwrap_or(0),
        }
    }

    pub fn write16(&mut self, off: u32, v: u16) {
        if let Some(r) = self.regs.get_mut(off as usize / 2) {
            *r = v;
        }
    }

    /// Lines per frame and timebase ticks per line for the current mode.
    /// Interlaced NTSC/MPAL is 525 lines at 29.97 Hz, PAL 625 at 25 Hz;
    /// progressive scans the same lines twice as fast.
    fn timing(&self) -> (u16, u64) {
        let pal = (self.regs[DCR as usize / 2] >> 8) & 3 == 1;
        let (lines, line_hz) = if pal { (625, 15_625) } else { (525, 15_734) };
        let progressive = self.regs[VICLK as usize / 2] & 1 != 0;
        let line_hz = if progressive { line_hz * 2 } else { line_hz };
        (lines, TB_HZ / line_hz)
    }

    /// Timebase at which [`Vi::advance`] next has work to do.
    pub fn next_event(&self) -> u64 {
        self.next_line
    }

    /// Moves the beam up to timebase `now`, latching display interrupts for
    /// each line it enters.
    pub fn advance(&mut self, now: u64) {
        let (lines, ticks) = self.timing();
        while now >= self.next_line {
            self.next_line += ticks;
            self.line = if self.line >= lines { 1 } else { self.line + 1 };
            for k in 0..4 {
                let hi = &mut self.regs[(DI0 as usize + 4 * k) / 2];
                if *hi & DI_VCT == self.line {
                    *hi |= DI_STATUS;
                    if *hi & DI_ENABLE != 0 {
                        self.interrupts += 1;
                    }
                }
            }
        }
    }

    /// Any enabled display interrupt pending.
    pub fn irq_line(&self) -> bool {
        (0..4).any(|k| self.regs[(DI0 as usize + 4 * k) / 2] & (DI_STATUS | DI_ENABLE) == DI_STATUS | DI_ENABLE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retrace_once_per_field() {
        let mut vi = Vi::default();
        // DI0 near the top of field 1, DI1 near the top of field 2.
        vi.write16(DI0, DI_ENABLE | 24);
        vi.write16(DI0 + 4, DI_ENABLE | 287);
        let (_, ticks) = vi.timing();

        let mut fired = Vec::new();
        for t in (0..ticks * 1050).step_by(ticks as usize) {
            vi.advance(t);
            for k in 0..2u32 {
                let hi = vi.read16(DI0 + 4 * k);
                if hi & DI_STATUS != 0 {
                    assert!(vi.irq_line());
                    fired.push((k, vi.read16(VCT)));
                    vi.write16(DI0 + 4 * k, hi & !DI_STATUS);
                }
            }
            assert!(!vi.irq_line());
        }
        assert_eq!(fired, [(0, 24), (1, 287), (0, 24), (1, 287)]);
    }
}
