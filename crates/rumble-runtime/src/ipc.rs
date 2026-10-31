//! Hollywood IPC registers at 0xCD000000 and the PPC interrupt flag/mask
//! pair they drive. IOS itself is high-level emulated in `ios.rs`; this is
//! only the doorbell protocol the SDK's IPC driver speaks.

use std::collections::VecDeque;

/// HW_PPCIRQFLAG bit for IPC.
pub const IRQ_IPC: u32 = 1 << 30;

/// HW_IPC_PPCCTRL bits.
const X1: u32 = 1 << 0; // PPC: request in PPCMSG is ready
const Y2: u32 = 1 << 1; // IOS: request acknowledged (write 1 to clear)
const Y1: u32 = 1 << 2; // IOS: reply in ARMMSG is ready (write 1 to clear)
const IY1: u32 = 1 << 4; // interrupt on Y1
const IY2: u32 = 1 << 5; // interrupt on Y2

#[derive(Default)]
pub struct Ipc {
    ppcmsg: u32,
    armmsg: u32,
    ctrl: u32,
    /// HW_PPCIRQFLAG (write 1 to clear) and HW_PPCIRQMASK.
    irq_flags: u32,
    irq_mask: u32,
    /// Finished requests waiting for the PPC to be ready for a reply.
    replies: VecDeque<u32>,
}

impl Ipc {
    /// `off` is the register offset from 0xCD000000.
    pub fn read32(&self, off: u32) -> u32 {
        match off {
            0x00 => self.ppcmsg,
            0x04 => self.ctrl,
            0x08 => self.armmsg,
            0x30 => self.irq_flags,
            0x34 => self.irq_mask,
            _ => 0,
        }
    }

    /// Returns the address of a request the PPC just sent, which the caller
    /// executes and hands back through [`Ipc::push_reply`].
    pub fn write32(&mut self, off: u32, v: u32) -> Option<u32> {
        let mut sent = None;
        match off {
            0x00 => self.ppcmsg = v,
            0x04 => {
                self.ctrl &= !(v & (Y1 | Y2));
                self.ctrl = (self.ctrl & !(IY1 | IY2)) | (v & (IY1 | IY2));
                if v & X1 != 0 {
                    // IOS takes the request at once and acknowledges it.
                    sent = Some(self.ppcmsg);
                    self.ctrl |= Y2;
                }
            }
            0x30 => self.irq_flags &= !v,
            0x34 => self.irq_mask = v,
            _ => {}
        }
        self.deliver();
        self.update_irq();
        sent
    }

    pub fn push_reply(&mut self, req: u32) {
        self.replies.push_back(req);
        self.deliver();
        self.update_irq();
    }

    /// The IPC flag is re-raised after every register write for as long as
    /// an enabled Y1/Y2 is pending, so clearing it early does not lose a
    /// reply that arrived meanwhile.
    fn update_irq(&mut self) {
        if (self.ctrl & Y1 != 0 && self.ctrl & IY1 != 0) || (self.ctrl & Y2 != 0 && self.ctrl & IY2 != 0) {
            self.irq_flags |= IRQ_IPC;
        }
    }

    /// Like IOS, posts the next reply only once the PPC has cleared the
    /// previous ack/reply and its interrupt.
    fn deliver(&mut self) {
        if self.ctrl & (Y1 | Y2) != 0 || self.irq_flags & IRQ_IPC != 0 {
            return;
        }
        if let Some(req) = self.replies.pop_front() {
            self.armmsg = req;
            self.ctrl |= Y1;
        }
    }

    /// Hollywood's interrupt line into the Processor Interface.
    pub fn irq_line(&self) -> bool {
        self.irq_flags & self.irq_mask != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sequence the SDK's IPC driver goes through for one request.
    #[test]
    fn request_ack_reply() {
        let mut ipc = Ipc::default();
        ipc.write32(0x34, IRQ_IPC);
        ipc.write32(0x04, 0x38);
        ipc.write32(0x00, 0x0123_4560);
        assert_eq!(ipc.write32(0x04, 0x30 | X1), Some(0x0123_4560));
        ipc.push_reply(0x0123_4560);

        // Ack first; the reply waits behind it.
        assert!(ipc.irq_line());
        assert_eq!(ipc.read32(0x04) & 0x22, 0x22);
        ipc.write32(0x04, 0x30 | Y2);
        assert_eq!(ipc.read32(0x04) & Y1, 0);
        ipc.write32(0x30, IRQ_IPC);

        // Then the reply.
        assert!(ipc.irq_line());
        assert_eq!(ipc.read32(0x04) & 0x14, 0x14);
        assert_eq!(ipc.read32(0x08), 0x0123_4560);
        ipc.write32(0x04, 0x30 | Y1);
        ipc.write32(0x30, IRQ_IPC);
        assert!(!ipc.irq_line());
    }
}
