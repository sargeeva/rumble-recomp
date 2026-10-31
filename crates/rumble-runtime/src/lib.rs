//! Runtime for the PC port: guest memory, hardware boundary and the
//! interpreter. See `docs/recomp.md`.

pub mod ai;
pub mod ax;
pub mod cpu;
pub mod dsp;
pub mod gx;
pub mod hle;
pub mod input;
pub mod ios;
pub mod ipc;
pub mod mem;
pub mod snd;
pub mod vi;

use cpu::Cpu;
use mem::{Fault, Memory};
use rumble_formats::dol::Dol;

/// Loads a DOL and sets up the low-memory globals the SDK reads at boot,
/// mirroring what the system menu / apploader leaves behind.
pub fn boot(dol: &Dol, title: ios::Title) -> Result<(Cpu, Memory), Fault> {
    let game_code = (title.id as u32).to_be_bytes();
    let mut m = Memory::default();
    m.ios = ios::Ios::new(title);
    // The header's bss range spans .sdata/.sdata2 too (they sit between
    // .bss and .sbss), so clear it before loading sections, not after.
    for a in (dol.bss_addr..dol.bss_addr + dol.bss_size).step_by(4) {
        m.w32(a, 0)?;
    }
    for s in &dol.sections {
        m.load(s.addr, s.data)?;
    }
    m.load(0x8000_0000, &game_code)?;
    // The running title's ID, which OSGetAppGamename returns; games pick
    // their region and locale folders from its last letter.
    m.load(0x8000_3180, &game_code)?;
    m.w32(0x8000_0020, 0x0D15_EA5E)?; // boot magic
    m.w32(0x8000_0028, mem::MEM1_SIZE as u32)?;
    m.w32(0x8000_002C, 0x0000_0023)?; // console type: retail Wii
    m.w32(0x8000_00F8, 243_000_000)?; // bus clock
    m.w32(0x8000_00FC, 729_000_000)?; // CPU clock
    // Memory layout IOS hands the title. OSInit takes the MEM2 arena from
    // 0x3124/0x3128; IOS keeps 0x933E0000-0x93400000 for itself.
    m.w32(0x8000_3100, mem::MEM1_SIZE as u32)?; // MEM1 physical size
    m.w32(0x8000_3104, mem::MEM1_SIZE as u32)?; // MEM1 simulated size
    m.w32(0x8000_3118, mem::MEM2_SIZE as u32)?; // MEM2 physical size
    m.w32(0x8000_311C, mem::MEM2_SIZE as u32)?; // MEM2 simulated size
    m.w32(0x8000_3120, 0x9340_0000)?; // MEM2 end
    m.w32(0x8000_3124, 0x9000_0800)?; // usable MEM2 start
    m.w32(0x8000_3128, 0x933E_0000)?; // usable MEM2 end
    m.w32(0x8000_3130, 0x933E_0000)?; // IOS heap start
    m.w32(0x8000_3134, 0x9340_0000)?; // IOS heap end

    // Effective-address translation on, FP available.
    let mut cpu = Cpu::default();
    cpu.pc = dol.entry;
    cpu.msr = 0x0000_2032;
    Ok((cpu, m))
}
