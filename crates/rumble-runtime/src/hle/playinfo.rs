//! Rumble Weekend: a "Befriended Shinies" line on the Information
//! podium's Play Info page (see `docs/multiplayer-ditto.md`).
//!
//! The podium is `script.arc/CItem_information.pkc`. Its Play Info page is
//! three `DlgInfoSetLine(page, row, label, value, suffix)` calls: Time
//! Played, Defeated Pokemon (stat -200) and Befriended Pokemon (stat -199).
//! The collection keeps, for each species, a bit per form for "befriended
//! as a shiny" (`fn_800e1b50` sets it; the collection screen shows it),
//! but no native gives it to scripts. So the new line asks `GetStatValue`
//! for `SHINY_STAT`, an id the game doesn't use, and the replacement of
//! `GetStatValue` (installed at the address the native table gives)
//! answers it with the number of those bits; every other id runs the
//! game's own function.
//!
//! The Befriended Pokemon line is replaced by a call to a new function
//! that draws it and the new line; the function goes where one nothing
//! calls was, and the rest of that function's space is removed (with every
//! code address after it moved down) to make room for the new label.

use super::multiplay::{native_entries, natives, op, Asm, IMAGE_START};
use super::Replacement;
use crate::cpu::{Cpu, SPR_LR};
use crate::mem::{Fault, Memory};
use rumble_formats::u8arc;
use std::collections::HashMap;

/// The new line's label.
const LABEL: &str = "Befriended Shinies";

/// The `GetStatValue` id that answers the shiny count.
const SHINY_STAT: i32 = -1000;

pub static REPLACEMENTS: &[Replacement] = &[Replacement { name: "GetStatValue", sig: &[], run: get_stat_value }];

/// Puts the `GetStatValue` replacement in place, at boot.
pub fn install(mem: &mut Memory) -> Result<(), String> {
    let image = mem.read_bytes(IMAGE_START, 0x0060_0000).map_err(|f| format!("{f:?}"))?;
    let (_, f) = native_entries(&image)?.into_iter().find(|(n, _)| n == "GetStatValue").ok_or("no GetStatValue")?;
    // The replacement runs the function's first two instructions itself.
    if mem.r32(f).ok() != Some(0x9421_ffe0) || mem.r32(f + 4).ok() != Some(0x7c08_02a6) {
        return Err("GetStatValue not as expected".into());
    }
    super::place(mem, "GetStatValue", f).map_err(|f| format!("{f:?}"))?;
    mem.multiplay.shiny_count = true;
    Ok(())
}

/// `GetStatValue(id)` for scripts: the shiny count for `SHINY_STAT`, else
/// the game's own function.
fn get_stat_value(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let vm = cpu.gpr[3];
    let top = mem.r32(vm + 0x28)? + (mem.r32(vm + 0x24)?.wrapping_sub(1)) * 8;
    let (kind, value) = (mem.r32(top)?, mem.r32(top + 4)?);
    if !matches!(kind, 0 | 5) || value as i32 != SHINY_STAT {
        return super::lyt::enter(cpu, mem, 0x20, 1);
    }
    let count = shiny_count(cpu, mem).unwrap_or(0);
    // The argument's slot becomes the result.
    mem.w32(top, 0)?;
    mem.w32(top + 4, count)?;
    cpu.gpr[3] = 0;
    cpu.pc = cpu.spr[SPR_LR];
    Ok(())
}

/// How many species and forms the collection has as befriended shinies.
/// The function `GetStatValue` replaces reaches the game's save data
/// through `bl` to a getter (`lwz r3, d(r13)`); that object's collection is
/// what its virtual function 0x28 (`lwz r3, x(r3)`) returns; the collection
/// is an array of 0x1c-byte species entries with the shiny bits at 0x18,
/// up to Rumble Weekend's last Pokedex number.
fn shiny_count(cpu: &Cpu, mem: &mut Memory) -> Option<u32> {
    let f = cpu.pc - 4;
    let call = (0..20).map(|i| f + i * 4).filter(|&a| mem.r32(a).is_ok_and(|w| w >> 26 == 18 && w & 1 == 1)).nth(1)?;
    let bl = mem.r32(call).ok()?;
    let getter = call.wrapping_add((((bl & 0x03ff_fffc) << 6) as i32 >> 6) as u32);
    let lwz = mem.r32(getter).ok()?;
    if lwz >> 16 != 0x806d || mem.r32(getter + 4).ok()? != 0x4e80_0020 {
        return None;
    }
    let save = mem.r32(cpu.gpr[13].wrapping_add(lwz as u16 as i16 as u32)).ok()?;
    let vtable = mem.r32(save).ok()?;
    let vfunc = mem.r32(vtable + 0x28).ok()?;
    let lwz = mem.r32(vfunc).ok()?;
    if lwz >> 16 != 0x8063 || mem.r32(vfunc + 4).ok()? != 0x4e80_0020 {
        return None;
    }
    let collection = mem.r32(save + (lwz & 0xffff)).ok()?;
    let entries = mem.r32(collection).ok()?;
    (0..=493).map(|i| mem.r32(entries + i * 0x1c + 0x18).map(u32::count_ones)).sum::<Result<u32, _>>().ok()
}

/// Rumble Weekend changes one thing in the DOL: a loop over the Pokedex
/// numbers stops at 493 instead of 150 (`rlwinm r0, r0, 0, 16, 31;
/// cmplwi r0, 493; ble`).
pub fn is_weekend(mem: &mut Memory) -> bool {
    let Ok(image) = mem.read_bytes(IMAGE_START, 0x0060_0000) else { return false };
    image.chunks_exact(4).collect::<Vec<_>>().windows(3).any(|w| {
        let word = |c: &[u8]| u32::from_be_bytes(c.try_into().unwrap());
        word(w[0]) == 0x5400_043e && word(w[1]) == 0x2800_01ed && word(w[2]) >> 16 == 0x4081
    })
}

/// Called with each archive the game unpacks while Rumble Weekend runs.
pub fn unpacked(mem: &mut Memory, out: &mut [u8]) {
    if !mem.multiplay.shiny_count {
        return;
    }
    let Ok(files) = u8arc::files(out) else { return };
    let base = out.as_ptr() as usize;
    let Some((at, len)) = files
        .iter()
        .find(|f| f.path.rsplit('/').next() == Some("CItem_information.pkc"))
        .map(|f| (f.data.as_ptr() as usize - base, f.data.len()))
    else {
        return;
    };
    drop(files);
    match natives(mem).and_then(|n| patch_script(&out[at..at + len], &n)) {
        Ok(new) => {
            out[at..at + new.len()].copy_from_slice(&new);
            out[at + new.len()..at + len].fill(0);
        }
        Err(e) => eprintln!("Play Info: script left as it was: {e}"),
    }
}

/// How many words follow an instruction as its data.
fn data_words(w: u32) -> usize {
    let count = (w >> 16 & 0xff) as usize;
    match w >> 24 {
        op::FLOAT => 1,
        // A table of offsets: the default in the first word's low half,
        // then one halfword per case.
        0x4c => count.div_ceil(2),
        // A table of (value, offset) pairs.
        0x4b => count,
        _ => 0,
    }
}

/// Instructions whose operand is a code address.
fn has_target(w: u32) -> bool {
    matches!(w >> 24, op::CALL | op::JUMP | op::JUMP_IF_NOT | 0x24 | 0x25 | 0x26 | op::FUNCTION_REF)
}

/// Removes `n` words at `at` from `code`, which nothing may jump into or
/// across by a switch table, and moves every code address after them down.
fn remove(code: &mut Vec<u32>, at: usize, n: usize) -> Result<(), String> {
    let end = at + n;
    let mut i = 0;
    while i < code.len() {
        let w = code[i];
        let extra = data_words(w);
        if has_target(w) {
            let to = (w & 0xff_ffff) as usize;
            if (at..end).contains(&to) {
                return Err("a jump into the removed code".into());
            }
            if to >= end && !(at..end).contains(&i) {
                code[i] = w & 0xff00_0000 | (to - n) as u32;
            }
        }
        if matches!(w >> 24, 0x4b | 0x4c) {
            // Offsets (signed halfwords) relative to the table.
            let halves = (i..=i + extra).flat_map(|j| [(code[j] >> 16) as u16, code[j] as u16]);
            for off in halves.skip(1) {
                let to = i as isize + off as i16 as isize;
                if (to < at as isize) != (i < at) || (to >= end as isize) != (i >= end) {
                    return Err("a switch across the removed code".into());
                }
            }
        }
        i += 1 + extra;
    }
    code.drain(at..end);
    Ok(())
}

/// The Information podium's script with the Befriended Shinies line.
pub fn patch_script(file: &[u8], natives: &HashMap<String, u32>) -> Result<Vec<u8>, String> {
    let rd = |at: usize| file.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or("script too short");
    if rd(0)? != 0x706b_1a20 {
        return Err("not a script".into());
    }
    let (code_size, wide_size, wide_count) = (rd(8)? as usize, rd(12)? as usize, rd(16)?);
    let mut code: Vec<u32> = (0..code_size / 4).map(|i| rd(0x20 + i * 4)).collect::<Result<_, _>>()?;
    let rest = file.get(0x20 + code_size..).ok_or("script too short")?;
    let native = |name: &str| natives.get(name).copied().ok_or(format!("no native {name}"));
    let (cat_gets, stat, set_line) = (native("CatGets")?, native("GetStatValue")?, native("DlgInfoSetLine")?);
    let is = |w: u32, o: u32, arg: u32| w == o << 24 | arg & 0xff_ffff;

    // The Befriended Pokemon line: page, row 2, label 0x70009, stat -199,
    // suffix 0x70002.
    let line = |page: u32| -> [u32; 12] {
        let w = |o: u32, a: u32| o << 24 | a & 0xff_ffff;
        [
            w(op::LOAD, op::LOCAL | page),
            w(op::INT, 2),
            w(op::INT, 0x7_0009),
            w(op::WSTRING, 0),
            w(op::NATIVE, cat_gets),
            w(op::INT, -199i32 as u32),
            w(op::NATIVE, stat),
            w(op::INT, 0x7_0002),
            w(op::WSTRING, 0),
            w(op::NATIVE, cat_gets),
            w(op::NATIVE, set_line),
            w(op::POP, 0),
        ]
    };
    let found: Vec<usize> = (0..code.len().saturating_sub(12))
        .filter(|&i| code[i] >> 16 == (op::LOAD << 8 | 1) && code[i..i + 12] == line(code[i] & 0xffff))
        .collect();
    let [row] = found[..] else { return Err(format!("{} Befriended Pokemon lines", found.len())) };
    let page = code[row] & 0xffff;

    let label = wide_count;
    let new_function = |base: u32| -> Result<Vec<u32>, String> {
        let local = |n: u32| op::LOCAL | n;
        let mut a = Asm::new(base, natives);
        a.op(op::FUNCTION, 0).op(op::FRAME, 1 << 16 | 1);
        a.words.extend_from_slice(&line(0));
        a.op(op::LOAD, local(0))
            .int(3)
            .op(op::WSTRING, label)
            .int(SHINY_STAT)
            .native("GetStatValue")
            .int(0x7_0002)
            .op(op::WSTRING, 0)
            .native("CatGets")
            .call_native("DlgInfoSetLine")
            .int(0)
            .op(op::RETURN, 0);
        a.finish()
    };
    let size = new_function(0)?.len();

    // A function nothing calls or refers to, after a jump over it.
    let referenced: Vec<usize> = code
        .iter()
        .filter(|&&w| matches!(w >> 24, op::CALL | op::FUNCTION_REF))
        .map(|&w| (w & 0xff_ffff) as usize)
        .filter(|&f| code.get(f).is_some_and(|&w| is(w, op::FUNCTION, 0)))
        .collect();
    let (dead, end) = (1..code.len())
        .filter(|&f| is(code[f], op::FUNCTION, 0) && code[f - 1] >> 24 == op::JUMP)
        .map(|f| (f, (code[f - 1] & 0xff_ffff) as usize))
        .find(|&(f, end)| {
            end > f && end - f >= size && end <= code.len() && !(f..end).contains(&row) && !referenced.iter().any(|r| (f..end).contains(r))
        })
        .ok_or("no room for the new function")?;
    let function = new_function(dead as u32)?;
    code[dead..dead + size].copy_from_slice(&function);
    // The line becomes a call to it.
    let call = [op::LOAD << 24 | op::LOCAL | page, op::CALL << 24 | dead as u32, op::POP << 24];
    code[row..row + 3].copy_from_slice(&call);
    code[row + 3..row + 12].fill(0);
    // Room for the label: the rest of the dead function's space, blanked
    // so none of its old instructions are read.
    code[dead + size..end].fill(0);
    remove(&mut code, dead + size, end - dead - size)?;

    let added: Vec<u8> = LABEL.encode_utf16().chain([0]).flat_map(u16::to_be_bytes).collect();
    let mut out = file[..0x20].to_vec();
    out.extend(code.iter().flat_map(|w| w.to_be_bytes()));
    out.extend_from_slice(&rest[..wide_size]);
    out.extend_from_slice(&added);
    out.extend_from_slice(&rest[wide_size..]);
    out[8..12].copy_from_slice(&(code.len() as u32 * 4).to_be_bytes());
    out[12..16].copy_from_slice(&((wide_size + added.len()) as u32).to_be_bytes());
    out[16..20].copy_from_slice(&(wide_count + 1).to_be_bytes());
    while out.len() > file.len() && out.last() == Some(&0) {
        out.pop();
    }
    if out.len() > file.len() {
        return Err(format!("the new script is {} bytes too large", out.len() - file.len()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Removing words moves later code addresses down and keeps earlier
    /// ones; a jump into the removed words is refused.
    #[test]
    fn remove_relocates() {
        let w = |o: u32, a: u32| o << 24 | a;
        let mut code = vec![w(op::JUMP, 4), w(op::CALL, 1), 0, 0, w(op::JUMP, 0)];
        remove(&mut code, 2, 2).unwrap();
        assert_eq!(code, vec![w(op::JUMP, 2), w(op::CALL, 1), w(op::JUMP, 0)]);
        let mut code = vec![w(op::JUMP, 2), 0, 0, 0];
        assert!(remove(&mut code, 1, 2).is_err());
        // A switch of 4 cases is followed by 2 words of offsets; the jump
        // after them is code.
        let mut code = vec![0, 0, 0x4c04_0003, 0xffeb_ffe5, 0xfff7_fff1, w(op::JUMP, 7), 0, 0];
        remove(&mut code, 6, 1).unwrap();
        assert_eq!(code[5], w(op::JUMP, 6));
    }

    /// The podium script gets the new line and still fits, in the build
    /// in `RUMBLE_TEST_BUILD` or build/ (skipped without one).
    #[test]
    fn script_patch_fits() {
        let dir = std::env::var_os("RUMBLE_TEST_BUILD")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build"));
        let Ok(file) = std::fs::read(dir.join("assets/script.arc/CItem_information.pkc")) else { return };
        let dol = std::fs::read(dir.join("main.dol")).unwrap();
        let dol = rumble_formats::dol::Dol::parse(&dol).unwrap();
        let mut image = vec![0; 0x60_0000];
        for s in &dol.sections {
            let at = (s.addr - IMAGE_START) as usize;
            image[at..at + s.data.len()].copy_from_slice(s.data);
        }
        let natives = super::super::multiplay::natives_in(&image).unwrap();
        let new = patch_script(&file, &natives).unwrap();
        assert!(new.len() <= file.len());
        std::fs::write(std::env::temp_dir().join("CItem_information.patched.pkc"), &new).unwrap();
    }
}
