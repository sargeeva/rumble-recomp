//! Online players' names over their health bars, drawn by the game itself
//! (see `docs/online.md`).
//!
//! A player's health bar is the layout `hp_player.brlyt`: a `root` pane
//! holding the `BAR` group (gauge and a badge per player, `P0_P` to `P3_P`,
//! of which the game shows the one for whom the bar is) and a text box,
//! `LEVEL`, which the game keeps hidden. As the layout is unpacked, `LEVEL`
//! is given room for a name. While the game draws, a bar's visible badge
//! says whose bar it is. When its `LEVEL` draws and the host has a name for
//! that player, it draws in passes instead: a plate (the gauge's frame,
//! mirrored up over it), then the name in the deep colour of their badge.
//! It is then part of the picture, so it moves with the bar exactly, here
//! and for joiners.

use crate::cpu::{Cpu, SPR_LR};
use crate::mem::{Fault, Memory};
use rumble_formats::u8arc;

/// `nw4r::lyt::Pane`: its link in its parent's list of children (next,
/// prev), its parent, its children (count, then the list's own link),
/// matrix relative to the layout (3x4, rows), alphas (its own and with its
/// parents'), flags (bit 0: visible) and name.
const PANE_LINK: u32 = 0x04;
const PANE_PARENT: u32 = 0x0c;
const PANE_CHILDREN: u32 = 0x10;
const PANE_GLOBAL_MTX: u32 = 0x84;
const PANE_ALPHA: u32 = 0xb8;
const PANE_GLOBAL_ALPHA: u32 = 0xb9;
const PANE_FLAGS: u32 = 0xbb;
const PANE_NAME: u32 = 0xbc;
/// `nw4r::lyt::TextBox`, which extends the pane: its text buffer, the
/// text's colours (top, bottom), its font's size (x, y), the buffer's size
/// in bytes, the text's length in characters and its position in the box.
const TEXT_BUFFER: u32 = 0xd8;
const TEXT_COLORS: u32 = 0xdc;
const TEXT_FONT_SIZE: u32 = 0xe8;
const TEXT_BUFFER_BYTES: u32 = 0xfc;
const TEXT_LENGTH: u32 = 0xfe;
const TEXT_POSITION_AT: u32 = 0x100;

/// Room for a name (`MAX_NAME` characters in the host) and its NUL, in the
/// patched layout. A text box with this buffer size is one patched here.
const NAME_CHARS: u16 = 16;
const BUFFER_BYTES: u16 = (NAME_CHARS + 1) * 2;

/// Where the name goes, in the bar's `root` pane (y up): the gauge's top is
/// about 20 below the origin and the badge's top about 7, its right edge 32
/// left of it. The box is centred on `TRANSLATE` with the text at its left
/// (text position 3: middle left, of a 3x3 grid row by row).
const TRANSLATE: [f32; 2] = [47.0, -14.4];
const SIZE: [f32; 2] = [144.0, 14.0];
const FONT: [f32; 2] = [8.4, 10.5];
const CHAR_SPACE: f32 = 0.0;
const TEXT_POSITION: u8 = 3;
/// The gauge's top edge, in its own units (its origin is its left middle).
const GAUGE_TOP: f32 = 7.0;
/// The plate's height, as a share of the gauge's.
const PLATE_HEIGHT: f32 = 0.8;
/// The plate is as wide as the name, as a tab on the bar, up to the
/// gauge's width, a name too long for that being narrowed to fit. In the
/// gauge's units: where the text starts, the font's width at `FONT` (each
/// character about a share of it, `char_width`), room after the text, and
/// the gauge's width.
const PLATE_TEXT_LEFT: f32 = 12.0;
const PLATE_EM: f32 = 9.6;
const PLATE_PAD: f32 = 5.0;
const GAUGE_WIDTH: f32 = 78.0;

/// About how wide `c` is in the font, as a share of its size.
fn char_width(c: char) -> f32 {
    match c {
        'M' | 'W' | 'm' | 'w' | '@' | '%' => 0.85,
        'i' | 'l' | 'j' | 'I' | '.' | ',' | '!' | '\'' | '|' | ':' | ';' | ' ' => 0.3,
        'f' | 't' | 'r' => 0.42,
        'a'..='z' => 0.56,
        '0'..='9' => 0.6,
        c if c.is_ascii() => 0.7,
        _ => 1.0,
    }
}
/// The text's colour (RGBA) for each player: the deep red, blue, yellow
/// and green of their badges.
const TEXT_COLORS_BY_PLAYER: [u32; 4] = [0xa01818ff, 0x1a2a9aff, 0x8a6a00ff, 0x0a7a18ff];

/// Called with each archive the game unpacks: patches `hp_player.brlyt`'s
/// `LEVEL` text box.
pub fn unpacked(out: &mut [u8]) {
    let Ok(files) = u8arc::files(out) else { return };
    let base = out.as_ptr() as usize;
    let Some((at, len)) = files
        .iter()
        .find(|f| f.path.ends_with("blyt/hp_player.brlyt"))
        .map(|f| (f.data.as_ptr() as usize - base, f.data.len()))
    else {
        return;
    };
    drop(files);
    if !patch_layout(&mut out[at..at + len]) {
        eprintln!("name tags: hp_player.brlyt isn't as expected");
    }
}

/// Moves and widens the `LEVEL` text box. Returns whether it was found.
fn patch_layout(lyt: &mut [u8]) -> bool {
    let be16 = |b: &[u8], o: usize| b.get(o..o + 2).map(|s| u16::from_be_bytes([s[0], s[1]]) as usize);
    let be32 = |b: &[u8], o: usize| b.get(o..o + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap()) as usize);
    if lyt.get(..4) != Some(b"RLYT") {
        return false;
    }
    let (Some(mut o), Some(count)) = (be16(lyt, 0xc), be16(lyt, 0xe)) else { return false };
    for _ in 0..count {
        let (Some(magic), Some(len)) = (lyt.get(o..o + 4).map(|m| <[u8; 4]>::try_from(m).unwrap()), be32(lyt, o + 4)) else { return false };
        if &magic == b"txt1" && lyt.get(o + 0xc..o + 0x12) == Some(b"LEVEL\0") {
            let put = |lyt: &mut [u8], at: usize, v: f32| lyt[at..at + 4].copy_from_slice(&v.to_be_bytes());
            put(lyt, o + 0x24, TRANSLATE[0]);
            put(lyt, o + 0x28, TRANSLATE[1]);
            put(lyt, o + 0x44, SIZE[0]);
            put(lyt, o + 0x48, SIZE[1]);
            let text = o + 0x4c;
            lyt[text..text + 2].copy_from_slice(&BUFFER_BYTES.to_be_bytes());
            lyt[text + 8] = TEXT_POSITION;
            put(lyt, text + 0x18, FONT[0]);
            put(lyt, text + 0x1c, FONT[1]);
            put(lyt, text + 0x20, CHAR_SPACE);
            return true;
        }
        if len < 8 {
            return false;
        }
        o += len;
    }
    false
}

/// A bar's `LEVEL` box being drawn, in passes (`Pass`): its own draw draws
/// the first pass's pane instead, returning through `GuestReturn`
/// (`pass_drawn`), which calls `Pane::Draw` again for the next.
pub struct BarDraw {
    level: u32,
    player: u8,
    gauge: Option<u32>,
    /// `Pane::Draw`'s draw info and its entry.
    info: u32,
    entry: u32,
    passes: Vec<Pass>,
    /// Whether the next `Pane::Draw` is the next pass.
    next: bool,
    /// The gauge's matrix and its children's flags, to put back after the
    /// plate.
    saved_mtx: Vec<u8>,
    saved_flags: Vec<(u32, u8)>,
    /// The name; the plate's width, as a share of the gauge's, and how much
    /// the name is narrowed to fit on it.
    name: Vec<u16>,
    plate_width: f32,
    squeeze: f32,
}

#[derive(Clone, Copy, PartialEq)]
enum Pass {
    /// The gauge's frame, mirrored up over it: the plate the name is on.
    Plate,
    Name,
}

/// At each `Pane::Draw`: notes each bar's player. At a `LEVEL` box with a
/// name to show, starts drawing it and returns the pane to draw first
/// (instead of the box), which the caller draws, returning through
/// `GuestReturn` to `pass_drawn`.
pub fn pane_draw(mem: &mut Memory, pane: u32, info: u32, entry: u32) -> Option<u32> {
    if let Some(mut draw) = mem.multiplay.bar_draw.take() {
        let next = std::mem::take(&mut draw.next);
        let first = next.then(|| start(mem, &mut draw));
        mem.multiplay.bar_draw = Some(draw);
        if next {
            return first;
        }
    }
    let ram = mem.ram();
    let name = ram.bytes(pane + PANE_NAME, 6)?;
    let parent = ram.u32(pane + PANE_PARENT);
    match name {
        [b'P', n @ b'0'..=b'3', b'_', b'P', 0, _] => {
            if ram.bytes(pane + PANE_FLAGS, 1).is_some_and(|f| f[0] & 1 != 0) {
                // badge -> bar -> BAR -> root
                let mut root = pane;
                for _ in 0..3 {
                    root = ram.u32(root + PANE_PARENT);
                }
                let player = n - b'0';
                let roots = &mut mem.multiplay.bar_players;
                match roots.iter_mut().find(|(r, _)| *r == root) {
                    Some(entry) => entry.1 = player,
                    None => roots.push((root, player)),
                }
            }
            None
        }
        [b'L', b'E', b'V', b'E', b'L', 0] => {
            if ram.u16(pane + TEXT_BUFFER_BYTES) != BUFFER_BYTES || mem.multiplay.bar_draw.is_some() {
                return None;
            }
            let root = parent;
            let player = mem.multiplay.bar_players.iter().find(|(r, _)| *r == root).map(|&(_, p)| p);
            let name: Vec<u16> = player
                .and_then(|p| mem.input.remotes.name(p as usize))
                .map(|n| n.encode_utf16().take(NAME_CHARS as usize).collect())
                .unwrap_or_default();
            if name.is_empty() {
                // Shown before: hidden again, as the game has it.
                let flags = ram.bytes(pane + PANE_FLAGS, 1).map_or(0, |f| f[0]);
                let _ = mem.load(pane + PANE_FLAGS, &[flags & !1]);
                return None;
            }
            let width: f32 = char::decode_utf16(name.iter().copied()).map(|c| char_width(c.unwrap_or(' ')) * PLATE_EM).sum();
            let room = GAUGE_WIDTH - PLATE_TEXT_LEFT - PLATE_PAD;
            let squeeze = (room / width.max(1.0)).min(1.0);
            // Found afresh each time: the game rebuilds bars.
            let gauge = child(&ram, root, b"BAR").and_then(|p| child(&ram, p, b"bar")).and_then(|p| child(&ram, p, b"gauge"));
            let mut passes = Vec::new();
            if gauge.is_some() {
                passes.push(Pass::Plate);
            }
            passes.push(Pass::Name);
            let mut draw = BarDraw {
                level: pane,
                player: player.unwrap_or(0),
                gauge,
                info,
                entry,
                passes,
                next: false,
                saved_mtx: Vec::new(),
                saved_flags: Vec::new(),
                name,
                plate_width: ((PLATE_TEXT_LEFT + width * squeeze + PLATE_PAD) / GAUGE_WIDTH).clamp(0.3, 1.0),
                squeeze,
            };
            let first = start(mem, &mut draw);
            mem.multiplay.bar_draw = Some(draw);
            Some(first)
        }
        _ => None,
    }
}

/// Sets up `draw`'s pass; returns the pane it draws.
fn start(mem: &mut Memory, draw: &mut BarDraw) -> u32 {
    match draw.passes[0] {
        Pass::Plate => {
            let gauge = draw.gauge.unwrap_or(0);
            let ram = mem.ram();
            let f = |a: u32| f32::from_bits(ram.u32(a));
            let mtx: Vec<f32> = (0..12).map(|i| f(gauge + PANE_GLOBAL_MTX + i * 4)).collect();
            draw.saved_mtx = ram.bytes(gauge + PANE_GLOBAL_MTX, 48).map(<[u8]>::to_vec).unwrap_or_default();
            draw.saved_flags =
                children(&ram, gauge).into_iter().map(|p| (p, ram.bytes(p + PANE_FLAGS, 1).map_or(0, |f| f[0]))).collect();
            // Mirrored about its top edge and squashed: y -> top + h * (top - y);
            // narrowed from its left edge, its origin.
            let mut plate = mtx.clone();
            for row in 0..3 {
                plate[row * 4] = mtx[row * 4] * draw.plate_width;
                plate[row * 4 + 1] = -mtx[row * 4 + 1] * PLATE_HEIGHT;
                plate[row * 4 + 3] = mtx[row * 4 + 3] + mtx[row * 4 + 1] * GAUGE_TOP * (1.0 + PLATE_HEIGHT);
            }
            let bytes: Vec<u8> = plate.iter().flat_map(|v| v.to_bits().to_be_bytes()).collect();
            let _ = mem.load(gauge + PANE_GLOBAL_MTX, &bytes);
            for &(p, flags) in &draw.saved_flags {
                let _ = mem.load(p + PANE_FLAGS, &[flags & !1]);
            }
            gauge
        }
        Pass::Name => {
            let text = Text {
                text: &draw.name,
                at: TRANSLATE,
                font: [FONT[0] * draw.squeeze, FONT[1]],
                position: TEXT_POSITION,
                color: TEXT_COLORS_BY_PLAYER[draw.player as usize % 4],
            };
            place(mem, draw.level, &text);
            draw.level
        }
    }
}

/// `pane`'s children, from its list of them (empty if it doesn't look like
/// one).
fn children(ram: &crate::mem::Ram, pane: u32) -> Vec<u32> {
    let count = ram.u32(pane + PANE_CHILDREN);
    let head = pane + PANE_CHILDREN + 4;
    let mut out = Vec::new();
    let mut node = ram.u32(head);
    while node != head && out.len() < 32 {
        if node < 0x8000_0000 {
            return Vec::new();
        }
        out.push(node - PANE_LINK);
        node = ram.u32(node);
    }
    if out.len() as u32 != count {
        return Vec::new();
    }
    out
}

/// `pane`'s child named `name`.
fn child(ram: &crate::mem::Ram, pane: u32, name: &[u8]) -> Option<u32> {
    children(ram, pane).into_iter().find(|&c| {
        ram.bytes(c + PANE_NAME, name.len() + 1).is_some_and(|n| &n[..name.len()] == name && n[name.len()] == 0)
    })
}

/// What a pass shows in the `LEVEL` box: the text, where the box's centre
/// is in the bar's `root` pane, the font's size, where in the box the text
/// is (3x3 grid, row by row) and its colour (RGBA).
struct Text<'a> {
    text: &'a [u16],
    at: [f32; 2],
    font: [f32; 2],
    position: u8,
    color: u32,
}

/// Shows the `LEVEL` box as `text`. The game keeps the box hidden, so nw4r
/// never works out where it is: its matrix is the parent's moved to `at`.
fn place(mem: &mut Memory, pane: u32, text: &Text) {
    let ram = mem.ram();
    let root = ram.u32(pane + PANE_PARENT);
    let buffer = ram.u32(pane + TEXT_BUFFER);
    let f = |a: u32| f32::from_bits(ram.u32(a));
    let parent: Vec<f32> = (0..12).map(|i| f(root + PANE_GLOBAL_MTX + i * 4)).collect();
    let [tx, ty] = text.at;
    let mut mtx = parent.clone();
    for row in 0..3 {
        let r = &parent[row * 4..row * 4 + 4];
        mtx[row * 4 + 3] = r[0] * tx + r[1] * ty + r[3];
    }
    let bytes: Vec<u8> = mtx.iter().flat_map(|v| v.to_bits().to_be_bytes()).collect();
    let parent_alpha = ram.bytes(root + PANE_GLOBAL_ALPHA, 1).map_or(0xff, |a| a[0]);
    let flags = ram.bytes(pane + PANE_FLAGS, 1).map_or(0, |f| f[0]);
    let mut chars: Vec<u8> = text.text.iter().flat_map(|c| c.to_be_bytes()).collect();
    chars.extend_from_slice(&[0, 0]);
    let font: Vec<u8> = text.font.iter().flat_map(|v| v.to_be_bytes()).collect();
    let _ = mem.load(buffer, &chars);
    let _ = mem.load(pane + TEXT_LENGTH, &(text.text.len() as u16).to_be_bytes());
    let _ = mem.load(pane + TEXT_POSITION_AT, &[text.position]);
    let _ = mem.load(pane + PANE_GLOBAL_MTX, &bytes);
    let _ = mem.load(pane + PANE_ALPHA, &[0xff, parent_alpha]);
    let _ = mem.load(pane + PANE_FLAGS, &[flags | 1]);
    let _ = mem.load(pane + TEXT_COLORS, &[text.color.to_be_bytes(), text.color.to_be_bytes()].concat());
    let _ = mem.load(pane + TEXT_FONT_SIZE, &font);
}

/// Back from a pass: puts back what it changed and calls `Pane::Draw`
/// again for the next (through its entry, as the game would), or, after
/// the last, returns to where the box's own draw would have.
pub fn pass_drawn(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let sp = cpu.gpr[1];
    let caller = mem.r32(sp + 8)?;
    cpu.gpr[1] = mem.r32(sp)?;
    let Some(mut draw) = mem.multiplay.bar_draw.take() else {
        cpu.pc = caller & !3;
        return Ok(());
    };
    if draw.passes[0] == Pass::Plate {
        let gauge = draw.gauge.unwrap_or(0);
        let _ = mem.load(gauge + PANE_GLOBAL_MTX, &draw.saved_mtx);
        for &(p, flags) in &draw.saved_flags {
            let _ = mem.load(p + PANE_FLAGS, &[flags]);
        }
    }
    draw.passes.remove(0);
    if draw.passes.is_empty() {
        cpu.pc = caller & !3;
        return Ok(());
    }
    draw.next = true;
    cpu.gpr[3] = draw.level;
    cpu.gpr[4] = draw.info;
    cpu.spr[SPR_LR] = caller;
    cpu.pc = draw.entry;
    mem.multiplay.bar_draw = Some(draw);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The game's layout gets its `LEVEL` box moved and widened.
    #[test]
    fn patches_the_bar_layout() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/assets/game2d.arc/arc/blyt/hp_player.brlyt");
        let Ok(mut lyt) = std::fs::read(dir) else { return };
        let before = lyt.clone();
        assert!(patch_layout(&mut lyt));
        assert_eq!(lyt.len(), before.len());
        assert!(lyt.windows(2).any(|w| w == BUFFER_BYTES.to_be_bytes()));
    }
}
