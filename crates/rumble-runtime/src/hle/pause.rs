//! "End Multiplayer" in the game's pause menu (see `docs/multiplayer.md`).
//!
//! The pause menu (`CPauseMenu`) builds a list of entries, each a message
//! and an action, and shows it with a list widget, which returns the chosen
//! entry's action. In the hub it has a Multiplayer entry (action 1), which
//! opens the Pokemon selection screen. While players 2-4 are in, the widget
//! call from the pause menu gets an End Multiplayer entry after it, with an
//! action of its own: chosen, it asks the host app to end multiplayer at
//! that screen and returns Multiplayer's action, so the screen opens.
//!
//! Its text is message 0x70 of `main.bmg`, one of nine unused ones ("????")
//! whose space it takes as the file is unpacked. The message lookup
//! returns a pointer to the text, so the entry's is the Continue entry's
//! moved by the distance between the two texts, checked before it is used.

use super::{push_guest_frame, Replacement, TAG_PAUSE};
use crate::cpu::{Cpu, SPR_LR};
use crate::mem::{Fault, Memory};
use rumble_formats::u8arc;

pub static REPLACEMENTS: &[Replacement] = &[Replacement {
    name: "ListWidget",
    sig: &[
        0x9421fee0, 0x7c0802a6, 0x90010124, 0x39610000, 0xdbe10110, 0xf3e10118, 0xdbc10100, 0xf3c10108, 0x48000001, 0x7c7a1b78,
        0x7c9b2378, 0x7cbc2b78, 0x7cdd3378, 0x7cfe3b78, 0x7d1f4378, 0x38600000, 0x48000001, 0x2c030000, 0x7c791b78, 0x41820008,
        0x48000001, 0x7f24cb78, 0x38610000, 0x48000001,
    ],
    run: list_widget,
}];

/// The unused messages, the one that becomes End Multiplayer, and its text.
const SPARE: std::ops::RangeInclusive<usize> = 0x6c..=0x74;
const END_MESSAGE: usize = 0x70;
const END_TEXT: &str = "End Multiplayer";
/// Continue, the pause menu's first entry.
const CONTINUE_MESSAGE: usize = 0x80;
const CONTINUE_TEXT: &str = "Continue";

/// The entries' actions: Multiplayer's, and End Multiplayer's (any the
/// menu doesn't know closes it).
const MULTIPLAYER: u32 = 1;
const END_MULTIPLAYER: u32 = 0x45;

#[derive(Default)]
pub struct State {
    /// Where the pause menu's widget call returns to.
    menu_return: Option<u32>,
    /// From the Continue text to End Multiplayer's, in bytes.
    text_offset: Option<i32>,
}

/// At boot: finds the pause menu's widget call, `addi r3..r7, r31, ...;
/// li r8, 1; bl`, after its entries are listed (r31 + 0x108 is the widget's
/// description, r31 + 0x90 the list).
pub fn configure(mem: &mut Memory) {
    let Ok(image) = mem.read_bytes(super::multiplay::IMAGE_START, 0x0060_0000) else { return };
    let word = |i: usize| u32::from_be_bytes(image[i * 4..i * 4 + 4].try_into().unwrap());
    const CALL: [u32; 6] = [0x387f0108, 0x389f0128, 0x38bf0020, 0x38df0070, 0x38ff0078, 0x39000001];
    let found: Vec<usize> = (0..image.len() / 4 - 7).filter(|&i| (0..6).all(|k| word(i + k) == CALL[k]) && word(i + 6) >> 26 == 18).collect();
    match found[..] {
        [i] => mem.multiplay.pause.menu_return = Some(super::multiplay::IMAGE_START + (i as u32 + 7) * 4),
        _ => eprintln!("pause menu: no End Multiplayer ({} matches)", found.len()),
    }
}

/// Called with each archive the game unpacks: gives `main.bmg`'s message
/// 0x70 the End Multiplayer text, in the space of the unused ones.
pub fn unpacked(mem: &mut Memory, out: &mut [u8]) {
    let Ok(files) = u8arc::files(out) else { return };
    let base = out.as_ptr() as usize;
    let Some((at, len)) = files.iter().find(|f| f.path.ends_with("/main.bmg")).map(|f| (f.data.as_ptr() as usize - base, f.data.len())) else {
        return;
    };
    drop(files);
    match patch_messages(&mut out[at..at + len]) {
        Some(offset) => mem.multiplay.pause.text_offset = Some(offset),
        None => eprintln!("pause menu: main.bmg isn't as expected, no End Multiplayer"),
    }
}

/// Puts End Multiplayer in message 0x70; returns the distance from the
/// Continue text to it.
fn patch_messages(bmg: &mut [u8]) -> Option<i32> {
    let be16 = |b: &[u8], o: usize| Some(u16::from_be_bytes(b.get(o..o + 2)?.try_into().ok()?) as usize);
    let be32 = |b: &[u8], o: usize| Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?) as usize);
    if bmg.get(..8) != Some(b"MESGbmg1") {
        return None;
    }
    let (mut inf, mut dat, mut o) = (None, None, 0x20);
    while o + 8 <= bmg.len() {
        let len = be32(bmg, o + 4)?;
        match &bmg[o..o + 4] {
            b"INF1" => inf = Some(o),
            b"DAT1" => dat = Some(o + 8),
            _ => {}
        }
        if len == 0 {
            break;
        }
        o += len;
    }
    let (inf, dat) = (inf?, dat?);
    let size = be16(bmg, inf + 10)?;
    let entry = |i: usize| inf + 0x10 + i * size;
    let text_at = |b: &[u8], i: usize| be32(b, entry(i)).map(|t| dat + t);
    let utf16 = |s: &str| s.encode_utf16().chain([0]).flat_map(|c| c.to_be_bytes()).collect::<Vec<u8>>();
    // The spare messages: "????", one after another.
    let first = text_at(bmg, *SPARE.start())?;
    let spare = utf16("????");
    for (k, i) in SPARE.enumerate() {
        let t = text_at(bmg, i)?;
        if t != first + k * spare.len() || bmg.get(t..t + spare.len())? != spare {
            return None;
        }
    }
    let text = utf16(END_TEXT);
    let last = text_at(bmg, *SPARE.end())?;
    if first + text.len() > last {
        return None;
    }
    bmg[first..first + text.len()].copy_from_slice(&text);
    // The others keep a "????": the last one's.
    for i in SPARE {
        let t = if i == END_MESSAGE { first } else { last };
        bmg[entry(i)..entry(i) + 4].copy_from_slice(&((t - dat) as u32).to_be_bytes());
    }
    let cont = text_at(bmg, CONTINUE_MESSAGE)?;
    (bmg.get(cont..cont + CONTINUE_TEXT.len() * 2 + 2)? == utf16(CONTINUE_TEXT)).then(|| first as i32 - cont as i32)
}

/// Whether anyone plays as players 2-4.
fn multiplayer(mem: &Memory) -> bool {
    (1..4).any(|chan| mem.input.remotes.connected(chan))
}

/// `ListWidget(desc, ...)`: from the pause menu, with players 2-4 in and a
/// Multiplayer entry, adds End Multiplayer after it, and returns through
/// `GuestReturn` (`chosen`) to see what was picked.
fn list_widget(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    if Some(cpu.spr[SPR_LR]) == mem.multiplay.pause.menu_return && multiplayer(mem) && add_entry(mem, cpu.gpr[3]).unwrap_or(false) {
        push_guest_frame(cpu, mem, 0, TAG_PAUSE)?;
        return super::lyt::enter(cpu, mem, 0x120, 0);
    }
    super::lyt::enter(cpu, mem, 0x120, 1)
}

/// Adds End Multiplayer to the pause menu's list, whose widget description
/// is at `desc` (the list's entries, then its count twice, at 4, 8 and
/// 0x10); the list itself (entries, count, room) is 0x78 before it.
fn add_entry(mem: &mut Memory, desc: u32) -> Result<bool, Fault> {
    let Some(offset) = mem.multiplay.pause.text_offset else { return Ok(false) };
    let list = desc - 0x78;
    let (entries, count, room) = (mem.r32(list)?, mem.r32(list + 4)?, mem.r32(list + 8)?);
    if count == 0 || count >= room || mem.r32(desc + 4)? != entries || mem.r32(desc + 8)? != count {
        return Ok(false);
    }
    let actions: Vec<u32> = (0..count).map(|i| mem.r32(entries + i * 8 + 4)).collect::<Result<_, _>>()?;
    if !actions.contains(&MULTIPLAYER) || actions.contains(&END_MULTIPLAYER) {
        return Ok(false);
    }
    // The text, where Continue's says it is.
    let text = mem.r32(entries)?.wrapping_add_signed(offset);
    let expected: Vec<u8> = END_TEXT.encode_utf16().chain([0]).flat_map(|c| c.to_be_bytes()).collect();
    if mem.read_bytes(text, expected.len() as u32).ok().as_deref() != Some(&expected[..]) {
        return Ok(false);
    }
    mem.w32(entries + count * 8, text)?;
    mem.w32(entries + count * 8 + 4, END_MULTIPLAYER)?;
    mem.w32(list + 4, count + 1)?;
    mem.w32(desc + 8, count + 1)?;
    mem.w32(desc + 0x10, count + 1)?;
    Ok(true)
}

/// Back from the pause menu's widget: End Multiplayer asks for multiplayer
/// to end at the Pokemon selection screen, which Multiplayer's action
/// opens.
pub fn chosen(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let sp = cpu.gpr[1];
    if cpu.gpr[3] == END_MULTIPLAYER {
        mem.input.remotes.request_end();
        cpu.gpr[3] = MULTIPLAYER;
    }
    cpu.pc = mem.r32(sp + 8)? & !3;
    cpu.gpr[1] = mem.r32(sp)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The game's main.bmg gets End Multiplayer in message 0x70.
    #[test]
    fn patches_messages() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/assets/en_US/msg.arc/main.bmg");
        let Ok(mut bmg) = std::fs::read(path) else { return };
        let len = bmg.len();
        let offset = patch_messages(&mut bmg).expect("patched");
        assert_eq!(bmg.len(), len);
        assert_eq!(offset, 3812 - 4200);
        assert!(patch_messages(&mut bmg).is_none(), "only once");
    }
}
