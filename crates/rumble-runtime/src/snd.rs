//! Player volumes per kind of sound. The game plays everything through
//! NW4R's sound players (named in `sound/waza_p.brsar`): player 2 is the
//! music (SOUND_PLAYER_BGM), player 3 the Pokémon's cries
//! (SOUND_PLAYER_POKEVOICE), player 0 the Wii Remote speaker, and the rest
//! sound effects. Each player's volume scales every sound it plays, so
//! the player's settings are applied there: the game's own volume for a
//! player (1, or what it set, say for a fade) times the setting.

use crate::mem::Memory;

/// This DOL's `SoundArchivePlayer`: the game's sound work object (an
/// r13-relative pointer), the player at +0x3F0 in it; the player count at
/// +0x3C and the `SoundPlayer` array at +0x40, 0x64 bytes each, with the
/// volume (f32) at +0x2C.
const R13: u32 = 0x804b_da20;
const SOUND_WORK: u32 = R13 - 0x4838;
const ARCHIVE_PLAYER: u32 = 0x3f0;
const PLAYER_COUNT: u32 = 0x3c;
const PLAYERS: u32 = 0x40;
const PLAYER_SIZE: u32 = 0x64;
const PLAYER_VOLUME: u32 = 0x2c;

const MUSIC_PLAYER: u32 = 2;
const VOICE_PLAYER: u32 = 3;
const REMOTE_PLAYER: u32 = 0;

/// Volumes (0-1) per kind of sound.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Volumes {
    pub music: f32,
    pub effects: f32,
    pub voices: f32,
}

impl Default for Volumes {
    fn default() -> Self {
        Self { music: 1.0, effects: 1.0, voices: 1.0 }
    }
}

/// Keeps the game's players at its own volume times the settings.
#[derive(Default)]
pub struct PlayerVolumes {
    /// Per player: the game's volume and what was last written.
    players: Vec<(f32, f32)>,
}

impl PlayerVolumes {
    pub fn apply(&mut self, mem: &mut Memory, v: Volumes) {
        let Ok(work) = mem.r32(SOUND_WORK) else { return };
        if work & 0xfe00_0000 != 0x8000_0000 {
            return;
        }
        let ap = work + ARCHIVE_PLAYER;
        let (Ok(count), Ok(array)) = (mem.r32(ap + PLAYER_COUNT), mem.r32(ap + PLAYERS)) else { return };
        if count == 0 || count > 64 || array & 0xfe00_0000 != 0x8000_0000 {
            return;
        }
        self.players.resize(count as usize, (1.0, 1.0));
        for p in 0..count {
            let at = array + p * PLAYER_SIZE + PLAYER_VOLUME;
            let Ok(bits) = mem.r32(at) else { return };
            let now = f32::from_bits(bits);
            let (game, written) = &mut self.players[p as usize];
            // Anything other than what was last written is the game's own.
            if now != *written {
                *game = now;
            }
            let setting = match p {
                MUSIC_PLAYER => v.music,
                VOICE_PLAYER => v.voices,
                REMOTE_PLAYER => 1.0,
                _ => v.effects,
            };
            let want = *game * setting;
            if want != now {
                let _ = mem.w32(at, want.to_bits());
            }
            *written = want;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The setting scales each kind's players; a volume the game sets
    /// later (a fade) is kept and scaled too.
    #[test]
    fn scales_players_by_kind() {
        let mut mem = Memory::default();
        let (work, array) = (0x8010_0000, 0x8020_0000);
        mem.w32(SOUND_WORK, work).unwrap();
        mem.w32(work + ARCHIVE_PLAYER + PLAYER_COUNT, 5).unwrap();
        mem.w32(work + ARCHIVE_PLAYER + PLAYERS, array).unwrap();
        let vol = |p: u32| array + p * PLAYER_SIZE + PLAYER_VOLUME;
        for p in 0..5 {
            mem.w32(vol(p), 1f32.to_bits()).unwrap();
        }
        let get = |mem: &mut Memory, p| f32::from_bits(mem.r32(vol(p)).unwrap());
        let mut pv = PlayerVolumes::default();
        let v = Volumes { music: 0.5, effects: 0.25, voices: 0.0 };
        pv.apply(&mut mem, v);
        assert_eq!([0, 1, 2, 3, 4].map(|p| get(&mut mem, p)), [1.0, 0.25, 0.5, 0.0, 0.25]);

        pv.apply(&mut mem, v);
        assert_eq!(get(&mut mem, 2), 0.5, "not compounded");

        mem.w32(vol(2), 0.4f32.to_bits()).unwrap(); // the game fades the music
        pv.apply(&mut mem, v);
        assert_eq!(get(&mut mem, 2), 0.2);
    }
}
