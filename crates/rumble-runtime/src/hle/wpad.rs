//! Wii Remote input without Bluetooth. WPAD and KPAD run as compiled, so
//! their setup, callbacks and state are what the game talks to. Underneath
//! them, WUD (the Wii's Bluetooth device manager) is replaced so the
//! Bluetooth stack never starts and no remote connects through it.
//!
//! Host input comes in at the top instead: `KPADReadEx` (behind `KPADRead`)
//! and `WPADProbe` answer from `mem.input`, so the remotes the host
//! connects are present and their buttons reach the game. Accelerometer and
//! pointer read as a remote at rest pointing off-screen.
//!
//! Commands WPAD sends to a remote are queued for the Bluetooth stack and
//! never answered, so anything that waits for an answer is replaced too:
//! `WPADGetInfo` (the game checks the battery before a stage), and the
//! game data a remote keeps in its own memory (Pokémon saved to the Wii
//! Remote), which lives in a file per remote (`remote_memory`).

use super::{call_guest, Replacement};
use crate::cpu::Cpu;
use crate::input::CHANNELS;
use crate::mem::{Fault, Memory};
use std::path::PathBuf;

/// `WPAD_STATE_SETUP`: the stack is up and remotes can connect.
const WUD_READY: u32 = 3;

pub static REPLACEMENTS: &[Replacement] = &[
    Replacement {
        name: "WUDInit",
        sig: &[
            0x9421fff0, 0x7c0802a6, 0x90010014, 0x93e1000c, 0x3fe00000, 0x3bff0000,
            0x800d0000, 0x2c000000, 0x4182000c, 0x38600000, 0x48000000, 0x3c600000,
            0x38630000, 0x4cc63182, 0x48000001, 0x48000001, 0x886d0000, 0x48000001,
            0x886d0000, 0x48000001, 0x886d0000, 0x48000001, 0x3c600000, 0x38630000,
        ],
        run: wud_init,
    },
    Replacement {
        name: "WUDGetStatus",
        sig: &[
            0x9421fff0, 0x7c0802a6, 0x90010014, 0x93e1000c, 0x48000001, 0x3c800000,
            0x38840000, 0x8be40000, 0x48000001, 0x7fe30774, 0x83e1000c, 0x80010014,
            0x7c0803a6, 0x38210000, 0x4e800020,
        ],
        run: wud_get_status,
    },
    Replacement {
        name: "KPADReadEx",
        sig: &[
            0x9421ff10, 0x7c0802a6, 0x900100f4, 0x39610000, 0x48000001, 0x880d0000,
            0x3d000000, 0x1d230578, 0x7c791b78, 0x2c000000, 0x39080000, 0x7c9d2378,
            0x7cb82b78, 0x7cde3378, 0x7cff3b78, 0x7f484a14, 0x3ae00000, 0x3ac00000,
            0x4082000c, 0x3ae00000, 0x48000000, 0x48000001, 0x2c030003, 0x4182000c,
        ],
        run: kpad_read_ex,
    },
    Replacement {
        name: "WPADProbe",
        sig: &[
            0x9421fff0, 0x7c0802a6, 0x3ca00000, 0x90010014, 0x5460103a, 0x38a50000,
            0x93e1000c, 0x93c10008, 0x7c9e2378, 0x7fe5002e, 0x48000001, 0x2c1e0000,
            0x4182000c, 0x881f0000, 0x901e0000, 0x83df0000, 0x2c1effff, 0x41820028,
            0x881f0000, 0x280000fd, 0x4082000c, 0x3bc00000, 0x48000000, 0x801f0000,
        ],
        run: wpad_probe,
    },
    Replacement {
        name: "WPADGetInfo",
        sig: &[
            0x9421fff0, 0x7c0802a6, 0x3ca00000, 0x90010014, 0x38a50000, 0x93e1000c,
            0x7c7f1b78, 0x48000001, 0x2c03fffe, 0x40820008, 0x48000000, 0x3c600000,
            0x57e0103a, 0x38630000, 0x7c63002e, 0x38630000, 0x48000001, 0x386d0000,
            0x7c63f8ae, 0x7c630774, 0x80010014, 0x83e1000c, 0x7c0803a6, 0x38210000,
            0x4e800020,
        ],
        run: wpad_get_info,
    },
    Replacement {
        name: "WPADWriteGameData",
        sig: &[
            0x9421ffd0, 0x7c0802a6, 0x90010034, 0x39610000, 0x48000001, 0x3d000000,
            0x5460103a, 0x39080000, 0x7c7d1b78, 0x7fc8002e, 0x7c982378, 0x7cbc2b78,
            0x7cdb3378, 0x7cfa3b78, 0x48000001, 0x833e0000, 0x7c7f1b78, 0x801e0000,
            0x2c19ffff, 0x41820318, 0x2c000000, 0x4082000c, 0x3b200000, 0x48000000,
        ],
        run: wpad_write_game_data,
    },
    Replacement {
        name: "WPADReadGameData",
        sig: &[
            0x9421ffd0, 0x7c0802a6, 0x90010034, 0x39610000, 0x48000001, 0x3d000000,
            0x5460103a, 0x39080000, 0x7c7b1b78, 0x7f48002e, 0x7c9c2378, 0x7cbd2b78,
            0x7cde3378, 0x7cff3b78, 0x48000001, 0x833a0000, 0x7c781b78, 0x801a0000,
            0x2c19ffff, 0x418200b4, 0x2c000000, 0x4082000c, 0x3b200000, 0x48000000,
        ],
        run: wpad_read_game_data,
    },
    Replacement {
        name: "WPADGetGameTitleUtf16",
        sig: &[
            0x9421ffe0, 0x7c0802a6, 0x3ca00000, 0x90010024, 0x5460103a, 0x38a50000,
            0x93e1001c, 0x93c10018, 0x93a10014, 0x7c9d2378, 0x7fe5002e, 0x48000001,
            0x801f0000, 0x38a00000, 0x2c000000, 0x41820010, 0x801f0000, 0x2c000000,
            0x40820008, 0x38a00000, 0x7c8500d0, 0x38000000, 0x7c842b78, 0x7c84fe70,
        ],
        run: wpad_get_game_title,
    },
];

/// The real one brings up the Bluetooth stack through `/dev/usb/oh1` and
/// starts WUD's polling alarm. Report success without either, so WPADInit
/// goes on to set up its channels.
fn wud_init(cpu: &mut Cpu, _mem: &mut Memory) -> Result<(), Fault> {
    cpu.gpr[3] = 1;
    Ok(())
}

/// Backs WPADGetStatus.
fn wud_get_status(cpu: &mut Cpu, _mem: &mut Memory) -> Result<(), Fault> {
    cpu.gpr[3] = WUD_READY;
    Ok(())
}

/// `KPADStatus` in this SDK: 0xB0 bytes per sample.
const KPAD_SAMPLE: u32 = 0xb0;
const KPAD_HOLD: u32 = 0x00;
const KPAD_TRIG: u32 = 0x04;
const KPAD_RELEASE: u32 = 0x08;
const KPAD_ACC: u32 = 0x0c;
const KPAD_ACC_VALUE: u32 = 0x18;
const KPAD_HORIZON: u32 = 0x34;
const KPAD_ACC_VERTICAL: u32 = 0x54;
const KPAD_DEV_TYPE: u32 = 0x5c;
const KPAD_WPAD_ERR: u32 = 0x5d;
const KPAD_DATA_FORMAT: u32 = 0x5f;

/// `WPAD_DEV_CORE` / `WPAD_DEV_NOT_FOUND`, and the core+acc+pointer format.
const DEV_CORE: u32 = 0;
const DEV_NOT_FOUND: u32 = 0xfd;
const FMT_CORE_ACC_DPD: u8 = 2;
/// `KPAD_READ_ERR_NO_CONTROLLER`, `WPAD_ERR_NO_CONTROLLER`.
const KPAD_NO_CONTROLLER: i32 = -2;
const WPAD_NO_CONTROLLER: i32 = -1;

fn write_f32(mem: &mut Memory, addr: u32, v: &[f32]) -> Result<(), Fault> {
    for (k, x) in v.iter().enumerate() {
        mem.w32(addr + 4 * k as u32, x.to_bits())?;
    }
    Ok(())
}

/// `KPADReadEx(chan, samples, length, err, copy_on_error)`: one sample per
/// read from the host's buttons; edges are against the previous read.
fn kpad_read_ex(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let (chan, buf, len, err, copy) = (cpu.gpr[3] as usize, cpu.gpr[4], cpu.gpr[5], cpu.gpr[6], cpu.gpr[7]);
    let connected = chan < CHANNELS && mem.input.remotes.connected(chan);
    let (count, code) = if !connected {
        if buf != 0 && copy != 0 {
            mem.w8(buf + KPAD_DEV_TYPE, DEV_NOT_FOUND as u8)?;
            mem.w8(buf + KPAD_WPAD_ERR, WPAD_NO_CONTROLLER as u8)?;
            mem.w8(buf + KPAD_DATA_FORMAT, 0)?;
        }
        (0, KPAD_NO_CONTROLLER)
    } else if buf != 0 && len > 0 {
        let hold = mem.input.remotes.buttons(chan);
        let last = std::mem::replace(&mut mem.input.last_read[chan], hold);
        mem.load(buf, &[0; KPAD_SAMPLE as usize])?;
        mem.w32(buf + KPAD_HOLD, hold)?;
        mem.w32(buf + KPAD_TRIG, hold & !last)?;
        mem.w32(buf + KPAD_RELEASE, last & !hold)?;
        // At rest, face up: 1 g straight down.
        write_f32(mem, buf + KPAD_ACC, &[0.0, -1.0, 0.0])?;
        write_f32(mem, buf + KPAD_ACC_VALUE, &[1.0])?;
        write_f32(mem, buf + KPAD_HORIZON, &[1.0, 0.0])?;
        write_f32(mem, buf + KPAD_ACC_VERTICAL, &[1.0, 0.0])?;
        mem.w8(buf + KPAD_DEV_TYPE, DEV_CORE as u8)?;
        mem.w8(buf + KPAD_DATA_FORMAT, FMT_CORE_ACC_DPD)?;
        (1, 0)
    } else {
        (0, 0)
    };
    if err != 0 {
        mem.w32(err, code as u32)?;
    }
    cpu.gpr[3] = count;
    Ok(())
}

/// `WPAD_ERR_NO_CONTROLLER`.
const WPAD_ERR_NO_CONTROLLER: u32 = -1i32 as u32;

/// `WPADGetInfo(chan, &info)`: the real one asks the remote for a status
/// report and sleeps until it answers, which never happens here. Answer at
/// once for a connected remote: nothing attached, IR and speaker off, full
/// battery, and the channel's player LED.
///
/// `WPADInfo`: BOOLs dpd, speaker, attach, lowBat, nearempty at 0x00-0x10,
/// then u8 battery (0-4), led, protocol, firmware at 0x14-0x17.
fn wpad_get_info(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let (chan, info) = (cpu.gpr[3] as usize, cpu.gpr[4]);
    if chan >= CHANNELS || !mem.input.remotes.connected(chan) {
        cpu.gpr[3] = WPAD_ERR_NO_CONTROLLER;
        return Ok(());
    }
    for off in (0..0x14).step_by(4) {
        mem.w32(info + off, 0)?;
    }
    mem.w8(info + 0x14, 4)?;
    mem.w8(info + 0x15, 1 << chan)?;
    mem.w8(info + 0x16, 0)?;
    mem.w8(info + 0x17, 0)?;
    cpu.gpr[3] = 0;
    Ok(())
}

/// `WPADProbe(chan, &type)`: a core remote on every channel the host has
/// connected.
fn wpad_probe(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let (chan, ty) = (cpu.gpr[3] as usize, cpu.gpr[4]);
    let connected = chan < CHANNELS && mem.input.remotes.connected(chan);
    if ty != 0 {
        mem.w32(ty, if connected { DEV_CORE } else { DEV_NOT_FOUND })?;
    }
    cpu.gpr[3] = if connected { 0 } else { WPAD_NO_CONTROLLER as u32 };
    Ok(())
}

/// A Wii Remote's memory (EEPROM). Games keep their data from 0x2A: a
/// header there and a copy of it at 0x62, then the data itself at 0x9A.
/// The Mii Parade follows at 0xFCA, so a game has up to 0xF30 bytes.
const REMOTE_MEMORY: usize = 0x4000;
const GAME_HEADER: [usize; 2] = [0x2a, 0x62];
const GAME_HEADER_LEN: usize = 0x38;
const GAME_DATA: usize = 0x9a;
const GAME_DATA_END: usize = 0xfca;
/// In the header: when it was written, the title, the game's 4-character
/// code, the language, and a checksum (the bytes before it summed, plus
/// 0x55).
const HEADER_TITLE: usize = 0x08;
const HEADER_GAME_CODE: usize = 0x2a;
const HEADER_LANGUAGE: usize = 0x2e;
const HEADER_CHECKSUM: usize = 0x2f;
/// Where the SDK keeps a pointer to the game code and the language
/// (r13-relative), and the title it writes, in this DOL (as read by the
/// real WPADWriteGameData).
const SDK_GAME_CODE_PTR: u32 = -0x3f7ci32 as u32;
const SDK_LANGUAGE: u32 = -0x3f78i32 as u32;
const SDK_TITLE: u32 = 0x8047_aca8;
const SDK_TITLE_LEN: u32 = 0x22;

/// WPAD's state for each channel (a pointer per channel). It starts with
/// the game data header it read from the remote when it connected,
/// followed by one word per header copy that is set when that copy was
/// bad.
const SDK_CHANNELS: u32 = 0x8047_7510;
const SDK_HEADER_BAD: [u32; 2] = [0x38, 0x3c];

/// `WPAD_ERR_TRANSFER`, `WPAD_ERR_INVALID` (a range past the game's
/// area), `WPAD_ERR_NOPERM` (the remote holds no data for this game).
const WPAD_ERR_TRANSFER: u32 = -3i32 as u32;
const WPAD_ERR_INVALID: u32 = -4i32 as u32;
const WPAD_ERR_NOPERM: u32 = -5i32 as u32;

/// The file standing in for remote `chan`'s memory, kept beside the NAND
/// as the remote is a device of its own.
fn remote_memory(mem: &Memory, chan: usize) -> Option<PathBuf> {
    Some(mem.ios.nand_dir()?.join("wiimote").join(format!("remote{}.bin", chan + 1)))
}

fn load_remote_memory(mem: &Memory, chan: usize) -> Vec<u8> {
    let mut data = remote_memory(mem, chan).and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
    data.resize(REMOTE_MEMORY, 0);
    data
}

/// The game's 4-character code, which the header names.
fn game_code(cpu: &Cpu, mem: &mut Memory) -> Result<[u8; 4], Fault> {
    let at = mem.r32(cpu.gpr[13].wrapping_add(SDK_GAME_CODE_PTR))?;
    Ok(mem.r32(at)?.to_be_bytes())
}

/// A header copy from a remote's memory, if its checksum holds.
fn valid_header(data: &[u8], at: usize) -> Option<&[u8]> {
    let h = &data[at..at + GAME_HEADER_LEN];
    let sum = h[..HEADER_CHECKSUM].iter().fold(0x55u8, |a, &b| a.wrapping_add(b));
    (h[HEADER_CHECKSUM] == sum).then_some(h)
}

/// The bytes `offset..offset + len` of the game's data area, if inside it.
fn game_range(offset: u32, len: u32) -> Option<std::ops::Range<usize>> {
    let start = GAME_DATA + offset as usize;
    let end = start + len as usize;
    (end <= GAME_DATA_END).then_some(start..end)
}

/// Returns `result` from the game data call `name(chan, buf, len, offset,
/// cb)` (arguments still in r3..r7), running `cb(chan, result)` first if
/// it has one. The call goes into the IOS log, and so a session's
/// `ios.log`, like a device request.
fn complete(cpu: &mut Cpu, mem: &mut Memory, name: &str, result: u32) -> Result<(), Fault> {
    let [chan, buf, len, offset, cb] = [cpu.gpr[3], cpu.gpr[4], cpu.gpr[5], cpu.gpr[6], cpu.gpr[7]];
    mem.ios.log.push(format!("{name}({chan}, {buf:#x}, {len:#x}, {offset:#x}) -> {}", result as i32));
    if cb == 0 {
        cpu.gpr[3] = result;
        return Ok(());
    }
    call_guest(cpu, mem, cb, &[chan, result], result)
}

/// `WPADWriteGameData(chan, buf, len, offset, cb)`: the real one sends the
/// header and the data to the remote in 16-byte writes and calls back once
/// the last is acknowledged. Here they go into the remote's file at once.
fn wpad_write_game_data(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let (chan, buf, len, offset) = (cpu.gpr[3], cpu.gpr[4], cpu.gpr[5] & 0xffff, cpu.gpr[6] & 0xffff);
    let c = chan as usize;
    if c >= CHANNELS || !mem.input.remotes.connected(c) {
        return complete(cpu, mem, "WPADWriteGameData", WPAD_ERR_NO_CONTROLLER);
    }
    let Some(range) = game_range(offset, len) else {
        return complete(cpu, mem, "WPADWriteGameData", WPAD_ERR_INVALID);
    };
    let mut header = [0u8; GAME_HEADER_LEN];
    header[..8].copy_from_slice(&mem.now.to_be_bytes());
    let title = mem.read_bytes(SDK_TITLE, SDK_TITLE_LEN)?;
    header[HEADER_TITLE..HEADER_TITLE + title.len()].copy_from_slice(&title);
    header[HEADER_GAME_CODE..HEADER_GAME_CODE + 4].copy_from_slice(&game_code(cpu, mem)?);
    header[HEADER_LANGUAGE] = mem.r8(cpu.gpr[13].wrapping_add(SDK_LANGUAGE))?;
    header[HEADER_CHECKSUM] = header[..HEADER_CHECKSUM].iter().fold(0x55u8, |a, &b| a.wrapping_add(b));
    let mut data = load_remote_memory(mem, c);
    for at in GAME_HEADER {
        data[at..at + GAME_HEADER_LEN].copy_from_slice(&header);
    }
    data[range].copy_from_slice(&mem.read_bytes(buf, len)?);
    let saved = remote_memory(mem, c).is_some_and(|p| {
        p.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::write(&p, &data).is_ok()
    });
    complete(cpu, mem, "WPADWriteGameData", if saved { 0 } else { WPAD_ERR_TRANSFER })
}

/// `WPADReadGameData(chan, buf, len, offset, cb)`: reads back what this
/// game wrote. Like the SDK, it checks the header's game code first and
/// answers `WPAD_ERR_NOPERM` for a remote with another game's data or none.
fn wpad_read_game_data(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let (chan, buf, len, offset) = (cpu.gpr[3], cpu.gpr[4], cpu.gpr[5] & 0xffff, cpu.gpr[6] & 0xffff);
    let c = chan as usize;
    if c >= CHANNELS || !mem.input.remotes.connected(c) {
        return complete(cpu, mem, "WPADReadGameData", WPAD_ERR_NO_CONTROLLER);
    }
    let data = load_remote_memory(mem, c);
    let at = GAME_HEADER[0] + HEADER_GAME_CODE;
    if data[at..at + 4] != game_code(cpu, mem)? {
        return complete(cpu, mem, "WPADReadGameData", WPAD_ERR_NOPERM);
    }
    let Some(range) = game_range(offset, len) else {
        return complete(cpu, mem, "WPADReadGameData", WPAD_ERR_INVALID);
    };
    mem.load(buf, &data[range])?;
    complete(cpu, mem, "WPADReadGameData", 0)
}

/// `WPADGetGameTitleUtf16(chan, &title)`: the title in the game data
/// header WPAD read when the remote connected. The game compares it with
/// its own to find a remote carrying its data, then reads that. Remotes
/// here never connect through WPAD, so the header comes from the remote's
/// file, into WPAD's state as the real read would leave it.
fn wpad_get_game_title(cpu: &mut Cpu, mem: &mut Memory) -> Result<(), Fault> {
    let (chan, out) = (cpu.gpr[3] as usize, cpu.gpr[4]);
    let data = load_remote_memory(mem, chan.min(CHANNELS - 1));
    let header = GAME_HEADER.iter().find_map(|&at| valid_header(&data, at)).filter(|_| chan < CHANNELS);
    let state = match header {
        Some(_) => mem.r32(SDK_CHANNELS + 4 * chan as u32)?,
        None => 0,
    };
    let result = match header {
        Some(h) if state != 0 => {
            mem.load(state, h)?;
            for off in SDK_HEADER_BAD {
                mem.w32(state + off, 0)?;
            }
            mem.w32(out, state + HEADER_TITLE as u32)?;
            0
        }
        // `WPAD_ERR_INVALID`, as for a remote whose header is bad.
        _ => {
            mem.w32(out, 0)?;
            WPAD_ERR_INVALID
        }
    };
    mem.ios.log.push(format!("WPADGetGameTitleUtf16({chan}) -> {}", result as i32));
    cpu.gpr[3] = result;
    Ok(())
}
