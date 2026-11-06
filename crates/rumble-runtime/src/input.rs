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
    /// Set when the game draws its Pokemon selection screen (where players
    /// join multiplayer), taken by the host to know it is showing.
    selecting: AtomicBool,
    /// Players' names, which the game shows over their health bars
    /// (`hle::nametag`).
    names: std::sync::Mutex<[Option<String>; CHANNELS]>,
    /// End Multiplayer was chosen in the game's pause menu, for the host to
    /// take (`hle::pause`).
    end_requested: AtomicBool,
    /// "True stick controls": each remote has a Classic Controller, whose
    /// left stick (`stick`, x then y as f32 bits, -1 to 1, y up) the game
    /// reads as analog movement (`hle/wpad.rs`).
    classic: AtomicBool,
    stick: [AtomicU32; CHANNELS * 2],
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

    /// Shows `name` over player `chan`'s health bar (none: the game's own).
    pub fn set_name(&self, chan: usize, name: Option<&str>) {
        let mut names = self.names.lock().unwrap();
        if let Some(slot) = names.get_mut(chan) {
            if slot.as_deref() != name {
                *slot = name.map(str::to_string);
            }
        }
    }

    pub fn name(&self, chan: usize) -> Option<String> {
        self.names.lock().unwrap().get(chan).cloned().flatten()
    }

    pub fn set_classic(&self, on: bool) {
        self.classic.store(on, Ordering::Relaxed);
    }

    pub fn classic(&self) -> bool {
        self.classic.load(Ordering::Relaxed)
    }

    /// Player `chan`'s left stick, -1 to 1 each way, y up.
    pub fn set_stick(&self, chan: usize, [x, y]: [f32; 2]) {
        if chan < CHANNELS {
            self.stick[chan * 2].store(x.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
            self.stick[chan * 2 + 1].store(y.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
        }
    }

    pub fn stick(&self, chan: usize) -> [f32; 2] {
        if chan >= CHANNELS {
            return [0.0; 2];
        }
        [f32::from_bits(self.stick[chan * 2].load(Ordering::Relaxed)), f32::from_bits(self.stick[chan * 2 + 1].load(Ordering::Relaxed))]
    }

    pub fn request_end(&self) {
        self.end_requested.store(true, Ordering::Relaxed);
    }

    pub fn take_end_requested(&self) -> bool {
        self.end_requested.swap(false, Ordering::Relaxed)
    }

    /// The game drew its Pokemon selection screen.
    pub fn mark_selecting(&self) {
        self.selecting.store(true, Ordering::Relaxed);
    }

    /// Whether the selection screen was drawn since the last call.
    pub fn take_selecting(&self) -> bool {
        self.selecting.swap(false, Ordering::Relaxed)
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
