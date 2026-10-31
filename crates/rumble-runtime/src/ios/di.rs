//! `/dev/di`: the disc drive, as a WiiWare title sees it with no disc in
//! the slot. Ioctls answer with DI results rather than IOS error codes.

use super::Buffers;
use crate::mem::{Fault, Memory};

/// DI results.
const SUCCESS: i32 = 1;
const DRIVE_ERROR: i32 = 2;

/// Cover register (DICVR) bit 0: cover open / no disc.
const CVR_OPEN: u32 = 1 << 0;
const CVR_INT: u32 = 1 << 2;

/// Error register for an empty slot: status "no disc", sense "medium not
/// present".
const NO_DISC_ERROR: u32 = 0x0302_3a00;

pub struct Di {
    cover: u32,
}

impl Default for Di {
    fn default() -> Self {
        Self { cover: CVR_OPEN }
    }
}

impl Di {
    pub fn ioctl(&mut self, m: &mut Memory, num: u32, b: &Buffers) -> Result<i32, Fault> {
        Ok(match num {
            // Inquiry: drive revision and date, as a retail drive reports.
            0x12 => {
                let mut info = [0u8; 0x20];
                info[0..4].copy_from_slice(&0x0000_0002u32.to_be_bytes());
                info[4..8].copy_from_slice(&0x2008_0714u32.to_be_bytes());
                info[8..12].copy_from_slice(&0x4100_0000u32.to_be_bytes());
                b.out(m, 0, &info)?;
                SUCCESS
            }
            // Get cover register.
            0x7a => {
                b.out_u32(m, 0, self.cover)?;
                SUCCESS
            }
            // Clear cover interrupt.
            0x86 => {
                self.cover &= !CVR_INT;
                SUCCESS
            }
            // Get cover status: 1 = no disc, 2 = disc inserted.
            0x88 => {
                b.out_u32(m, 0, 1)?;
                SUCCESS
            }
            // Status and control registers (DISR, DICR): idle.
            0x95 | 0x96 => {
                b.out_u32(m, 0, 0)?;
                SUCCESS
            }
            // Request error.
            0xe0 => {
                b.out_u32(m, 0, NO_DISC_ERROR)?;
                SUCCESS
            }
            // Notify reset, set spinup flag, interrupt masks, reset, stop
            // motor, audio buffer config: nothing to do without a disc.
            0x7e | 0x7f | 0x85 | 0x87 | 0x89 | 0x8a | 0xe3 | 0xe4 => SUCCESS,
            // Reads, disc ID, partitions: there is no disc.
            _ => DRIVE_ERROR,
        })
    }
}
