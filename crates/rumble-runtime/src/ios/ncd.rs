//! `/dev/net/ncd/manage`: the network configuration daemon. Games read the
//! console's wireless MAC address through it (Pokémon Rumble does before
//! creating a save). There is no network: the config is empty and the link
//! reports as wired.
//!
//! Every ioctlv writes its result word to the first output buffer, except
//! GetConfig, which puts the config there and the result in the second.

use super::Buffers;
use crate::mem::{Fault, Memory};

const LOCK_WIRELESS_DRIVER: u32 = 0x1;
const UNLOCK_WIRELESS_DRIVER: u32 = 0x2;
const GET_CONFIG: u32 = 0x3;
const GET_LINK_STATUS: u32 = 0x7;
const GET_WIRELESS_MAC_ADDRESS: u32 = 0x8;

/// A fixed address in Nintendo's range (00:17:AB).
pub const MAC_ADDRESS: [u8; 6] = [0x00, 0x17, 0xab, 0x52, 0x55, 0x4d];
const LINK_WIRED: u32 = 1;

pub fn ioctlv(m: &mut Memory, num: u32, b: &Buffers) -> Result<i32, Fault> {
    let mut result_buffer = 0;
    match num {
        LOCK_WIRELESS_DRIVER | UNLOCK_WIRELESS_DRIVER => {}
        GET_CONFIG => {
            if let Some(&(_, len)) = b.output.first() {
                b.out(m, 0, &vec![0; len as usize])?;
            }
            result_buffer = 1;
        }
        GET_LINK_STATUS => {
            let mut out = [0u8; 8];
            out[4..].copy_from_slice(&LINK_WIRED.to_be_bytes());
            b.out(m, 0, &out)?;
        }
        GET_WIRELESS_MAC_ADDRESS => b.out(m, 1, &MAC_ADDRESS)?,
        _ => {}
    }
    if b.output.len() > result_buffer {
        b.out_u32(m, result_buffer, 0)?;
    }
    Ok(0)
}
