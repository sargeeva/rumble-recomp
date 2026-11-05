//! The hub's multiplayer pedestal, replaced by Ditto (see
//! `docs/multiplayer-ditto.md`). Multiplayer is not implemented, so the
//! pedestal that starts it becomes the game's own Ditto, idling, and talking
//! to it says so. The first time on a save, it also drops a silver coin.
//!
//! Both changes are made to the game's own files as they are unpacked
//! (`CXSecureUncompressLH`), in the archive in RAM, from data in the user's
//! files, so nothing new ships and every backend runs the same code:
//!
//! - `item.arc`'s `terminal_multiplay.brres` becomes a Ditto model file:
//!   `pii_1-151_387-492.arc`'s `METAMON.brres` with the textures it uses
//!   and the idle animation of its skeleton type moved in from that
//!   archive's shared files.
//! - `script.arc`'s `CItem_multi.pkc`, the pedestal's script, gets a new
//!   main loop and interaction (see `patch_script`).
//!
//! `State::original` (`[game] original_multiplayer` in config.toml, not
//! shown in the menu) leaves both files as they are, for whoever implements
//! multiplayer.

use crate::mem::Memory;
use rumble_formats::{brres, u8arc};
use std::collections::HashMap;

#[derive(Default)]
pub struct State {
    /// Keep the original pedestal and its multiplayer prompt.
    pub original: bool,
    /// The Ditto model file, once the Pokemon archive has been unpacked.
    ditto: Option<Vec<u8>>,
    /// Where `terminal_multiplay.brres` was unpacked (address and size) if
    /// that came first; it is replaced once Ditto is known.
    pending: Option<(u32, u32)>,
    /// Rumble Weekend is running (`super::playinfo`).
    pub weekend: bool,
    /// The shiny count is answered (`super::playinfo::install`).
    pub shiny_count: bool,
    /// The addons the player picked (`super::addons`).
    pub addons: super::addons::Addons,
    /// The title copyright's length in characters, until the "Recomped"
    /// line is added to it (`super::title`).
    pub title_note: Option<u32>,
}

/// The model the pedestal script loads, and its animation (the original's
/// falls into place; Ditto's idles).
const MODEL: &str = "terminal_multiplay.brres";
const FALL: &str = "terminal_multiplay__fall";
const IDLE: &str = "terminal_multiplay__wait";

/// What talking to Ditto says.
const MESSAGE: &str = "Sargeeva: Multiplayer has not been implemented yet.";

/// The save flag (in the save slot's 64 flags) set once the coin dropped.
/// The game uses flags up to 31.
const COIN_FLAG: u32 = 40;
/// The coin's value: 10 to 99 is the silver coin (`CItem_money.pkc`).
const COIN_VALUE: u32 = 10;
/// Ditto's size: about the pedestal's footprint, which its collision is
/// made for (Ditto's own model is about 70% as wide).
const DITTO_SCALE: f32 = 1.4;

/// Applies the setting at boot, before code is compiled or translated
/// (blocks whose code differs from RAM are interpreted, so every backend
/// runs the patch). With Ditto, the pause menu's Multiplayer entry is left
/// out too (see `hide_pause_entry`).
pub fn configure(mem: &mut Memory, original: bool) {
    mem.multiplay.original = original;
    mem.multiplay.weekend = super::playinfo::is_weekend(mem);
    if mem.multiplay.weekend {
        if let Err(e) = super::playinfo::install(mem) {
            eprintln!("Play Info: no shiny count: {e}");
        }
    }
    if !original {
        if let Err(e) = hide_pause_entry(mem) {
            eprintln!("multiplayer pause entry left in: {e}");
        }
    }
}

/// The pause menu (`CPauseMenu`) adds its Multiplayer entry (message 0x86)
/// in the hub once save flag 3 is set: `li r3, 3; bl CheckFlag;
/// cmpwi r3, 0; beq skip`, then `lis r3, 6; addi r3, r3, 0x86` a few
/// instructions on. The `beq` becomes `b`, so it is always skipped.
fn hide_pause_entry(mem: &mut Memory) -> Result<(), String> {
    let image = mem.read_bytes(IMAGE_START, 0x0060_0000).map_err(|f| format!("{f:?}"))?;
    let (at, b) = pause_entry_branch(&image)?;
    mem.w32(at, b).map_err(|f| format!("{f:?}"))
}

/// Where the pause menu's Multiplayer `beq` is in a DOL image loaded at
/// `IMAGE_START`, and the unconditional branch to put there.
fn pause_entry_branch(image: &[u8]) -> Result<(u32, u32), String> {
    let word = |i: usize| u32::from_be_bytes(image[i * 4..i * 4 + 4].try_into().unwrap());
    let found: Vec<usize> = (0..image.len() / 4 - 10)
        .filter(|&i| {
            word(i) == 0x3860_0003
                && word(i + 1) >> 26 == 18
                && word(i + 2) == 0x2c03_0000
                && word(i + 3) >> 16 == 0x4182
                && (i + 4..i + 10).any(|j| word(j) == 0x3863_0086)
        })
        .collect();
    let [i] = found[..] else { return Err(format!("{} matches", found.len())) };
    let beq = word(i + 3);
    if beq & 0x8003 != 0 {
        return Err("unexpected branch".into());
    }
    Ok((IMAGE_START + (i as u32 + 3) * 4, 0x4800_0000 | beq & 0xfffc))
}

/// Called with each archive the game unpacks, before it reaches RAM at
/// `dst`.
pub fn unpacked(mem: &mut Memory, dst: u32, out: &mut [u8]) {
    if !u8arc::is_u8(out) {
        return;
    }
    super::title::unpacked(mem, out);
    super::addons::unpacked(mem, out);
    if mem.multiplay.weekend {
        super::playinfo::unpacked(mem, out);
    }
    if mem.multiplay.original {
        return;
    }
    let Ok(files) = u8arc::files(out) else { return };
    let base = out.as_ptr() as usize;
    let find = |name: &str| {
        files
            .iter()
            .find(|f| f.path.rsplit('/').next() == Some(name))
            .map(|f| (f.data.as_ptr() as usize - base, f.data.len()))
    };
    let (metamon, textures, anim) = (find("METAMON.brres"), find("textures_1-151_387-492.brres"), find("anim.brres"));
    let model = find(MODEL);
    let script = find("CItem_multi.pkc");
    drop(files);

    if let (Some(m), Some(t), Some(a)) = (metamon, textures, anim) {
        let part = |(at, len): (usize, usize)| &out[at..at + len];
        match ditto_model(part(m), part(t), part(a)) {
            Ok(file) => mem.multiplay.ditto = Some(file),
            Err(e) => eprintln!("multiplayer pedestal: building Ditto: {e}"),
        }
        if let (Some((addr, size)), Some(file)) = (mem.multiplay.pending.take(), mem.multiplay.ditto.clone()) {
            // Still the original file there?
            if mem.read_bytes(addr, 12).is_ok_and(|h| &h[..4] == b"bres" && u32::from_be_bytes(h[8..12].try_into().unwrap()) == size) {
                let _ = replace_in_ram(mem, addr, size, &file);
            }
        }
    }
    if let Some((at, len)) = model {
        match &mem.multiplay.ditto {
            Some(file) if file.len() <= len => replace(&mut out[at..at + len], file),
            Some(_) => eprintln!("multiplayer pedestal: Ditto's model is larger than the pedestal's"),
            None => mem.multiplay.pending = Some((dst + at as u32, len as u32)),
        }
    }
    if let Some((at, len)) = script {
        let result = natives(mem).and_then(|n| patch_script(&out[at..at + len], &n));
        match result {
            Ok(new) => replace(&mut out[at..at + len], &new),
            Err(e) => eprintln!("multiplayer pedestal: script left as it was: {e}"),
        }
    }
}

fn replace(slot: &mut [u8], new: &[u8]) {
    slot[..new.len()].copy_from_slice(new);
    slot[new.len()..].fill(0);
}

fn replace_in_ram(mem: &mut Memory, addr: u32, size: u32, new: &[u8]) -> Result<(), crate::mem::Fault> {
    if new.len() > size as usize {
        return Ok(());
    }
    let mut buf = new.to_vec();
    buf.resize(size as usize, 0);
    mem.load(addr, &buf)
}

/// A model file for the pedestal: Ditto (`METAMON.brres`) named as the
/// pedestal's model, with the textures and palette it names from the shared
/// texture file, and its skeleton type's idle animation from `anim.brres`.
pub fn ditto_model(metamon: &[u8], textures: &[u8], anim: &[u8]) -> Result<Vec<u8>, brres::BrresError> {
    let mut b = brres::Builder::default();
    b.add_model_file(metamon, "terminal_multiplay")?;
    for tex in ["Pii_METAMON_eye", "high_light", "light_tex"] {
        b.add(textures, "Textures(NW4R)", tex, None)?;
    }
    b.add(textures, "Palettes(NW4R)", "Pii_METAMON_eye", None)?;
    b.add(anim, "AnmChr(NW4R)", "slime_anim_kamae", Some(IDLE))?;
    Ok(b.build())
}

/// The script engine's native functions by name, with the index scripts
/// call them by, from the DOL in RAM.
pub(super) fn natives(mem: &mut Memory) -> Result<HashMap<String, u32>, String> {
    let image = mem.read_bytes(IMAGE_START, 0x0060_0000).map_err(|f| format!("{f:?}"))?;
    natives_in(&image)
}

/// Where `natives_in`'s image starts: the DOL's first text section.
pub const IMAGE_START: u32 = 0x8000_4000;

/// The native functions in an image of the DOL as loaded at `IMAGE_START`.
/// The game keeps them in a table of (function, name, argument count),
/// entry 0 empty; it is found by its first name.
pub fn natives_in(image: &[u8]) -> Result<HashMap<String, u32>, String> {
    Ok(native_entries(image)?.into_iter().enumerate().map(|(i, (name, _))| (name, i as u32 + 1)).collect())
}

/// The native table's names and functions, in index order from 1.
pub(super) fn native_entries(image: &[u8]) -> Result<Vec<(String, u32)>, String> {
    let word = |at: usize| u32::from_be_bytes(image[at..at + 4].try_into().unwrap());
    let cstr = |addr: u32| -> Option<&str> {
        let s = image.get(addr.checked_sub(IMAGE_START)? as usize..)?;
        std::str::from_utf8(&s[..s.iter().position(|&b| b == 0)?]).ok()
    };
    let first = b"NewStageCamera\0";
    let name_at = image.windows(first.len()).position(|w| w == first).ok_or("no native table")? as u32 + IMAGE_START;
    let entry = (0..image.len() - 4)
        .step_by(4)
        .find(|&i| word(i) == name_at)
        .map(|i| i - 4)
        .ok_or("no native table")?;
    if entry < 12 || (entry - 12..entry).step_by(4).any(|i| word(i) != 0) {
        return Err("native table not as expected".into());
    }
    let mut entries = Vec::new();
    for e in (entry..image.len() - 12).step_by(12) {
        let (f, name) = (word(e), word(e + 4));
        let Some(name) = (f != 0).then(|| cstr(name)).flatten() else { break };
        entries.push((name.to_string(), f));
    }
    Ok(entries)
}

/// Script (`.pkc`) instructions: an 8-bit opcode and a 24-bit operand.
pub(super) mod op {
    pub const NOP: u32 = 0x00;
    pub const NEG: u32 = 0x03;
    pub const STORE: u32 = 0x0a;
    pub const NE: u32 = 0x0c;
    pub const NOT: u32 = 0x11;
    pub const LOAD: u32 = 0x19;
    pub const CALL: u32 = 0x1a;
    pub const NATIVE: u32 = 0x1b;
    pub const JUMP: u32 = 0x1c;
    pub const JUMP_IF_NOT: u32 = 0x1d;
    pub const RETURN: u32 = 0x1e;
    pub const POP: u32 = 0x1f;
    pub const FUNCTION: u32 = 0x20;
    /// After `FUNCTION`: argument count << 16 | local count.
    pub const FRAME: u32 = 0x01;
    pub const END: u32 = 0x23;
    pub const SELF: u32 = 0x27;
    /// A native taking a variable number of arguments; the next word holds
    /// their count.
    pub const NATIVE_VARARGS: u32 = 0x28;
    pub const INT: u32 = 0x40;
    /// The float is the next word.
    pub const FLOAT: u32 = 0x41;
    pub const STRING: u32 = 0x42;
    pub const WSTRING: u32 = 0x43;
    pub const FUNCTION_REF: u32 = 0x44;
    /// Operand for `LOAD`/`STORE` of a local (else a global).
    pub const LOCAL: u32 = 0x1_0000;
}

/// Code under construction, with labels for jumps.
pub(super) struct Asm<'a> {
    base: u32,
    pub(super) words: Vec<u32>,
    labels: HashMap<&'static str, u32>,
    fixups: Vec<(usize, &'static str)>,
    natives: &'a HashMap<String, u32>,
}

impl<'a> Asm<'a> {
    pub(super) fn new(base: u32, natives: &'a HashMap<String, u32>) -> Self {
        Self { base, words: Vec::new(), labels: HashMap::new(), fixups: Vec::new(), natives }
    }
    pub(super) fn op(&mut self, op: u32, arg: u32) -> &mut Self {
        self.words.push(op << 24 | arg & 0xff_ffff);
        self
    }
    pub(super) fn int(&mut self, v: i32) -> &mut Self {
        self.op(op::INT, v as u32)
    }
    pub(super) fn float(&mut self, v: f32) -> &mut Self {
        self.op(op::FLOAT, 0);
        self.words.push(v.to_bits());
        self
    }
    pub(super) fn native(&mut self, name: &str) -> &mut Self {
        let index = self.natives.get(name).copied().unwrap_or(u32::MAX);
        self.op(op::NATIVE, index)
    }
    /// Calls native `name` and drops its result.
    pub(super) fn call_native(&mut self, name: &str) -> &mut Self {
        self.native(name).op(op::POP, 0)
    }
    pub(super) fn label(&mut self, name: &'static str) -> &mut Self {
        self.labels.insert(name, self.base + self.words.len() as u32);
        self
    }
    pub(super) fn jump(&mut self, op: u32, to: &'static str) -> &mut Self {
        self.fixups.push((self.words.len(), to));
        self.op(op, 0)
    }
    pub(super) fn finish(mut self) -> Result<Vec<u32>, String> {
        if self.words.contains(&(op::NATIVE << 24 | 0xff_ffff)) {
            return Err("a native is missing".into());
        }
        for (at, name) in &self.fixups {
            let to = self.labels.get(name).ok_or_else(|| format!("no label {name}"))?;
            self.words[*at] |= to;
        }
        Ok(self.words)
    }
}

/// The pedestal script with Ditto's behaviour:
///
/// - Kept: everything up to `LeaveInitPart` (loading the model, which is
///   now Ditto's, the callback that notes which player touched it, and the
///   "Multiplayer" balloon), and the helpers it calls.
/// - The main part still waits, hidden, until the hub shows the pedestal
///   (flag 4), then plays Ditto's idle animation (named as the original's
///   "fall" animation was, with `wait` for `fall`) and loops on the
///   interaction.
/// - The interaction shows `MESSAGE` and, if `COIN_FLAG` is clear, sets it
///   and drops a `COIN_VALUE` coin as items drop money: a `CItem_money`
///   item with its value in object register 11 and its speeds in 12 and 13.
///   It goes where a function nothing calls was: the new script must fit
///   in the original's place in the archive.
pub fn patch_script(file: &[u8], natives: &HashMap<String, u32>) -> Result<Vec<u8>, String> {
    let rd = |at: usize| file.get(at..at + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or("script too short");
    if rd(0)? != 0x706b_1a20 {
        return Err("not a script".into());
    }
    let (code_size, wide_size, wide_count, narrow_count) = (rd(8)? as usize, rd(12)? as usize, rd(16)?, rd(24)? as usize);
    let code: Vec<u32> = (0..code_size / 4).map(|i| rd(0x20 + i * 4)).collect::<Result<_, _>>()?;
    let wide = file.get(0x20 + code_size..0x20 + code_size + wide_size).ok_or("script too short")?;
    let narrow_start = 0x20 + code_size + wide_size;
    let mut narrow: Vec<String> = Vec::new();
    let mut at = narrow_start;
    for _ in 0..narrow_count {
        let s = file.get(at..).ok_or("script too short")?;
        let end = s.iter().position(|&b| b == 0).ok_or("script too short")?;
        narrow.push(String::from_utf8_lossy(&s[..end]).into_owned());
        at += end + 1;
    }
    let narrow_end = at;
    let string = |name: &str| narrow.iter().position(|s| s == name).map(|i| i as u32).ok_or(format!("no string {name}"));
    let (model, fall, money) = (string(MODEL)?, string(FALL)?, string("CItem_money.pkc")?);
    let native = |name: &str| natives.get(name).copied().ok_or(format!("no native {name}"));
    for name in [
        "method_loadModel", "LeaveInitPart", "CheckFlag", "SetFlag", "Yield", "GetThisObject", "method_show",
        "method_play", "method_setModelScale", "PlayerMessageOpen", "MessageIsClosed", "Resume",
        "SetNewObjRegister", "NewItem", "method_setDirDeg", "method_setXYZ", "method_getX", "method_getZ",
    ] {
        native(name)?;
    }
    let is = |w: u32, op: u32, arg: u32| w == op << 24 | arg;

    // Where the init part ends: after the model load, `LeaveInitPart; pop`.
    let load = code
        .windows(2)
        .position(|w| is(w[0], op::STRING, model) && is(w[1], op::NATIVE, native("method_loadModel").unwrap()))
        .ok_or("no model load")?;
    let leave = native("LeaveInitPart")?;
    let split = (load..code.len() - 1)
        .find(|&i| is(code[i], op::NATIVE, leave) && is(code[i + 1], op::POP, 0))
        .ok_or("no LeaveInitPart")?
        + 2;
    // The rest calls the interaction poll, the wait helper (twice), the
    // per-frame helper, and the interaction function itself.
    let calls: Vec<u32> = code[split..].iter().filter(|w| *w >> 24 == op::CALL).map(|w| w & 0xff_ffff).collect();
    let [poll, wait, wait2, per_frame, interact] = calls[..] else { return Err(format!("unexpected main part ({} calls)", calls.len())) };
    if wait != wait2 || (interact as usize) < split {
        return Err("unexpected main part".into());
    }
    // Functions referenced from anywhere. (A float operand can look like a
    // call; it seldom points at a function.)
    let referenced: Vec<u32> = code
        .iter()
        .filter(|w| matches!(*w >> 24, op::CALL | op::FUNCTION_REF))
        .map(|w| w & 0xff_ffff)
        .filter(|&f| code.get(f as usize).is_some_and(|&w| is(w, op::FUNCTION, 0)))
        .collect();
    if referenced.iter().any(|&f| f as usize >= split && f != interact) {
        return Err("main part has other functions".into());
    }

    let message = wide_count;
    let ids = Ids { poll, wait, money, message, coin_flag: COIN_FLAG };
    // Put it in place of a function nothing calls: a `FUNCTION` after a
    // jump over it, with no referenced function up to the jump's target.
    let size = interaction(0, natives, &ids)?.len();
    let dead = (1..split)
        .filter(|&f| is(code[f], op::FUNCTION, 0) && code[f - 1] >> 24 == op::JUMP)
        .map(|f| (f, (code[f - 1] & 0xff_ffff) as usize))
        .find(|&(f, end)| end > f && end - f >= size && end <= split && !referenced.iter().any(|&r| (f..end).contains(&(r as usize))))
        .ok_or("no room for the interaction")?;
    let interaction_words = interaction(dead.0 as u32, natives, &ids)?;

    let mut main = Asm::new(split as u32, natives);
    main.int(4)
        .native("CheckFlag")
        .op(op::NOT, 0)
        .jump(op::JUMP_IF_NOT, "shown")
        .op(op::SELF, 0)
        .int(0)
        .call_native("method_show")
        .label("hidden")
        .int(4)
        .native("CheckFlag")
        .op(op::NOT, 0)
        .jump(op::JUMP_IF_NOT, "appear")
        .call_native("Yield")
        .jump(op::JUMP, "hidden")
        .label("appear")
        .op(op::SELF, 0)
        .int(1)
        .call_native("method_show")
        .label("shown")
        .op(op::SELF, 0)
        .float(DITTO_SCALE)
        .call_native("method_setModelScale")
        .op(op::SELF, 0)
        .op(op::STRING, fall)
        .call_native("method_play")
        .label("loop")
        .int(1)
        .jump(op::JUMP_IF_NOT, "end")
        .native("GetThisObject")
        .op(op::CALL, per_frame)
        .op(op::POP, 0)
        .op(op::CALL, dead.0 as u32)
        .op(op::POP, 0)
        .call_native("Yield")
        .jump(op::JUMP, "loop")
        .label("end")
        .op(op::END, 0);
    let main = main.finish()?;

    let mut new_code = code[..split].to_vec();
    new_code[dead.0..dead.1].fill(0);
    new_code[dead.0..dead.0 + interaction_words.len()].copy_from_slice(&interaction_words);
    new_code.extend_from_slice(&main);

    let mut out = file[..0x20].to_vec();
    out.extend(new_code.iter().flat_map(|w| w.to_be_bytes()));
    out.extend_from_slice(wide);
    let added: Vec<u8> = MESSAGE.encode_utf16().chain([0]).flat_map(u16::to_be_bytes).collect();
    out.extend_from_slice(&added);
    for (i, s) in narrow.iter().enumerate() {
        let s = if i == fall as usize { IDLE } else { s };
        out.extend_from_slice(s.as_bytes());
        out.push(0);
    }
    out.extend_from_slice(&file[narrow_end..]);
    let set = |out: &mut Vec<u8>, at: usize, v: u32| out[at..at + 4].copy_from_slice(&v.to_be_bytes());
    set(&mut out, 8, new_code.len() as u32 * 4);
    set(&mut out, 12, (wide_size + added.len()) as u32);
    set(&mut out, 16, wide_count + 1);
    if out.len() > file.len() {
        let extra = out.len() - file.len();
        // Trailing zeros after the strings can go.
        if out[out.len() - extra..].iter().all(|&b| b == 0) {
            out.truncate(file.len());
        } else {
            return Err(format!("the new script is {extra} bytes too large"));
        }
    }
    Ok(out)
}

/// What the interaction refers to: the script's helper functions (the
/// interaction poll and the wait helper), its `CItem_money.pkc` string, the
/// message's wide string and the save flag.
struct Ids {
    poll: u32,
    wait: u32,
    money: u32,
    message: u32,
    coin_flag: u32,
}

/// The interaction function, placed at word `base`: when a player has
/// touched Ditto (the game paused for it), show the message to them, wait
/// for it to close, drop the coin once per save, and let the game go on.
fn interaction(base: u32, natives: &HashMap<String, u32>, ids: &Ids) -> Result<Vec<u32>, String> {
    let local = |n: u32| op::LOCAL | n;
    let flag = ids.coin_flag as i32;
    let new_item = natives.get("NewItem").copied().ok_or("no native NewItem")?;
    let mut a = Asm::new(base, natives);
    a.op(op::FUNCTION, 0)
        .op(op::FRAME, 2)
        .op(op::CALL, ids.poll)
        .op(op::STORE, local(0))
        .op(op::LOAD, local(0))
        .int(1)
        .op(op::NEG, 0)
        .op(op::NE, 0)
        .jump(op::JUMP_IF_NOT, "done")
        .op(op::LOAD, local(0))
        .op(op::WSTRING, ids.message)
        .call_native("PlayerMessageOpen")
        .label("message")
        .native("MessageIsClosed")
        .op(op::NOT, 0)
        .jump(op::JUMP_IF_NOT, "closed")
        .native("GetThisObject")
        .op(op::CALL, ids.wait)
        .op(op::POP, 0)
        .jump(op::JUMP, "message")
        .label("closed")
        .int(flag)
        .native("CheckFlag")
        .op(op::NOT, 0)
        .jump(op::JUMP_IF_NOT, "resume")
        .int(flag)
        .call_native("SetFlag")
        .int(11)
        .int(COIN_VALUE as i32)
        .call_native("SetNewObjRegister")
        .int(12)
        .float(0.1)
        .call_native("SetNewObjRegister")
        .int(13)
        .float(0.05)
        .call_native("SetNewObjRegister")
        .op(op::STRING, ids.money)
        .native("GetThisObject")
        .op(op::NATIVE_VARARGS, new_item)
        .op(op::NOP, 2)
        .op(op::STORE, local(1))
        .op(op::LOAD, local(1))
        .int(0)
        .call_native("method_setDirDeg")
        .op(op::LOAD, local(1))
        .op(op::SELF, 0)
        .native("method_getX")
        .float(2.0)
        .op(op::SELF, 0)
        .native("method_getZ")
        .call_native("method_setXYZ")
        .label("resume")
        .call_native("Resume")
        .label("done")
        .int(0)
        .op(op::RETURN, 0);
    a.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The extracted game files of `rumble-port build`, if there are any:
    /// `RUMBLE_TEST_BUILD` or build/.
    fn build_dir() -> Option<PathBuf> {
        let dir = std::env::var_os("RUMBLE_TEST_BUILD")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build"));
        dir.join("assets/script.arc/CItem_multi.pkc").exists().then_some(dir)
    }

    fn dol_image(dir: &std::path::Path) -> Vec<u8> {
        let dol = std::fs::read(dir.join("main.dol")).unwrap();
        let dol = rumble_formats::dol::Dol::parse(&dol).unwrap();
        let mut image = vec![0; 0x60_0000];
        for s in &dol.sections {
            let at = (s.addr - IMAGE_START) as usize;
            image[at..at + s.data.len()].copy_from_slice(s.data);
        }
        image
    }

    /// The pedestal script is patched in its own space, with the message
    /// and the idle animation's name, and the natives it calls exist.
    #[test]
    fn script_patch_fits() {
        let Some(dir) = build_dir() else { return };
        let natives = natives_in(&dol_image(&dir)).unwrap();
        assert_eq!(natives["NewStageCamera"], 1);
        let file = std::fs::read(dir.join("assets/script.arc/CItem_multi.pkc")).unwrap();
        let new = patch_script(&file, &natives).unwrap();
        assert!(new.len() <= file.len());
        let wide: Vec<u8> = MESSAGE.encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert!(new.windows(wide.len()).any(|w| w == wide));
        assert!(new.windows(IDLE.len()).any(|w| w == IDLE.as_bytes()));
        std::fs::write(std::env::temp_dir().join("CItem_multi.patched.pkc"), &new).unwrap();
    }

    /// The pause menu's Multiplayer branch is found once.
    #[test]
    fn pause_entry_found() {
        let Some(dir) = build_dir() else { return };
        let (at, b) = pause_entry_branch(&dol_image(&dir)).unwrap();
        assert_eq!(b >> 26, 18);
        assert!(at > IMAGE_START);
    }

    /// Ditto's model file has the model under the pedestal's name, the
    /// textures and palette it names and its idle animation.
    #[test]
    fn ditto_model_parts() {
        let Some(dir) = build_dir() else { return };
        let pii = dir.join("assets/pii_1-151_387-492.arc");
        let read = |n: &str| std::fs::read(pii.join(n)).unwrap();
        let file = ditto_model(&read("METAMON.brres"), &read("textures_1-151_387-492.brres"), &read("anim.brres")).unwrap();
        let groups = brres::groups(&file).unwrap();
        let names: Vec<(String, Vec<String>)> =
            groups.iter().map(|(g, fs)| (g.clone(), fs.iter().map(|f| f.0.clone()).collect())).collect();
        assert_eq!(names[0], ("3DModels(NW4R)".into(), vec!["terminal_multiplay".into()]));
        assert!(brres::subfile(&file, "AnmChr(NW4R)", IDLE).is_ok());
        assert!(file.len() <= 53248, "{} bytes", file.len());
        std::fs::write(std::env::temp_dir().join("terminal_multiplay.ditto.brres"), &file).unwrap();
    }
}
