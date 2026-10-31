//! A "Recomped" line under the title screen's copyright (see
//! `docs/multiplayer-ditto.md`).
//!
//! The copyright is the `copy` text box of `title2d.arc/arc/blyt/
//! title_main.brlyt`. Its text can't grow in the archive, so it grows in
//! two steps: as the archive is unpacked, the box's buffer size (a field
//! of its `txt1` block) is raised, so the game allocates room for the
//! line; when the box first draws (`Pane::Draw`, `lyt.rs`), the line is
//! added to the text in that buffer.

use crate::mem::Memory;
use rumble_formats::u8arc;

/// What is added after the copyright.
const NOTE: &str = "\nRecomped 10/30/2026";

/// The copyright's last words, to know it is the expected text box.
const LAST: &str = "Developed by Ambrella.";

/// Called with each archive the game unpacks.
pub fn unpacked(mem: &mut Memory, out: &mut [u8]) {
    let Ok(files) = u8arc::files(out) else { return };
    let base = out.as_ptr() as usize;
    let Some((at, len)) = files
        .iter()
        .find(|f| f.path.ends_with("blyt/title_main.brlyt"))
        .map(|f| (f.data.as_ptr() as usize - base, f.data.len()))
    else {
        return;
    };
    drop(files);
    if let Some(chars) = raise_buffer(&mut out[at..at + len]) {
        mem.multiplay.title_note = Some(chars);
    }
}

/// Raises the `copy` text box's buffer for `NOTE`; returns its text's
/// length in characters.
fn raise_buffer(lyt: &mut [u8]) -> Option<u32> {
    let be16 = |b: &[u8], o: usize| Some(u16::from_be_bytes(b.get(o..o + 2)?.try_into().ok()?) as usize);
    let be32 = |b: &[u8], o: usize| Some(u32::from_be_bytes(b.get(o..o + 4)?.try_into().ok()?) as usize);
    if lyt.get(0..4)? != b"RLYT" {
        return None;
    }
    let mut o = be16(lyt, 0xc)?;
    for _ in 0..be16(lyt, 0xe)? {
        let len = be32(lyt, o + 4)?;
        if lyt.get(o..o + 4)? == b"txt1" && lyt.get(o + 0xc..o + 0x11)? == b"copy\0" {
            let (buf, bytes, text) = (be16(lyt, o + 0x4c)?, be16(lyt, o + 0x4e)?, o + be32(lyt, o + 0x58)?);
            let s: Vec<u16> = lyt.get(text..text + bytes)?.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
            let s = String::from_utf16(s.strip_suffix(&[0])?).ok()?;
            if !s.ends_with(LAST) {
                return None;
            }
            let needed = (s.encode_utf16().count() + NOTE.encode_utf16().count() + 1) * 2;
            let raised = buf.max(needed);
            lyt[o + 0x4c..o + 0x4e].copy_from_slice(&(raised as u16).to_be_bytes());
            return Some(s.encode_utf16().count() as u32);
        }
        if len < 8 {
            break;
        }
        o += len;
    }
    None
}

/// At each `Pane::Draw`: if `pane` is the copyright text box, still with its
/// original text, add `NOTE`. A `TextBox` follows the `Pane` it extends
/// with its text buffer's address, then its buffer size and text length
/// (halfwords); they are found by their values.
pub fn pane_draw(mem: &mut Memory, pane: u32) {
    let Some(chars) = mem.multiplay.title_note else { return };
    let Ok(obj) = mem.read_bytes(pane, 0x140) else { return };
    if !obj[..0xd8].windows(5).any(|w| w == b"copy\0") {
        return;
    }
    let word = |o: usize| u32::from_be_bytes(obj[o..o + 4].try_into().unwrap());
    let text: Vec<u8> = {
        let mut ram = Vec::new();
        for o in (0xd0..0x130).step_by(4) {
            let p = word(o);
            if let Ok(t) = mem.read_bytes(p, chars * 2) {
                let units: Vec<u16> = t.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                if String::from_utf16(&units).is_ok_and(|s| s.ends_with(LAST)) {
                    ram = p.to_be_bytes().to_vec();
                    break;
                }
            }
        }
        ram
    };
    let [a, b, c, d] = text[..] else { return };
    let buffer = u32::from_be_bytes([a, b, c, d]);
    let total = chars as usize + NOTE.encode_utf16().count();
    // The buffer size (bytes) and text length (characters).
    let Some(len_at) = (0xd8..0x13c)
        .step_by(2)
        .find(|&o| u16::from_be_bytes([obj[o + 2], obj[o + 3]]) as u32 == chars && u16::from_be_bytes([obj[o], obj[o + 1]]) as usize >= (total + 1) * 2)
        .map(|o| o + 2)
    else {
        return;
    };
    let added: Vec<u8> = NOTE.encode_utf16().chain([0]).flat_map(u16::to_be_bytes).collect();
    if mem.load(buffer + chars * 2, &added).is_ok() {
        let _ = mem.load(pane + len_at as u32, &(total as u16).to_be_bytes());
    }
    mem.multiplay.title_note = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The title layout's copyright box gets room for the note, in the
    /// build in `RUMBLE_TEST_BUILD` or build/ (skipped without one).
    #[test]
    fn copyright_buffer_raised() {
        let dir = std::env::var_os("RUMBLE_TEST_BUILD")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build"));
        let Ok(mut lyt) = std::fs::read(dir.join("assets/title2d.arc/arc/blyt/title_main.brlyt")) else { return };
        let before = lyt.clone();
        assert_eq!(raise_buffer(&mut lyt), Some(88));
        let changed: Vec<usize> = (0..lyt.len()).filter(|&i| lyt[i] != before[i]).collect();
        assert!(changed.len() <= 2, "only the buffer size changes");
    }
}
