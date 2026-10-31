//! `/dev/stm`: power and reset button events, VI dimming and the slot LED.
//! The event hook stays pending until a button is pressed; nothing presses
//! one yet, so it only completes when the game releases it.

use super::Buffers;
use crate::mem::{Fault, Memory};

const EVENT_HOOK: u32 = 0x1000;
const RELEASE_EVENT_HOOK: u32 = 0x3002;

#[derive(Default)]
pub struct Stm {
    /// The pending event-hook request, if any.
    hook: Option<u32>,
}

impl Stm {
    /// Returns true when `req` is now waiting for an event.
    pub fn hook(&mut self, num: u32, req: u32) -> bool {
        if num == EVENT_HOOK && self.hook.is_none() {
            self.hook = Some(req);
            return true;
        }
        false
    }

    /// Ioctls on `/dev/stm/immediate`. Hot reset, shutdown, VI dimming and
    /// LED control are accepted without effect.
    pub fn immediate(&mut self, m: &mut Memory, num: u32, _b: &Buffers) -> Result<(i32, Vec<u32>), Fault> {
        let released = if num == RELEASE_EVENT_HOOK { self.release(m)? } else { Vec::new() };
        Ok((0, released))
    }

    /// Completes the pending event hook with "no event".
    pub fn release(&mut self, m: &mut Memory) -> Result<Vec<u32>, Fault> {
        let Some(req) = self.hook.take() else { return Ok(Vec::new()) };
        let out = m.r32(req + 0x18)?;
        if out != 0 {
            m.w32(out, 0)?;
        }
        m.w32(req + 4, 0)?;
        m.w32(req + 8, super::IOCTL)?;
        m.w32(req, super::REPLY)?;
        Ok(vec![req])
    }
}
