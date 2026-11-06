//! Multiplayer's state in the runtime, and what every archive the game
//! unpacks goes through (`CXSecureUncompressLH`): the title, name tags,
//! the pause menu's End Multiplayer, addons and Play Info patch their files
//! there, in the archive in RAM, from the user's own files, so nothing new
//! ships and every backend runs the same code.
//!
//! Also the script (`.pkc`) tools those patches share: the script engine's
//! native functions, its opcodes and an assembler.

use crate::mem::Memory;
use rumble_formats::u8arc;
use std::collections::HashMap;

#[derive(Default)]
pub struct State {
    /// Rumble Weekend is running (`super::playinfo`).
    pub weekend: bool,
    /// The shiny count is answered (`super::playinfo::install`).
    pub shiny_count: bool,
    /// Layouts of the Pokemon selection screen's player panels
    /// (`super::lyt`).
    pub selection_layouts: Vec<u32>,
    /// Health bars' `root` panes and whose bar each is (`super::nametag`).
    pub bar_players: Vec<(u32, u8)>,
    /// The health bar text box being drawn (`super::nametag`).
    pub bar_draw: Option<super::nametag::BarDraw>,
    /// The addons the player picked (`super::addons`).
    pub addons: super::addons::Addons,
    /// The title copyright's length in characters, until the "Recomped"
    /// line is added to it (`super::title`).
    pub title_note: Option<u32>,
    /// End Multiplayer in the pause menu (`super::pause`).
    pub pause: super::pause::State,
}

/// Applies the settings at boot, before code is compiled or translated
/// (blocks whose code differs from RAM are interpreted, so every backend
/// runs the patches).
pub fn configure(mem: &mut Memory) {
    super::pause::configure(mem);
    mem.multiplay.weekend = super::playinfo::is_weekend(mem);
    if mem.multiplay.weekend {
        if let Err(e) = super::playinfo::install(mem) {
            eprintln!("Play Info: no shiny count: {e}");
        }
    }
}

/// Called with each archive the game unpacks, before it reaches RAM.
pub fn unpacked(mem: &mut Memory, out: &mut [u8]) {
    if !u8arc::is_u8(out) {
        return;
    }
    super::title::unpacked(mem, out);
    super::nametag::unpacked(out);
    super::pause::unpacked(mem, out);
    super::addons::unpacked(mem, out);
    if mem.multiplay.weekend {
        super::playinfo::unpacked(mem, out);
    }
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
    pub const STORE: u32 = 0x0a;
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
    pub const SELF: u32 = 0x27;
    pub const INT: u32 = 0x40;
    /// The float is the next word.
    pub const FLOAT: u32 = 0x41;
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
