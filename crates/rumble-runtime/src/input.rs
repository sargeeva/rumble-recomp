//! Host input as Wii Remotes. The host (window thread) sets each channel's
//! button bits; the KPAD/WPAD replacements (`hle/wpad.rs`) hand them to the
//! game. Shared through atomics so the emulation thread never waits on it.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// Wii Remote button bits (WPAD/KPAD), in the remote's own orientation:
/// "up" is towards the IR end when held upright.
pub mod button {
    pub const LEFT: u32 = 0x0001;
    pub const RIGHT: u32 = 0x0002;
    pub const DOWN: u32 = 0x0004;
    pub const UP: u32 = 0x0008;
    pub const PLUS: u32 = 0x0010;
    pub const TWO: u32 = 0x0100;
    pub const ONE: u32 = 0x0200;
    pub const B: u32 = 0x0400;
    pub const A: u32 = 0x0800;
    pub const MINUS: u32 = 0x1000;
    pub const HOME: u32 = 0x8000;

    /// Names accepted on the command line and in mappings.
    pub fn by_name(name: &str) -> Option<u32> {
        Some(match name.to_ascii_lowercase().as_str() {
            "left" => LEFT,
            "right" => RIGHT,
            "down" => DOWN,
            "up" => UP,
            "plus" | "+" => PLUS,
            "2" | "two" => TWO,
            "1" | "one" => ONE,
            "b" => B,
            "a" => A,
            "minus" | "-" => MINUS,
            "home" => HOME,
            _ => return None,
        })
    }
}

pub const CHANNELS: usize = 4;

/// What the host is holding, per channel.
#[derive(Default)]
pub struct Remotes {
    buttons: [AtomicU32; CHANNELS],
    /// Scripted presses, merged with what the host holds.
    scripted: [AtomicU32; CHANNELS],
    /// Bit n: channel n has a remote.
    connected: AtomicU32,
    /// With `latching`, the game reads `latched`, set from the rest by
    /// `latch` at points the emulator chooses, rather than what is held
    /// that instant: the same input then reaches the game at the same
    /// instruction whenever it is played, so a recording replays exactly.
    latching: AtomicBool,
    latched: [AtomicU32; CHANNELS],
}

impl Remotes {
    pub fn connect(&self, chan: usize, on: bool) {
        let bit = 1 << chan;
        if on {
            self.connected.fetch_or(bit, Ordering::Relaxed);
        } else {
            self.connected.fetch_and(!bit, Ordering::Relaxed);
        }
    }

    pub fn connected(&self, chan: usize) -> bool {
        chan < CHANNELS && self.connected.load(Ordering::Relaxed) & (1 << chan) != 0
    }

    pub fn set_buttons(&self, chan: usize, bits: u32) {
        if let Some(b) = self.buttons.get(chan) {
            b.store(bits, Ordering::Relaxed);
        }
    }

    pub fn set_scripted(&self, chan: usize, bits: u32) {
        if let Some(b) = self.scripted.get(chan) {
            b.store(bits, Ordering::Relaxed);
        }
    }

    /// What the game sees on `chan`.
    pub fn buttons(&self, chan: usize) -> u32 {
        if self.latching.load(Ordering::Relaxed) {
            return self.latched.get(chan).map_or(0, |b| b.load(Ordering::Relaxed));
        }
        self.held(chan)
    }

    fn held(&self, chan: usize) -> u32 {
        let held = |a: &[AtomicU32; CHANNELS]| a.get(chan).map_or(0, |b| b.load(Ordering::Relaxed));
        held(&self.buttons) | held(&self.scripted)
    }

    /// From now on the game sees what is held at each `latch`.
    pub fn latch(&self) {
        self.latching.store(true, Ordering::Relaxed);
        for (chan, l) in self.latched.iter().enumerate() {
            l.store(self.held(chan), Ordering::Relaxed);
        }
    }
}

/// The emulator's side: the shared state plus what the game last read, for
/// pressed/released edges.
#[derive(Default)]
pub struct Input {
    pub remotes: Arc<Remotes>,
    pub last_read: [u32; CHANNELS],
}
