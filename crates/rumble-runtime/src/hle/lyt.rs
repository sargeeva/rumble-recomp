//! Widescreen HUD placement. The HUD is drawn with NW4R layouts; with
//! widescreen, the 4:3 picture is centered in the wider one, and the
//! layouts of the in-game HUD (the portrait, combo counter, minimap and
//! money/lives) are moved out to the side of the screen they are on.
//!
//! `nw4r::lyt::Layout::Build` is watched to recognise those layouts from
//! their `.brlyt` data, and `nw4r::lyt::Pane::Draw` to know when one is
//! drawing: from its root pane's draw until that returns, the GX draws
//! belong to it (`Gx::set_hud_anchor`, `Draw` x placement in `widen`).
//! Neither changes what the game computes; both carry on into the
//! function itself past its first two instructions, done here, and count
//! as the instructions the game ran, so recorded sessions replay the same.
//! (A HUD root pane's draw returns through `GuestReturn`: that one extra
//! instruction stands for the second at its entry.)

use super::{push_guest_frame, Replacement, TAG_HUD, TAG_NAME};
use crate::cpu::{Cpu, SPR_LR};
use crate::mem::{Fault, Memory};

pub static REPLACEMENTS: &[Replacement] = &[
    Replacement {
        name: "nw4r::lyt::Layout::Build",
        sig: &[
            0x9421ffa0, 0x7c0802a6, 0x90010064, 0x39610000, 0x48000001, 0x7c9f2378,
            0x3c800000, 0x7c7e1b78, 0x7cae2b78, 0x38840000, 0x7fe3fb78, 0x48000001,
            0x2c030000, 0x4082000c, 0x38600000, 0x48000000, 0xa07f0000, 0x38800000,
            0x5460c63f, 0x5463063e, 0x40820024, 0x38030000, 0x38600000, 0x28000002,
        ],
        run: layout_build,
    },
    Replacement {
        name: "nw4r::lyt::Pane::Draw",
        sig: &[
            0x9421ffe0, 0x7c0802a6, 0xc0220000, 0x90010024, 0x93e1001c, 0x93c10018,
            0x7c7e1b78, 0x93a10014, 0x7c9d2378, 0xc0030000, 0x88030000, 0xfc010000,
            0x540407fe, 0x41820010, 0xc0030000, 0xfc010000, 0x40820008, 0x38800000,
            0x88630000, 0x2c030000, 0x40820010, 0x540007bd, 0x41820008, 0x38800000,
        ],
        run: pane_draw,
    },
];

/// Where a `Layout` keeps its root pane.
const ROOT_PANE: u32 = 0x10;

/// Runs a function's first two instructions, `stwu r1, -frame(r1)` and
/// `mflr r0` (its entry holds the trap and the word after it
/// `GuestReturn`), and carries on after them; `counted` of them count as
/// run besides the trap.
pub(super) fn enter(cpu: &mut Cpu, mem: &mut Memory, frame: u32, counted: u32) -> Result<(), Fault> {
    cpu.hle_steps = counted;
    let entry = cpu.pc - 4;
    let sp = cpu.gpr[1];
    mem.w32(sp - frame, sp)?;
    cpu.gpr[1] = sp - frame;
    cpu.gpr[0] = cpu.spr[SPR_LR];
    cpu.pc = entry + 8;
    Ok(())
}

/// `Layout::Build(this, lytRes, resAcsr)`: notes whether the layout is
/// part of the HUD, and on which side.
fn layout_build(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let (layout, res) = (cpu.gpr[3], cpu.gpr[4]);
    let side = hud_side(mem, res);
    let select = is_selection(mem, res);
    let list = &mut mem.multiplay.selection_layouts;
    list.retain(|&l| l != layout);
    if select {
        list.push(layout);
    }
    let hud = &mut mem.gx.hud_layouts;
    hud.retain(|&(l, _)| l != layout);
    if let Some(side) = side {
        hud.push((layout, side));
    }
    enter(cpu, mem, 0x60, 1)
}

/// `Pane::Draw(this, drawInfo)`: a HUD layout's root pane draws the whole
/// layout; until it returns, draws belong to the HUD.
fn pane_draw(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let pane = cpu.gpr[3];
    super::title::pane_draw(mem, pane);
    if let Some(first) = super::nametag::pane_draw(mem, pane, cpu.gpr[4], cpu.pc - 4) {
        // A name tag, drawn in passes instead of the box (`nametag`).
        cpu.gpr[3] = first;
        push_guest_frame(cpu, mem, 0, TAG_NAME)?;
        return enter(cpu, mem, 0x20, 0);
    }
    let ram = mem.ram();
    if mem.multiplay.selection_layouts.iter().any(|&l| ram.u32(l + ROOT_PANE) == pane) {
        mem.input.remotes.mark_selecting();
    }
    let side = mem.gx.hud_layouts.iter().find(|&&(l, _)| ram.u32(l + ROOT_PANE) == pane).map(|&(_, s)| s);
    if let Some(side) = side {
        mem.gx.set_hud_anchor(side);
        // The pane draws as usual but returns through `GuestReturn`, which
        // ends the HUD's draws.
        push_guest_frame(cpu, mem, 0, TAG_HUD)?;
        return enter(cpu, mem, 0x20, 0);
    }
    enter(cpu, mem, 0x20, 1)
}

/// The side of the screen a `.brlyt` at `res` belongs to, if it is a HUD
/// layout, from its panes' and textures' names.
fn hud_side(mem: &Memory, res: u32) -> Option<i8> {
    let (panes, textures) = names(mem, res)?;
    let pane = |n: &str| panes.iter().any(|p| p == n);
    let texture = |prefix: &str| textures.iter().any(|t| t.starts_with(prefix));
    if pane("pii_profile") {
        // enemy_info: the portrait, top left.
        Some(-1)
    } else if pane("COMBO_c") || pane("win_r_02") || texture("minimap_circle") || texture("life_ball") {
        // combo; money_root, life_root, moneylife_root; minimap; life (each
        // life ball). Not the minimap's map and marks (minimap_map,
        // minimap_enemy, ...): those are drawn into the texture the minimap
        // shows, not on the screen.
        Some(1)
    } else {
        None
    }
}

/// Whether a `.brlyt` at `res` is a player's panel on the Pokemon
/// selection screen (`dlgpiisel_p_root`), where players join multiplayer.
fn is_selection(mem: &Memory, res: u32) -> bool {
    names(mem, res).is_some_and(|(panes, _)| ["HELP0", "PIISPEC", "PLAYERID"].iter().all(|n| panes.iter().any(|p| p == n)))
}

/// The pane and texture names in a `.brlyt` in RAM.
fn names(mem: &Memory, res: u32) -> Option<(Vec<String>, Vec<String>)> {
    let ram = mem.ram();
    if ram.u32(res) != u32::from_be_bytes(*b"RLYT") {
        return None;
    }
    let bytes = ram.bytes(res, ram.u32(res + 8).min(0x10_0000) as usize)?;
    let be16 = |o: usize| Some(u16::from_be_bytes(bytes.get(o..o + 2)?.try_into().ok()?) as usize);
    let be32 = |o: usize| Some(u32::from_be_bytes(bytes.get(o..o + 4)?.try_into().ok()?) as usize);
    let cstr = |o: usize, max: usize| {
        let s = bytes.get(o..(o + max).min(bytes.len()))?;
        let end = s.iter().position(|&b| b == 0).unwrap_or(s.len());
        Some(String::from_utf8_lossy(&s[..end]).into_owned())
    };
    let (mut panes, mut textures) = (Vec::new(), Vec::new());
    let mut o = be16(0xc)?;
    for _ in 0..be16(0xe)? {
        let (magic, len) = (bytes.get(o..o + 4)?, be32(o + 4)?);
        match magic {
            b"pan1" | b"pic1" | b"txt1" | b"wnd1" | b"bnd1" => panes.push(cstr(o + 0xc, 16)?),
            b"txl1" => {
                let list = o + 0xc;
                for k in 0..be16(o + 8)? {
                    textures.push(cstr(list + be32(list + 8 * k)?, 64)?);
                }
            }
            _ => {}
        }
        if len < 8 {
            break;
        }
        o += len;
    }
    Some((panes, textures))
}
