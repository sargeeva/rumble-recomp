//! DSP interface registers at 0xCC005000: mailboxes, the DSP control/status
//! register (DSPCR), ARAM DMA and audio (AI) DMA. The DSP itself is
//! high-level emulated: it never runs code, it just answers the handshakes
//! the SDK waits on.
//!
//! Booting a task: once released from halt, the boot ROM mails the CPU,
//! then takes the task description as (command, value) mail pairs from
//! `__DSP_boot_task`, ending with 0x80F3D001 and the init vector. The
//! microcode then starts and announces itself with `DSP_INIT` and a DSP
//! interrupt; the SDK's handler answers by calling the task's init callback.
//!
//! The running task is taken to be AX. Each audio frame the CPU mails
//! 0xBABExxxx and the command list's address; the microcode works through
//! the list and yields (`DSP_YIELD`), which the SDK's handler answers with
//! "continue" and AX's resume callback. The list itself is run on the host
//! (`ax.rs`), by `Memory` as the mail arrives (`pending_cmdlist`).
//!
//! Each buffer the audio DMA starts playing is noted (`played`) for
//! `Memory` to pass the samples on to the host.

use crate::cpu::TB_HZ;
use std::collections::VecDeque;

/// DSPCR bits.
const RES: u16 = 1 << 0; // reset, self-clearing
const PIINT: u16 = 1 << 1; // assert interrupt to the DSP, self-clearing
const HALT: u16 = 1 << 2;
const AIDINT: u16 = 1 << 3; // interrupt status bits are write-1-to-clear
const AIDINT_MASK: u16 = 1 << 4;
const ARINT: u16 = 1 << 5;
const ARINT_MASK: u16 = 1 << 6;
const DSPINT: u16 = 1 << 7;
const DSPINT_MASK: u16 = 1 << 8;
const INT_STATUS: u16 = AIDINT | ARINT | DSPINT;
/// Read-only: ARAM DMA in progress, DSP running its init code.
const READ_ONLY: u16 = (1 << 9) | (1 << 10);

/// Audio DMA control (0x5036): enable, and length in 32-byte blocks.
const AI_DMA_ENABLE: u16 = 0x8000;
const AI_DMA_BLOCKS: u16 = 0x7fff;
/// Stereo 16-bit samples per 32-byte block.
const SAMPLES_PER_BLOCK: u64 = 8;

/// Mail-valid bit in the high half of a mailbox.
const MAIL_VALID: u16 = 0x8000;
/// Sent once the DSP is released from halt, like the boot ROM's "ready"
/// mail. The SDK only checks the valid bit.
const READY_MAIL: u32 = 0x8071_feed;
/// Last boot command: the next mail is the init vector, then the task runs.
const BOOT_INIT_VECTOR: u32 = 0x80f3_d001;

/// Task-to-CPU mails, as the SDK's DSP handler names them.
const DSP_INIT: u32 = 0xdcd1_0000;
const DSP_RESUME: u32 = 0xdcd1_0001;
const DSP_YIELD: u32 = 0xdcd1_0002;
/// CPU-to-task mails.
const MAIL_RESUME: u32 = 0xcdd1_0000;
const MAIL_CONTINUE: u32 = 0xcdd1_0003;
const MAIL_CMDLIST: u32 = 0xbabe_0000;

/// How many task mails the `boot` report keeps.
const TASK_MAIL_LOG: usize = 16;

pub struct Dsp {
    /// CPU -> DSP mailbox (0x5000/0x5002).
    cpu_mbox: [u16; 2],
    /// DSP -> CPU mailbox (0x5004/0x5006), and mails waiting behind it,
    /// each with whether it raises DSPINT when it arrives.
    dsp_mbox: [u16; 2],
    outbox: VecDeque<(u32, bool)>,
    dspcr: u16,
    /// 0x5012 ARAM size, 0x5016 ARAM mode, 0x501A refresh.
    aram_cfg: [u16; 3],
    /// 0x5020 main-memory address, 0x5024 ARAM address, 0x5028 count.
    dma: [u16; 6],
    /// 0x5030/0x5032 audio DMA source, 0x5036 control.
    ai_dma: [u16; 4],
    /// The buffer the audio DMA is playing: when it started, its length in
    /// blocks, and when the next one is loaded (`u64::MAX` when stopped).
    ai_buffer_start: u64,
    ai_buffer_blocks: u64,
    ai_next_reload: u64,
    /// Current timebase and audio DMA sample rate, from `Memory`.
    now: u64,
    rate: u64,
    /// Boot mails received while the boot ROM is waiting for a task.
    boot: Option<Vec<u32>>,
    /// Length of the command list whose address the next mail carries.
    cmdlist: Option<u16>,
    /// The first mails sent to the running task, for the `boot` report.
    pub task_mail: Vec<u32>,
    /// Audio frames (command lists) the task has processed.
    pub frames: u64,
    /// The last command list: address and length in bytes.
    pub last_cmdlist: (u32, u16),
    /// A command list to run now (its address).
    pub pending_cmdlist: Option<u32>,
    /// Audio DMA buffers started since `Memory` last looked: (address,
    /// bytes).
    pub played: Vec<(u32, u32)>,
    /// Audio DMA buffers started.
    pub ai_buffers: u64,
}

impl Default for Dsp {
    fn default() -> Self {
        Self {
            cpu_mbox: [0; 2],
            dsp_mbox: [0; 2],
            outbox: VecDeque::new(),
            dspcr: 0,
            aram_cfg: [0; 3],
            dma: [0; 6],
            ai_dma: [0; 4],
            ai_buffer_start: 0,
            ai_buffer_blocks: 0,
            ai_next_reload: u64::MAX,
            now: 0,
            rate: 32_000,
            boot: None,
            cmdlist: None,
            task_mail: Vec::new(),
            frames: 0,
            last_cmdlist: (0, 0),
            pending_cmdlist: None,
            played: Vec::new(),
            ai_buffers: 0,
        }
    }
}

impl Dsp {
    /// Brings the DSP's view of time and the audio DMA rate up to date;
    /// `Memory` calls this before every access.
    pub fn sync(&mut self, now: u64, rate: u64) {
        self.now = now;
        self.rate = rate;
    }

    /// `off` is the register offset from 0xCC005000.
    pub fn read16(&mut self, off: u32) -> u16 {
        match off {
            0x00 => self.cpu_mbox[0],
            0x02 => self.cpu_mbox[1],
            0x04 => self.dsp_mbox[0],
            0x06 => {
                // Reading the low half consumes the mail; the next queued
                // one takes its place.
                let lo = self.dsp_mbox[1];
                self.dsp_mbox[0] &= !MAIL_VALID;
                if let Some((mail, irq)) = self.outbox.pop_front() {
                    self.post(mail, irq);
                }
                lo
            }
            0x0a => self.dspcr,
            0x12 => self.aram_cfg[0],
            0x16 => self.aram_cfg[1],
            0x1a => self.aram_cfg[2],
            0x20..=0x2a => self.dma[(off as usize - 0x20) / 2],
            0x30 | 0x32 | 0x36 => self.ai_dma[(off as usize - 0x30) / 2],
            0x3a => self.ai_blocks_left() as u16,
            _ => 0,
        }
    }

    pub fn write16(&mut self, off: u32, v: u16) {
        match off {
            0x00 => self.cpu_mbox[0] = v,
            // The HLE DSP takes mail as soon as it is complete.
            0x02 => {
                let mail = (self.cpu_mbox[0] as u32) << 16 | v as u32;
                self.cpu_mbox = [self.cpu_mbox[0] & !MAIL_VALID, v];
                self.receive(mail);
            }
            0x0a => self.write_dspcr(v),
            0x12 => self.aram_cfg[0] = v,
            0x16 => self.aram_cfg[1] = v,
            0x1a => self.aram_cfg[2] = v,
            0x20..=0x2a => {
                self.dma[(off as usize - 0x20) / 2] = v;
                // Writing the low half of the count starts the transfer. The
                // Wii has no ARAM behind this port and nothing reads the data
                // back, so it completes at once without copying.
                if off == 0x2a {
                    self.dspcr |= ARINT;
                }
            }
            0x30 | 0x32 => self.ai_dma[(off as usize - 0x30) / 2] = v,
            0x36 => {
                let was_on = self.ai_dma[3] & AI_DMA_ENABLE != 0;
                self.ai_dma[3] = v;
                match (was_on, v & AI_DMA_ENABLE != 0) {
                    (false, true) => self.ai_reload(self.now),
                    (true, false) => self.ai_next_reload = u64::MAX,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn write_dspcr(&mut self, v: u16) {
        let was_halted = self.dspcr & HALT != 0;
        let keep = self.dspcr & (INT_STATUS | READ_ONLY) & !(v & INT_STATUS);
        self.dspcr = keep | (v & !(INT_STATUS | READ_ONLY | RES | PIINT));
        if v & RES != 0 {
            self.dsp_mbox = [0; 2];
            self.outbox.clear();
        }
        if was_halted && self.dspcr & HALT == 0 {
            self.send(READY_MAIL, false);
            self.boot = Some(Vec::new());
        }
    }

    /// Queues a mail to the CPU; it goes straight into the mailbox when that
    /// is empty.
    fn send(&mut self, mail: u32, irq: bool) {
        if self.dsp_mbox[0] & MAIL_VALID == 0 && self.outbox.is_empty() {
            self.post(mail, irq);
        } else {
            self.outbox.push_back((mail, irq));
        }
    }

    fn post(&mut self, mail: u32, irq: bool) {
        self.dsp_mbox = [(mail >> 16) as u16 | MAIL_VALID, mail as u16];
        if irq {
            self.dspcr |= DSPINT;
        }
    }

    fn receive(&mut self, mail: u32) {
        let Some(boot) = &mut self.boot else {
            self.task_receive(mail);
            return;
        };
        boot.push(mail);
        if boot.len() >= 2 && boot[boot.len() - 2] == BOOT_INIT_VECTOR {
            self.boot = None;
            self.send(DSP_INIT, true);
        }
    }

    /// Mail to the running (AX) task.
    fn task_receive(&mut self, mail: u32) {
        if self.task_mail.len() < TASK_MAIL_LOG {
            self.task_mail.push(mail);
        }
        if let Some(len) = self.cmdlist.take() {
            // The command list's address: the frame is done at once.
            self.last_cmdlist = (mail, len);
            self.pending_cmdlist = Some(mail);
            self.frames += 1;
            self.send(DSP_YIELD, true);
            return;
        }
        match mail {
            m if m & 0xffff_0000 == MAIL_CMDLIST => self.cmdlist = Some(m as u16),
            MAIL_RESUME => self.send(DSP_RESUME, true),
            // Continue after a yield: nothing to acknowledge.
            MAIL_CONTINUE => {}
            _ => {}
        }
    }

    fn ai_ticks(&self, blocks: u64) -> u64 {
        blocks * SAMPLES_PER_BLOCK * TB_HZ / self.rate
    }

    /// Starts playing the buffer the registers describe; like the hardware,
    /// signals AIDINT so the CPU can queue the next one.
    fn ai_reload(&mut self, at: u64) {
        let blocks = (self.ai_dma[3] & AI_DMA_BLOCKS) as u64;
        self.ai_buffer_start = at;
        self.ai_buffer_blocks = blocks;
        if blocks != 0 {
            self.dspcr |= AIDINT;
            self.ai_buffers += 1;
            let addr = (self.ai_dma[0] as u32) << 16 | self.ai_dma[1] as u32;
            // Only the latest few matter if nobody collects them.
            if self.played.len() < 64 {
                self.played.push((addr, blocks as u32 * 32));
            }
        }
        self.ai_next_reload = at + self.ai_ticks(blocks.max(1));
    }

    fn ai_blocks_left(&self) -> u64 {
        if self.ai_next_reload == u64::MAX {
            return 0;
        }
        let played = self.now.saturating_sub(self.ai_buffer_start) * self.rate / (SAMPLES_PER_BLOCK * TB_HZ);
        self.ai_buffer_blocks.saturating_sub(played)
    }

    /// Timebase at which [`Dsp::advance`] next has work to do.
    pub fn next_event(&self) -> u64 {
        self.ai_next_reload
    }

    /// Moves the audio DMA up to the current time (see [`Dsp::sync`]).
    pub fn advance(&mut self) {
        while self.ai_next_reload <= self.now {
            self.ai_reload(self.ai_next_reload);
        }
    }

    /// Any unmasked DSP-side interrupt (AI DMA, ARAM DMA, DSP mail).
    pub fn irq_line(&self) -> bool {
        let cr = self.dspcr;
        (cr & AIDINT != 0 && cr & AIDINT_MASK != 0)
            || (cr & ARINT != 0 && cr & ARINT_MASK != 0)
            || (cr & DSPINT != 0 && cr & DSPINT_MASK != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_init_handshake() {
        let mut d = Dsp::default();
        d.write16(0x0a, 0x08ac);
        let cr = d.read16(0x0a);
        d.write16(0x0a, cr | RES);
        assert_eq!(d.read16(0x0a) & RES, 0);

        d.write16(0x28, 0);
        d.write16(0x2a, 0x20);
        let cr = d.read16(0x0a);
        assert_ne!(cr & ARINT, 0);
        d.write16(0x0a, cr);
        assert_eq!(d.read16(0x0a) & ARINT, 0);

        let cr = d.read16(0x0a);
        d.write16(0x0a, cr & !0x800);
        assert_eq!(d.read16(0x04) & MAIL_VALID, 0);
        let cr = d.read16(0x0a);
        d.write16(0x0a, cr & !HALT);
        assert_ne!(d.read16(0x04) & MAIL_VALID, 0);
        d.read16(0x06);
        assert_eq!(d.read16(0x04) & MAIL_VALID, 0);
    }

    fn mail(d: &mut Dsp, m: u32) {
        d.write16(0x00, (m >> 16) as u16);
        d.write16(0x02, m as u16);
    }

    fn read_mail(d: &mut Dsp) -> u32 {
        let hi = d.read16(0x04);
        (hi as u32) << 16 | d.read16(0x06) as u32
    }

    /// What the SDK's DSP handler does on entry: acknowledge DSPINT
    /// without touching the other interrupt bits.
    fn ack_dspint(d: &mut Dsp) {
        let cr = d.read16(0x0a);
        d.write16(0x0a, cr & !(AIDINT | ARINT) | DSPINT);
    }

    /// Boots a task the way this game's `__DSP_boot_task` does.
    fn booted() -> Dsp {
        let mut d = Dsp::default();
        d.write16(0x0a, HALT);
        d.write16(0x0a, DSPINT_MASK);
        assert_ne!(d.read16(0x04) & MAIL_VALID, 0, "boot ROM ready mail");
        d.read16(0x06);
        let boot = [0x80f3_a001, 0x003d_95e0, 0x80f3_c002, 0, 0x80f3_a002, 0x2000, 0x80f3_b002, 0, 0x80f3_d001];
        for m in boot {
            mail(&mut d, m);
            assert!(!d.irq_line());
        }
        mail(&mut d, 0x10);
        d
    }

    #[test]
    fn task_boot_handshake() {
        let mut d = booted();
        assert!(d.irq_line());
        assert_eq!(read_mail(&mut d), DSP_INIT);
        ack_dspint(&mut d);
        assert!(!d.irq_line());
    }

    /// One AX frame: command list in, yield out; "continue" needs no answer.
    #[test]
    fn ax_frame_yields() {
        let mut d = booted();
        read_mail(&mut d);
        ack_dspint(&mut d);

        mail(&mut d, 0xbabe_0080);
        assert!(!d.irq_line());
        mail(&mut d, 0x0046_c720);
        assert!(d.irq_line());
        assert_eq!(read_mail(&mut d), DSP_YIELD);
        ack_dspint(&mut d);
        mail(&mut d, MAIL_CONTINUE);
        assert!(!d.irq_line());
        assert_eq!(d.frames, 1);
    }

    /// Mails the CPU has not read yet are not overwritten.
    #[test]
    fn mails_queue_behind_an_unread_one() {
        let mut d = booted();
        mail(&mut d, MAIL_RESUME);
        assert_eq!(read_mail(&mut d), DSP_INIT);
        assert_eq!(read_mail(&mut d), DSP_RESUME);
    }

    /// AX's output: 0x180-byte buffers (12 blocks, 96 samples at 32 kHz),
    /// one AIDINT per buffer, every 3 ms.
    #[test]
    fn ai_dma_interrupts_once_per_buffer() {
        let mut d = Dsp::default();
        d.sync(1000, 32_000);
        d.write16(0x0a, AIDINT_MASK);
        d.write16(0x30, 0x0046);
        d.write16(0x32, 0xc000);
        d.write16(0x36, 12);
        assert!(!d.irq_line());
        d.write16(0x36, 12 | AI_DMA_ENABLE);
        assert!(d.irq_line());
        assert_eq!(d.read16(0x3a), 12);

        let frame = TB_HZ * 3 / 1000;
        let mut irqs = 0;
        for t in (1000..1000 + 100 * frame).step_by(1000) {
            d.sync(t, 32_000);
            if t >= d.next_event() {
                d.advance();
            }
            if d.irq_line() {
                irqs += 1;
                let cr = d.read16(0x0a);
                d.write16(0x0a, cr & !(ARINT | DSPINT) | AIDINT);
            }
        }
        assert!((99..=101).contains(&irqs), "{irqs} interrupts in 300 ms");
        d.write16(0x36, 12);
        assert_eq!(d.next_event(), u64::MAX);
    }
}
