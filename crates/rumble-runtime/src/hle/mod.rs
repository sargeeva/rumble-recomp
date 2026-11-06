//! High-level replacements for SDK functions whose host behaviour must
//! differ from the Wii's (see `docs/recomp.md`). Each one is found in the
//! DOL by signature, and its first instruction is overwritten with a trap
//! word; when the interpreter reaches it, the Rust replacement runs and
//! returns to the caller.
//!
//! A replacement can also finish by calling a guest function, such as the
//! callback an asynchronous SDK call takes (`call_guest`). The word after
//! each replaced entry is a second trap, `GuestReturn`, which the guest
//! function returns to.

pub mod addons;
mod cx;
pub mod multiplay;
mod nametag;
mod pause;
mod playinfo;
mod title;
mod lyt;
pub mod mtx;
mod wpad;

use crate::cpu::{Cpu, SPR_LR};
use crate::mem::{Fault, Memory};
use rumble_recomp::analysis::Program;
use rumble_recomp::sigs;

/// Gekko leaves primary opcode 1 unassigned. A word with it marks a
/// replaced function's entry; the low 26 bits index `REPLACEMENTS`.
const TRAP: u32 = 1 << 26;

pub struct Replacement {
    pub name: &'static str,
    /// Masked leading instructions, from `rumble-port sig`. Empty for one
    /// that is not found in the DOL.
    sig: &'static [u32],
    /// Runs in place of the function: arguments in r3.., result in r3.
    run: fn(&mut Cpu, &mut Memory) -> Result<(), Fault>,
}

static REPLACEMENTS: &[&[Replacement]] =
    &[&[GUEST_RETURN], wpad::REPLACEMENTS, cx::REPLACEMENTS, mtx::REPLACEMENTS, lyt::REPLACEMENTS, playinfo::REPLACEMENTS, pause::REPLACEMENTS];

/// Placed after each replaced entry, where `call_guest` returns.
const GUEST_RETURN: Replacement = Replacement { name: "GuestReturn", sig: &[], run: guest_return };

/// The frame `call_guest` pushes: back chain, then at +8 the replacement's
/// return address, at +12 its result and at +16 what to do on the way
/// back (`TAG_*`).
const GUEST_FRAME: u32 = 24;

/// Nothing more to do when the guest function returns.
const TAG_NONE: u32 = 0;
/// The guest function drew a HUD layout (`lyt`).
const TAG_HUD: u32 = 1;
/// The guest function drew a pass of a name tag (`nametag`).
const TAG_NAME: u32 = 2;
/// The pause menu's list widget returned (`pause`).
const TAG_PAUSE: u32 = 3;

fn all() -> impl Iterator<Item = &'static Replacement> {
    REPLACEMENTS.iter().flat_map(|r| r.iter())
}

/// Where each replacement went: the function's address, or the number of
/// signature matches when it is not exactly one (then it is not installed).
pub fn install(prog: &Program, mem: &mut Memory) -> Result<Vec<(&'static str, Result<u32, usize>)>, Fault> {
    let mut placed = Vec::new();
    for (index, r) in all().enumerate().filter(|(_, r)| !r.sig.is_empty()) {
        let found = sigs::find(prog, r.sig);
        if let [addr] = found[..] {
            mem.w32(addr, TRAP | index as u32)?;
            mem.w32(addr + 4, TRAP)?; // GUEST_RETURN
            placed.push((r.name, Ok(addr)));
        } else {
            placed.push((r.name, Err(found.len())));
        }
    }
    Ok(placed)
}

/// Installs the replacement `name` (one with no signature) at `addr`.
fn place(mem: &mut Memory, name: &str, addr: u32) -> Result<(), Fault> {
    let index = all().position(|r| r.name == name).expect("replacement exists");
    mem.w32(addr, TRAP | index as u32)?;
    mem.w32(addr + 4, TRAP) // GUEST_RETURN
}

/// Runs the replacement a trap word names. Returns false if `word` is not
/// a trap word.
pub fn call(word: u32, cpu: &mut Cpu, mem: &mut Memory) -> Result<bool, Fault> {
    if word >> 26 != TRAP >> 26 {
        return Ok(false);
    }
    match all().nth((word & 0x03ff_ffff) as usize) {
        Some(r) => (r.run)(cpu, mem).map(|()| true),
        None => Ok(false),
    }
}

/// Ends a replacement by calling the guest function `f` with `args` in
/// r3..; when it returns, the replacement returns `result` to its caller.
/// For callbacks the SDK runs on completion: with the work done at once,
/// the replacement completes it before returning, as the SDK itself does
/// when a call fails.
fn call_guest(cpu: &mut Cpu, mem: &mut Memory, f: u32, args: &[u32], result: u32) -> Result<(), Fault> {
    push_guest_frame(cpu, mem, result, TAG_NONE)?;
    cpu.gpr[3..3 + args.len()].copy_from_slice(args);
    cpu.pc = f;
    Ok(())
}

/// Makes the function called next return through `GuestReturn`, which
/// returns `result` to the replacement's caller after doing what `tag`
/// says.
fn push_guest_frame(cpu: &mut Cpu, mem: &mut Memory, result: u32, tag: u32) -> Result<(), Fault> {
    let sp = cpu.gpr[1] - GUEST_FRAME;
    mem.w32(sp, cpu.gpr[1])?;
    mem.w32(sp + 8, cpu.spr[SPR_LR])?;
    mem.w32(sp + 12, result)?;
    mem.w32(sp + 16, tag)?;
    cpu.gpr[1] = sp;
    // Running, pc is the word after the replacement's trap: GUEST_RETURN.
    cpu.spr[SPR_LR] = cpu.pc;
    Ok(())
}

/// Back from `call_guest`'s function: pop its frame and return its result.
fn guest_return(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let sp = cpu.gpr[1];
    match mem.r32(sp + 16)? {
        TAG_HUD => mem.gx.set_hud_anchor(0),
        TAG_NAME => return nametag::pass_drawn(cpu, mem),
        TAG_PAUSE => return pause::chosen(cpu, mem),
        _ => {}
    }
    cpu.gpr[3] = mem.r32(sp + 12)?;
    cpu.pc = mem.r32(sp + 8)? & !3;
    cpu.gpr[1] = mem.r32(sp)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reaching a trap word runs the replacement and returns to LR.
    #[test]
    fn trap_runs_replacement_and_returns() {
        let index = all().position(|r| r.name == "WUDGetStatus").unwrap() as u32;
        let mut mem = Memory::default();
        mem.w32(0x8000_1000, TRAP | index).unwrap();
        mem.w32(0x8000_2000, 0x6000_0000).unwrap(); // nop
        let mut cpu = Cpu::default();
        cpu.pc = 0x8000_1000;
        cpu.spr[SPR_LR] = 0x8000_2000;
        cpu.run(&mut mem, 1, &mut |_| {});
        assert_eq!(cpu.gpr[3], 3);
        assert_eq!(cpu.pc, 0x8000_2000);
    }

    /// Calls the replacement named `name` with r3..r7, returning r3.
    fn call_named(name: &str, mem: &mut Memory, args: [u32; 5]) -> u32 {
        let index = all().position(|r| r.name == name).unwrap() as u32;
        mem.w32(0x8000_1000, TRAP | index).unwrap();
        mem.w32(0x8000_2000, 0x6000_0000).unwrap();
        let mut cpu = Cpu::default();
        cpu.pc = 0x8000_1000;
        cpu.spr[SPR_LR] = 0x8000_2000;
        cpu.gpr[3..8].copy_from_slice(&args);
        cpu.run(mem, 1, &mut |_| {});
        cpu.gpr[3]
    }

    /// KPADRead answers from the host's buttons, with pressed/released
    /// edges between reads, and reports a missing controller.
    #[test]
    fn kpad_read_follows_host_buttons() {
        use crate::input::button;
        let (buf, err) = (0x8000_4000, 0x8000_5000);
        let mut mem = Memory::default();
        assert_eq!(call_named("KPADReadEx", &mut mem, [0, buf, 1, err, 0]), 0);
        assert_eq!(mem.r32(err).unwrap() as i32, -2);

        mem.input.remotes.connect(0, true);
        mem.input.remotes.set_buttons(0, button::A);
        assert_eq!(call_named("KPADReadEx", &mut mem, [0, buf, 1, err, 0]), 1);
        assert_eq!(mem.r32(err).unwrap(), 0);
        assert_eq!((mem.r32(buf).unwrap(), mem.r32(buf + 4).unwrap()), (button::A, button::A));

        call_named("KPADReadEx", &mut mem, [0, buf, 1, err, 0]);
        assert_eq!(mem.r32(buf + 4).unwrap(), 0, "no new press");

        mem.input.remotes.set_buttons(0, 0);
        call_named("KPADReadEx", &mut mem, [0, buf, 1, err, 0]);
        assert_eq!((mem.r32(buf).unwrap(), mem.r32(buf + 8).unwrap()), (0, button::A));

        let ty = 0x8000_6000;
        assert_eq!(call_named("WPADProbe", &mut mem, [0, ty, 0, 0, 0]), 0);
        assert_eq!(mem.r32(ty).unwrap(), 0);
        assert_eq!(call_named("WPADProbe", &mut mem, [1, ty, 0, 0, 0]) as i32, -1);
        assert_eq!(mem.r32(ty).unwrap(), 0xfd);
    }

    /// Runs `name` with r3..r7 until it is back at its caller, through
    /// any guest callback it calls. Returns r3 and the callback's r4 (its
    /// result argument), or None if it called none.
    fn call_with_callback(name: &str, cpu: &mut Cpu, mem: &mut Memory, args: [u32; 5]) -> (u32, Option<u32>) {
        let index = all().position(|r| r.name == name).unwrap() as u32;
        mem.w32(0x8000_1000, TRAP | index).unwrap();
        mem.w32(0x8000_1004, TRAP).unwrap();
        mem.w32(0x8000_2000, 0x6000_0000).unwrap(); // nop
        // The callback keeps its result in r30: mr r30, r4; blr.
        mem.w32(0x8000_3000, 0x7c9e_2378).unwrap();
        mem.w32(0x8000_3004, 0x4e80_0020).unwrap();
        let sp = 0x8000_8000;
        cpu.gpr[1] = sp;
        cpu.gpr[30] = u32::MAX;
        cpu.pc = 0x8000_1000;
        cpu.spr[SPR_LR] = 0x8000_2000;
        cpu.gpr[3..8].copy_from_slice(&args);
        for _ in 0..8 {
            if cpu.pc == 0x8000_2000 {
                break;
            }
            cpu.run(mem, 1, &mut |_| {});
        }
        assert_eq!((cpu.pc, cpu.gpr[1]), (0x8000_2000, sp), "back at the caller with its stack");
        (cpu.gpr[3], (cpu.gpr[30] != u32::MAX).then_some(cpu.gpr[30]))
    }

    /// Game data written to a remote reads back for the same game, through
    /// the callback, and from a file per remote; another game sees none.
    #[test]
    fn wii_remote_game_data_round_trip() {
        let dir = std::env::temp_dir().join(format!("rumble-wpad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut mem = Memory::default();
        mem.ios = crate::ios::Ios::new(crate::ios::Title {
            id: 0,
            tmd: Vec::new(),
            ticket: Vec::new(),
            contents_dir: dir.join("contents"),
            nand_dir: dir.join("nand"),
        });
        mem.input.remotes.connect(0, true);
        let mut cpu = Cpu::default();
        // The SDK's pointer to the game code, r13-relative.
        cpu.gpr[13] = 0x8001_0000 + 0x3f7c;
        mem.w32(0x8001_0000, 0x8000_0000).unwrap();
        mem.load(0x8000_0000, b"WPSE").unwrap();
        let (buf, out, cb) = (0x8000_4000, 0x8000_5000, 0x8000_3000);
        mem.load(buf, b"pikachu!").unwrap();

        let read = |cpu: &mut Cpu, mem: &mut Memory| call_with_callback("WPADReadGameData", cpu, mem, [0, out, 8, 0x10, cb]);
        assert_eq!(read(&mut cpu, &mut mem), (-5i32 as u32, Some(-5i32 as u32)), "nothing saved yet");
        assert_eq!(call_with_callback("WPADWriteGameData", &mut cpu, &mut mem, [0, buf, 8, 0x10, cb]), (0, Some(0)));
        assert_eq!(read(&mut cpu, &mut mem), (0, Some(0)));
        assert_eq!(mem.read_bytes(out, 8).unwrap(), b"pikachu!");
        let file = std::fs::read(dir.join("nand/wiimote/remote1.bin")).unwrap();
        assert_eq!(&file[0x9a + 0x10..0x9a + 0x18], b"pikachu!");

        mem.load(0x8000_0000, b"RMCE").unwrap();
        assert_eq!(read(&mut cpu, &mut mem).0, -5i32 as u32, "another game");
        assert_eq!(
            call_with_callback("WPADReadGameData", &mut cpu, &mut mem, [1, out, 8, 0, 0]),
            (-1i32 as u32, None),
            "no remote, no callback"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The CX replacement reads its arguments and reports a source that is
    /// not LH-compressed the way the SDK does.
    #[test]
    fn cx_rejects_non_lh_source() {
        let mut mem = Memory::default();
        mem.w32(0x8000_4000, 0x1000_0010).unwrap(); // LZ header, not LH
        let r = call_named("CXSecureUncompressLH", &mut mem, [0x8000_4000, 16, 0x8000_5000, 0x8000_6000, 0]);
        assert_eq!(r as i32, -1);
    }
}