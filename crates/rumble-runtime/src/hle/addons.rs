//! Optional changes the player picks in setup ("Addons"; see
//! `docs/addons.md`). Like the other changes in `hle`, they are made to
//! the game's files as they are unpacked, so every backend runs them.
//!
//! - `custom_sparkle` (Rumble Weekend): shinies sparkle with stars, in
//!   yellow and cyan, at random angles. Weekend shinies are guaranteed
//!   befriends, and an enemy that is shows the `drop` sparkle (effect 80,
//!   from `CEnemyPii.pkc`/`CBossPii.pkc`). Two copies of `drop` are made in
//!   `main.breff`, with the star texture, random rotation, half the rate
//!   and a yellow or a cyan colour, under the names of effects 89 (`test`)
//!   and 78 (`otamesi_out`), which the game doesn't play. The scripts' call
//!   that plays `drop` calls a new function instead, which plays both for a
//!   shiny and `drop` for anything else.
//!   The floor toy a knocked-out shiny leaves (`CDummy_ball.pkc`, effects
//!   84-87) gets the same, plus its ring (effect 79, `otamesi_in`: a copy
//!   of `item_capsule_01` that spawns only the ring).

use super::multiplay::{natives, op, Asm, IMAGE_START};
use crate::mem::Memory;
use rumble_formats::u8arc;
use std::collections::HashMap;

/// What the player picked.
#[derive(Debug, Clone, Copy, Default)]
pub struct Addons {
    /// Shinies sparkle with stars, in yellow and cyan.
    pub custom_sparkle: bool,
}

/// The guaranteed-befriend sparkle, and the effects (id, name in the
/// effect table) its yellow and cyan star copies take over.
const DROP: u32 = 80;
const YELLOW: (u32, &str) = (89, "test");
const CYAN: (u32, &str) = (78, "otamesi_out");

/// Unused emitters whose space the copies go in (`drop` is 0x340 bytes;
/// `test` and `otamesi_out` themselves are smaller).
const YELLOW_SLOT: &str = "item_bakuhatu_star_old";
const CYAN_SLOT: &str = "hit_critical_tmp";

/// The floor toy's sparkle (`item_capsule_01`) spawns two children, its
/// sparkles and the ring under the toy. A shiny toy plays the stars and a
/// copy that spawns only the ring, under effect 79's name, in the space of
/// an unused emitter.
const RING: (u32, &str) = (79, "otamesi_in");
const RING_SLOT: &str = "kaifuku_tmp";

/// Each copy's colour: red, green (a base and the most a particle's
/// differs from it, either way) and blue. `drop`'s are ff, 88 +- 6f, 00:
/// red to yellow.
const YELLOW_RGB: (u8, (u8, u8), u8) = (0xff, (0xe0, 0x10), 0x00);
const CYAN_RGB: (u8, (u8, u8), u8) = (0x00, (0xe0, 0x10), 0xff);

pub fn configure(mem: &mut Memory, mut addons: Addons) {
    if addons.custom_sparkle {
        if !mem.multiplay.weekend {
            addons.custom_sparkle = false;
        } else if let Err(e) = check_effect_ids(mem) {
            eprintln!("custom sparkle: off: {e}");
            addons.custom_sparkle = false;
        }
    }
    mem.multiplay.addons = addons;
}

/// The effect table (24-byte entries: id, name, ...) gives effects 89 and
/// 78 the names the star copies are filed under.
fn check_effect_ids(mem: &mut Memory) -> Result<(), String> {
    let image = mem.read_bytes(IMAGE_START, 0x0060_0000).map_err(|f| format!("{f:?}"))?;
    let word = |at: usize| image.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()));
    let named = |at: usize, name: &str| {
        let Some(ptr) = word(at).and_then(|p| p.checked_sub(IMAGE_START)) else { return false };
        image.get(ptr as usize..ptr as usize + name.len() + 1).is_some_and(|s| &s[..name.len()] == name.as_bytes() && s[name.len()] == 0)
    };
    for (id, name) in [YELLOW, CYAN, RING, (DROP, "drop")] {
        // Entry `id` of a table whose entry 0 has id 0.
        let found = (0..image.len() - 4).step_by(4).any(|at| {
            word(at) == Some(id) && named(at + 4, name) && at >= id as usize * 24 && word(at - id as usize * 24) == Some(0)
        });
        if !found {
            return Err(format!("effect {id} isn't {name}"));
        }
    }
    Ok(())
}

/// Called with every file the game unpacks, U8 archive or not.
pub fn unpacked_file(mem: &mut Memory, out: &mut [u8]) {
    if !out.starts_with(b"REFF") {
        return;
    }
    if mem.multiplay.addons.custom_sparkle {
        if let Err(e) = star_sparkles(out) {
            eprintln!("custom sparkle: effects left as they were: {e}");
        }
    }
}

/// Makes the yellow and cyan star copies of `drop` in `main.breff`.
fn star_sparkles(out: &mut [u8]) -> Result<(), String> {
    let all = reff::emitters(out).ok_or("not as expected")?;
    let find = |name: &str| all.iter().find(|(n, _)| n == name).map(|(_, e)| *e).ok_or(format!("no emitter {name}"));
    let drop = find("drop")?;
    let original = out[drop.entry..drop.entry + drop.size].to_vec();
    for ((_, name), slot, (r, g, b)) in [(YELLOW, YELLOW_SLOT, YELLOW_RGB), (CYAN, CYAN_SLOT, CYAN_RGB)] {
        let slot = find(slot)?;
        let target = find(name)?;
        if slot.size < drop.size {
            return Err(format!("{name}'s new place is too small"));
        }
        // The copy goes in the slot, and the name's table entry points at it.
        out[slot.entry..slot.entry + slot.size].fill(0);
        out[slot.entry..slot.entry + drop.size].copy_from_slice(&original);
        let copy = reff::Emitter { entry: slot.entry, size: drop.size, desc: slot.entry + 8, particle: drop.particle - drop.entry + slot.entry };
        if !reff::set_texture(out, &copy, "EFF_star") {
            return Err("no room for the texture name".into());
        }
        // `drop` mirrors its texture (a quarter of a sparkle) into a whole
        // one; the star is whole, so it is clamped, as the game's star
        // emitters do.
        let wrap = copy.particle + reff::TEXTURE_WRAP;
        out[wrap + 1] &= !0x0f;
        // Rotation about the view axis: fully random.
        out[copy.particle + reff::ROTATE_RANDOM_Z] = 100;
        out[copy.particle..copy.particle + 4].copy_from_slice(&[r, g.0, b, 0xff]);
        reff::set_green(out, &copy, g).ok_or("drop's colour isn't animated as expected")?;
        // Two emitters: half the rate each.
        let rate = copy.desc + reff::EMIT_RATE;
        let half = f32::from_be_bytes(out[rate..rate + 4].try_into().unwrap()) / 2.0;
        out[rate..rate + 4].copy_from_slice(&half.to_be_bytes());
        reff::repoint(out, &target, slot.entry, drop.size);
    }

    let capsule = find("item_capsule_01")?;
    let (slot, target) = (find(RING_SLOT)?, find(RING.1)?);
    if slot.size < capsule.size {
        return Err(format!("{}'s new place is too small", RING.1));
    }
    let original = out[capsule.entry..capsule.entry + capsule.size].to_vec();
    out[slot.entry..slot.entry + slot.size].fill(0);
    out[slot.entry..slot.entry + capsule.size].copy_from_slice(&original);
    let copy = reff::Emitter {
        entry: slot.entry,
        size: capsule.size,
        desc: slot.entry + 8,
        particle: capsule.particle - capsule.entry + slot.entry,
    };
    reff::only_child(out, &copy, 1).ok_or("the toy's sparkle doesn't spawn its children as expected")?;
    reff::repoint(out, &target, slot.entry, capsule.size);
    Ok(())
}

/// Called with each archive the game unpacks.
pub fn unpacked(mem: &mut Memory, out: &mut [u8]) {
    if !mem.multiplay.addons.custom_sparkle {
        return;
    }
    let Ok(files) = u8arc::files(out) else { return };
    let base = out.as_ptr() as usize;
    let scripts: Vec<(String, usize, usize)> = files
        .iter()
        .filter(|f| matches!(f.path.rsplit('/').next(), Some("CEnemyPii.pkc" | "CBossPii.pkc" | "CDummy_ball.pkc")))
        .map(|f| (f.path.clone(), f.data.as_ptr() as usize - base, f.data.len()))
        .collect();
    drop(files);
    if scripts.is_empty() {
        return;
    }
    let natives = match natives(mem) {
        Ok(n) => n,
        Err(e) => return eprintln!("addons: {e}"),
    };
    for (path, at, len) in scripts {
        let sparkle = if path.ends_with("CDummy_ball.pkc") { floor_sparkle_script } else { shiny_sparkle_script };
        if let Err(e) = sparkle(&mut out[at..at + len], &natives) {
            eprintln!("custom sparkle: {path} left as it was: {e}");
        }
    }
}

fn read_code(file: &[u8]) -> Result<Vec<u32>, String> {
    let rd = |at: usize| file.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or("script too short");
    if rd(0)? != 0x706b_1a20 {
        return Err("not a script".into());
    }
    (0..rd(8)? as usize / 4).map(|i| rd(0x20 + i * 4).map_err(String::from)).collect()
}

fn write_code(file: &mut [u8], code: &[u32]) {
    for (i, w) in code.iter().enumerate() {
        file[0x20 + i * 4..0x24 + i * 4].copy_from_slice(&w.to_be_bytes());
    }
}

/// Instructions whose operand is a code address.
fn has_target(w: u32) -> bool {
    matches!(w >> 24, op::CALL | op::JUMP | op::JUMP_IF_NOT | 0x24 | 0x25 | 0x26 | op::FUNCTION_REF)
}

/// Where a new function of `size` words can go: in place of a function
/// nothing calls, refers to or jumps into, after a jump over it, holding
/// none of `sites`.
fn free_function(code: &[u32], size: usize, sites: &[usize]) -> Result<usize, String> {
    let targets: Vec<(usize, usize)> =
        code.iter().enumerate().filter(|(_, &w)| has_target(w)).map(|(i, &w)| (i, (w & 0xff_ffff) as usize)).collect();
    (1..code.len())
        .filter(|&f| code[f] == op::FUNCTION << 24 && code[f - 1] >> 24 == op::JUMP)
        .map(|f| (f, (code[f - 1] & 0xff_ffff) as usize))
        .find(|&(f, end)| {
            end > f
                && end - f >= size
                && end <= code.len()
                && !sites.iter().any(|s| (f..end).contains(s))
                && !targets.iter().any(|&(from, to)| (f..end).contains(&to) && !(f..end).contains(&from))
        })
        .map(|(f, _)| f)
        .ok_or_else(|| "no room for the new function".into())
}

/// The enemy script's `self; int 80; int 0; method_newEffectWithRelDir;
/// pop` (right after the boss's same call with 81) becomes `call sparkle;
/// pop; jump past`. `sparkle` plays the stars for a shiny and `drop`
/// otherwise.
fn shiny_sparkle_script(file: &mut [u8], natives: &HashMap<String, u32>) -> Result<(), String> {
    let mut code = read_code(file)?;
    let new_effect = natives.get("method_newEffectWithRelDir").map(|&n| op::NATIVE << 24 | n).ok_or("no native")?;
    let int = |v: u32| op::INT << 24 | v;
    let call = [op::SELF << 24, int(DROP), int(0), new_effect, op::POP << 24];
    let sites: Vec<usize> = (6..code.len().saturating_sub(5))
        .filter(|&i| code[i..i + 5] == call && code[i - 6..i].contains(&int(DROP + 1)))
        .collect();
    let [site] = sites[..] else { return Err(format!("{} drop sparkle calls", sites.len())) };

    let sparkle = |base: u32| -> Result<Vec<u32>, String> {
        let mut a = Asm::new(base, natives);
        a.op(op::FUNCTION, 0).op(op::FRAME, 0);
        a.op(op::SELF, 0).native("method_getPPD").native("method_isRareColor").jump(op::JUMP_IF_NOT, "normal");
        a.op(op::SELF, 0).int(YELLOW.0 as i32).int(0).call_native("method_newEffectWithRelDir");
        a.op(op::SELF, 0).int(CYAN.0 as i32).int(0).native("method_newEffectWithRelDir").op(op::RETURN, 0);
        a.label("normal");
        a.op(op::SELF, 0).int(DROP as i32).int(0).native("method_newEffectWithRelDir").op(op::RETURN, 0);
        a.finish()
    };
    let dead = free_function(&code, sparkle(0)?.len(), &[site])?;
    let function = sparkle(dead as u32)?;
    code[dead..dead + function.len()].copy_from_slice(&function);
    code[site..site + 3].copy_from_slice(&[op::CALL << 24 | dead as u32, op::POP << 24, op::JUMP << 24 | (site + 5) as u32]);
    write_code(file, &code);
    Ok(())
}

/// The floor toy a knocked-out enemy leaves (`CDummy_ball.pkc`) sparkles
/// with effects 84-87 (`item_capsule_01`-`04`, by a switch), each played
/// by `load g; int id; int 180; method_newEffectWithAbsDir; pop`. Each
/// becomes `load g; int id; call sparkle; pop; jump past`; `sparkle(target,
/// id)` plays the ring and the stars if the toy is shiny (global 0xc, which
/// the toy sets from `method_isRareColor` of the Pokemon it came from) and
/// `id` otherwise.
fn floor_sparkle_script(file: &mut [u8], natives: &HashMap<String, u32>) -> Result<(), String> {
    const SHINY: u32 = 0xc;
    let mut code = read_code(file)?;
    let new_effect = natives.get("method_newEffectWithAbsDir").map(|&n| op::NATIVE << 24 | n).ok_or("no native")?;
    let is_rare = natives.get("method_isRareColor").map(|&n| op::NATIVE << 24 | n).ok_or("no native")?;
    let int = |v: u32| op::INT << 24 | v;
    // The toy's shiny flag: `load ppd; method_isRareColor; store 0xc`, once.
    let stores: Vec<usize> = (1..code.len()).filter(|&i| code[i] == op::STORE << 24 | SHINY).collect();
    if stores.len() != 1 || code[stores[0] - 1] != is_rare {
        return Err("the toy's shiny flag isn't as expected".into());
    }
    let sites: Vec<usize> = (1..code.len().saturating_sub(5))
        .filter(|&i| {
            code[i] >> 24 == op::LOAD
                && (84..=87).any(|id| code[i + 1] == int(id))
                && code[i + 2..i + 5] == [int(180), new_effect, op::POP << 24]
        })
        .collect();
    if sites.len() != 4 {
        return Err(format!("{} floor sparkle calls", sites.len()));
    }

    let sparkle = |base: u32| -> Result<Vec<u32>, String> {
        let local = |n: u32| op::LOCAL | n;
        let mut a = Asm::new(base, natives);
        a.op(op::FUNCTION, 0).op(op::FRAME, 2 << 16 | 2);
        a.op(op::LOAD, SHINY).jump(op::JUMP_IF_NOT, "normal");
        a.op(op::LOAD, local(0)).int(RING.0 as i32).int(180).call_native("method_newEffectWithAbsDir");
        a.op(op::LOAD, local(0)).int(YELLOW.0 as i32).int(180).call_native("method_newEffectWithAbsDir");
        a.op(op::LOAD, local(0)).int(CYAN.0 as i32).int(180).native("method_newEffectWithAbsDir").op(op::RETURN, 0);
        a.label("normal");
        a.op(op::LOAD, local(0)).op(op::LOAD, local(1)).int(180).native("method_newEffectWithAbsDir").op(op::RETURN, 0);
        a.finish()
    };
    let dead = free_function(&code, sparkle(0)?.len(), &sites)?;
    let function = sparkle(dead as u32)?;
    code[dead..dead + function.len()].copy_from_slice(&function);
    for site in sites {
        code[site + 2..site + 5].copy_from_slice(&[op::CALL << 24 | dead as u32, op::POP << 24, op::JUMP << 24 | (site + 5) as u32]);
    }
    write_code(file, &code);
    Ok(())
}

/// NW4R REFF (`.breff`) emitters, as laid out in Pokemon Rumble's
/// `effect/main.breff` (see `docs/addons.md`).
pub mod reff {
    /// Where the emitter table starts: after the file header (0x10), the
    /// REFF block header and the project name.
    const TABLE: usize = 0x30;
    /// In the `EmitterDesc`: particles emitted per frame (f32).
    pub const EMIT_RATE: usize = 0x10;
    /// In the `ParticleParameterDesc`: how random the rotation about the
    /// view axis is, in percent.
    pub const ROTATE_RANDOM_Z: usize = 0x7b;
    /// In the `ParticleParameterDesc`: the textures' wrap modes (u16; the
    /// low 4 bits are the first texture's S and T).
    pub const TEXTURE_WRAP: usize = 0x74;

    fn u16_at(d: &[u8], at: usize) -> Option<usize> {
        d.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
    }
    fn u32_at(d: &[u8], at: usize) -> Option<usize> {
        d.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()) as usize)
    }

    /// An emitter's place in the file.
    #[derive(Debug, Clone, Copy)]
    pub struct Emitter {
        /// Start of its entry.
        pub entry: usize,
        pub size: usize,
        /// The `EmitterDesc` (emission, velocity, draw settings).
        pub desc: usize,
        /// The `ParticleParameterDesc` (colours, size, rotation, textures).
        pub particle: usize,
    }

    /// Every emitter's name and place, with where its table entry's offset
    /// and size are.
    fn table(d: &[u8]) -> Option<Vec<(String, Emitter, usize)>> {
        let count = u16_at(d, TABLE + 4)?;
        let mut at = TABLE + 8;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let len = u16_at(d, at)?;
            let name = d.get(at + 2..at + 2 + len)?;
            let name = String::from_utf8_lossy(name.split(|&b| b == 0).next()?).into_owned();
            at += 2 + len;
            let (offset, size) = (u32_at(d, at)?, u32_at(d, at + 4)?);
            let entry = TABLE + offset;
            let desc_size = u32_at(d, entry + 4)?;
            let particle = entry + 8 + desc_size + 4;
            if particle + 0x8a > entry + size || entry + size > d.len() {
                return None;
            }
            out.push((name, Emitter { entry, size, desc: entry + 8, particle }, at));
            at += 8;
        }
        Some(out)
    }

    pub fn emitters(d: &[u8]) -> Option<Vec<(String, Emitter)>> {
        Some(table(d)?.into_iter().map(|(n, e, _)| (n, e)).collect())
    }

    pub fn emitter(d: &[u8], name: &str) -> Option<Emitter> {
        emitters(d)?.into_iter().find(|(n, _)| n == name).map(|(_, e)| e)
    }

    /// Points `target`'s table entry at the emitter data at `entry`.
    pub fn repoint(d: &mut [u8], target: &Emitter, entry: usize, size: usize) {
        let Some(at) = table(d).and_then(|t| t.into_iter().find(|(_, e, _)| e.entry == target.entry).map(|(_, _, at)| at)) else {
            return;
        };
        d[at..at + 4].copy_from_slice(&((entry - TABLE) as u32).to_be_bytes());
        d[at + 4..at + 8].copy_from_slice(&(size as u32).to_be_bytes());
    }

    /// Renames an emitter's first texture in place. The name record keeps
    /// its length (the name is NUL-padded), so `name` must not be longer
    /// than the one it replaces.
    pub fn set_texture(d: &mut [u8], e: &Emitter, name: &str) -> bool {
        let at = e.particle + 0x88;
        let Some(len) = u16_at(d, at) else { return false };
        if name.len() + 1 > len || at + 2 + len > e.entry + e.size {
            return false;
        }
        let slot = &mut d[at + 2..at + 2 + len];
        slot.fill(0);
        slot[..name.len()].copy_from_slice(name.as_bytes());
        true
    }

    /// Makes an emitter whose child track spawns two children by name
    /// (`item_capsule_01`: its sparkles, index 0, then the ring, index 1)
    /// spawn only child `index`: the key table keeps one key, which names
    /// it. A key is 24 bytes and ends with the child's name index (u16).
    pub fn only_child(d: &mut [u8], e: &Emitter, index: u16) -> Option<()> {
        let tracks = e.particle + u32_at(d, e.particle - 4)?;
        let count = u16_at(d, tracks)?;
        let emit_count = u16_at(d, tracks + 4 + count * 8)?;
        let mut track = tracks + 4 + count * 8 + 4 + emit_count * 8;
        let sizes = tracks + 4 + count * 4;
        for k in 0..count {
            let size = u32_at(d, sizes + k * 4)?;
            if d.get(track..track + 3)? == [0xac, 0, 5] {
                let keys = track + 0x20;
                let last = |n: usize| u16_at(d, keys + 4 + n * 24 + 22);
                if u16_at(d, keys)? != 2 || last(0)? != 0 || last(1)? != 1 {
                    return None;
                }
                d[keys..keys + 2].copy_from_slice(&1u16.to_be_bytes());
                d[keys + 4 + 22..keys + 4 + 24].copy_from_slice(&index.to_be_bytes());
                return Some(());
            }
            track += size;
        }
        None
    }

    /// Sets a particle's green (a base and the most it differs from it,
    /// either way) in an emitter whose colour track animates green only
    /// (`drop`'s): the first particle track, a byte curve (type 0) on
    /// colour 1 (kind 0) with only green enabled, whose range table has one
    /// pair.
    pub fn set_green(d: &mut [u8], e: &Emitter, (base, spread): (u8, u8)) -> Option<()> {
        let tracks = e.particle + u32_at(d, e.particle - 4)?;
        let count = u16_at(d, tracks)?;
        let emit_count = u16_at(d, tracks + 4 + count * 8)?;
        let track = tracks + 4 + count * 8 + 4 + emit_count * 8;
        let header = d.get(track..track + 4)?;
        if count == 0 || header != [0xac, 0, 0, 0b10] {
            return None;
        }
        let range = track + 0x20 + u32_at(d, track + 0x0c)?;
        if u32_at(d, track + 0x10)? != 8 || u16_at(d, range)? != 1 {
            return None;
        }
        d[range + 4] = base;
        d[range + 5] = spread;
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The build in `RUMBLE_TEST_BUILD` or build/ (the tests are skipped
    /// without one).
    fn build_dir() -> std::path::PathBuf {
        std::env::var_os("RUMBLE_TEST_BUILD")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build"))
    }

    fn natives_of(dir: &std::path::Path) -> HashMap<String, u32> {
        let dol = std::fs::read(dir.join("main.dol")).unwrap();
        let dol = rumble_formats::dol::Dol::parse(&dol).unwrap();
        let mut image = vec![0; 0x60_0000];
        for s in &dol.sections {
            let at = (s.addr - IMAGE_START) as usize;
            image[at..at + s.data.len()].copy_from_slice(s.data);
        }
        super::super::multiplay::natives_in(&image).unwrap()
    }

    /// Effects 89 and 78 become yellow and cyan clamped stars; `drop` stays.
    #[test]
    fn star_copies() {
        let Ok(mut file) = std::fs::read(build_dir().join("assets/effect/main.breff")) else { return };
        let drop_before = reff::emitter(&file, "drop").unwrap();
        let drop_bytes = file[drop_before.entry..drop_before.entry + drop_before.size].to_vec();
        star_sparkles(&mut file).unwrap();
        let drop = reff::emitter(&file, "drop").unwrap();
        assert_eq!(&file[drop.entry..drop.entry + drop.size], &drop_bytes[..]);
        for ((_, name), rgb) in [(YELLOW, [0xff, 0xe0, 0x00]), (CYAN, [0x00, 0xe0, 0xff])] {
            let e = reff::emitter(&file, name).unwrap();
            assert_eq!(e.size, drop.size);
            assert_eq!(&file[e.particle..e.particle + 3], &rgb);
            assert_eq!(&file[e.particle + 0x8a..e.particle + 0x93], b"EFF_star\0");
            assert_eq!(file[e.particle + reff::ROTATE_RANDOM_Z], 100);
            assert_eq!(file[e.particle + reff::TEXTURE_WRAP + 1] & 0x0f, 0);
        }
        let ring = reff::emitter(&file, RING.1).unwrap();
        assert_eq!(ring.size, reff::emitter(&file, "item_capsule_01").unwrap().size);
        assert!(reff::only_child(&mut file.clone(), &ring, 1).is_none(), "already one key");
    }

    /// Both enemy scripts and the floor toy's get the shiny check.
    #[test]
    fn script_patches() {
        let dir = build_dir();
        let Ok(_) = std::fs::metadata(dir.join("assets/script.arc/CEnemyPii.pkc")) else { return };
        let natives = natives_of(&dir);
        type Patch = fn(&mut [u8], &HashMap<String, u32>) -> Result<(), String>;
        let patches: [(&str, Patch); 3] =
            [("CEnemyPii.pkc", shiny_sparkle_script), ("CBossPii.pkc", shiny_sparkle_script), ("CDummy_ball.pkc", floor_sparkle_script)];
        for (name, patch) in patches {
            let mut file = std::fs::read(dir.join("assets/script.arc").join(name)).unwrap();
            let before = file.clone();
            patch(&mut file, &natives).unwrap();
            assert_eq!(file.len(), before.len());
            assert_ne!(file, before);
        }
    }
}
