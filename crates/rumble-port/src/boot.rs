//! `boot` command: run build/main.dol in the interpreter and report how far
//! it gets and which hardware it touched; optionally render frames to PNGs.

use std::io::Write as _;
use crate::render::{self, Renderer};
use anyhow::{Context, Result};
use rumble_formats::dol::Dol;
use rumble_formats::wad::Wad;
use rumble_recomp::analysis::{self, Program};
use rumble_runtime::cpu::blocks::{self, Blocks};
use rumble_runtime::cpu::{Cpu, Stop};
use rumble_runtime::ios::Title;
use rumble_runtime::mem::Memory;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Instructions per slice when frames are taken between slices.
const SLICE: u64 = 1_000_000;
/// Frames a scripted press is held for.
pub const PRESS_FRAMES: u64 = 10;

/// A scripted button press: Wii Remote button bits held from a frame on.
#[derive(Clone)]
pub struct Press {
    pub buttons: u32,
    pub frame: u64,
}

impl std::str::FromStr for Press {
    type Err = String;

    /// `BUTTON@FRAME`, e.g. `a@300` or `2@450`.
    fn from_str(s: &str) -> Result<Self, String> {
        let (name, frame) = s.split_once('@').ok_or("expected BUTTON@FRAME")?;
        let buttons = rumble_runtime::input::button::by_name(name).ok_or_else(|| format!("unknown button {name:?}"))?;
        let frame = frame.parse().map_err(|_| format!("bad frame number {frame:?}"))?;
        Ok(Press { buttons, frame })
    }
}

pub fn read_dol(build_dir: &Path) -> Result<Vec<u8>> {
    std::fs::read(build_dir.join("main.dol")).context("reading build/main.dol (run `build` first)")
}

/// Loads the title from build/ and boots the DOL with the SDK replacements
/// installed, keeping the console's NAND (saves) in `build/<nand>`.
pub fn start(build_dir: &Path, nand: &str, dol: &Dol, prog: &Program) -> Result<(Cpu, Memory)> {
    let wad_data = std::fs::read(build_dir.join("game.wad")).context("reading build/game.wad (run `build` first)")?;
    let wad = Wad::parse(&wad_data)?;
    let title = Title {
        id: wad.title_id,
        tmd: wad.tmd().to_vec(),
        ticket: wad.ticket().to_vec(),
        contents_dir: build_dir.join("contents"),
        nand_dir: build_dir.join(nand),
    };
    let (cpu, mut mem) = rumble_runtime::boot(dol, title).map_err(|f| anyhow::anyhow!("{f:?}"))?;
    let placed = rumble_runtime::hle::install(prog, &mut mem).map_err(|f| anyhow::anyhow!("{f:?}"))?;
    let missing: Vec<String> = placed
        .iter()
        .filter_map(|(name, at)| at.as_ref().err().map(|n| format!("{name} ({n} matches)")))
        .collect();
    println!("replaced {} SDK functions", placed.len() - missing.len());
    if !missing.is_empty() {
        println!("  not found: {}", missing.join(", "));
    }
    Ok((cpu, mem))
}

/// Buttons the scripted presses hold at `frame`.
pub fn scripted_buttons(presses: &[Press], frame: u64) -> u32 {
    presses
        .iter()
        .filter(|p| (p.frame..p.frame + PRESS_FRAMES).contains(&frame))
        .fold(0, |acc, p| acc | p.buttons)
}

/// Lists the OS threads (the active-thread queue at 0x800000DC, linked
/// through OSThread+0x2FC) with their state and where each is suspended,
/// for finding what a hung game waits on.
fn print_threads(mem: &mut Memory, prog: &Program) {
    let current = mem.r32(0x8000_00e4).unwrap_or(0);
    let mut t = mem.r32(0x8000_00dc).unwrap_or(0);
    let name = |pc: u32| prog.function_at(pc).map_or(format!("{pc:08x}"), |f| format!("fn_{:08x}+0x{:x}", f.start, pc - f.start));
    println!("threads:");
    for _ in 0..64 {
        if t & 0xfe00_0000 != 0x8000_0000 {
            break;
        }
        let r = |m: &mut Memory, off: u32| m.r32(t + off).unwrap_or(0);
        let state = mem.r16(t + 0x2c8).unwrap_or(0);
        let state_name = match state {
            1 => "ready",
            2 => "running",
            4 => "waiting",
            8 => "moribund",
            _ => "?",
        };
        let (srr0, lr, prio, queue) = (r(mem, 0x198), r(mem, 0x84), r(mem, 0x2d0), r(mem, 0x2dc));
        let mark = if t == current { " (current)" } else { "" };
        println!(
            "  {t:08x}{mark}: {state_name}, priority {prio}, at {} (lr {}), waits on queue {queue:08x}",
            name(srr0),
            name(lr)
        );
        // Callers: saved r1 is the stack; each frame holds the back chain
        // and, in the caller's frame, the saved LR.
        let mut sp = r(mem, 0x4);
        let mut calls = Vec::new();
        for _ in 0..8 {
            let Ok(back) = mem.r32(sp) else { break };
            if back & 0xfe00_0000 != 0x8000_0000 || back <= sp {
                break;
            }
            calls.push(name(mem.r32(back + 4).unwrap_or(0)));
            sp = back;
        }
        println!("      called from {}", calls.join(" <- "));
        t = r(mem, 0x2fc);
    }
}

/// Writes interleaved 16-bit stereo samples as a WAV file.
fn write_wav(path: &Path, samples: &[i16], rate: u32) -> Result<()> {
    let data = samples.len() as u32 * 2;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&2u16.to_le_bytes()); // stereo
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))
}

/// Where to write rendered frames, and how often.
pub struct Dump {
    pub dir: PathBuf,
    pub every: u64,
}

/// How the game's code runs.
#[derive(Clone, Copy, Default, PartialEq)]
pub enum Exec {
    /// Recompiled blocks where there are any, the interpreter elsewhere.
    #[default]
    Blocks,
    /// Blocks only where they can't move an interrupt (`Blocks::exact`).
    Exact,
    /// The game's code translated with Cranelift at load time
    /// (`rumble-jit`), the interpreter elsewhere.
    Jit,
    Interp,
}

impl Exec {
    pub fn new(interp: bool, exact: bool, jit: bool) -> Self {
        if interp { Exec::Interp } else if jit { Exec::Jit } else if exact { Exec::Exact } else { Exec::Blocks }
    }
}

/// Installs the compiled blocks that match RAM, unless interpreting.
/// Returns each block's (or translated function's) host address and guest
/// start, for the profiler.
pub fn install_blocks(cpu: &mut Cpu, mem: &mut Memory, exec: Exec, prog: &Program) -> Vec<(u64, u32)> {
    // A build to distribute has no recompiled blocks (they are game code):
    // it translates at load time by default.
    #[cfg(feature = "jit")]
    let exec = if exec == Exec::Blocks && rumble_blocks::BLOCKS.is_empty() { Exec::Jit } else { exec };
    #[cfg(not(feature = "jit"))]
    if exec == Exec::Jit {
        let _ = prog;
        println!("built without the `jit` feature, interpreting");
        return Vec::new();
    }
    #[cfg(feature = "jit")]
    if exec == Exec::Jit {
        match rumble_jit::translate(prog, mem, 64) {
            Ok(t) => {
                let s = &t.stats;
                println!(
                    "translated {} functions, {} blocks, {} instructions ({:.1}% as IR) to {:.1} MB in {:.1}s ({} blocks differ from RAM and are interpreted)",
                    s.functions,
                    s.blocks,
                    s.insns,
                    s.native as f64 * 100.0 / s.insns.max(1) as f64,
                    s.code_bytes as f64 / 1e6,
                    s.seconds,
                    s.skipped
                );
                cpu.blocks = Some(Box::new(t.blocks));
                return t.funcs;
            }
            Err(e) => {
                println!("translation failed, interpreting: {e}");
                return Vec::new();
            }
        }
    }
    if exec == Exec::Interp || rumble_blocks::BLOCKS.is_empty() {
        println!("interpreting (no recompiled blocks{})", if exec == Exec::Interp { " used" } else { " built in" });
        return Vec::new();
    }
    let (mut blocks, skipped) = Blocks::new(rumble_blocks::BLOCKS, mem);
    blocks.exact = exec == Exec::Exact;
    println!(
        "{} recompiled blocks{} ({skipped} differ from RAM and are interpreted)",
        blocks.len(),
        if blocks.exact { ", exact mode" } else { "" }
    );
    cpu.blocks = Some(Box::new(blocks));
    rumble_blocks::BLOCKS.iter().map(|b| (b.run as usize as u64, b.start)).collect()
}

/// Measurement switches for `boot`.
#[derive(Default)]
pub struct Options {
    /// Skip counting function entries (they cost a search per branch).
    pub no_trace: bool,
    /// Build draw commands even when no frames are rendered.
    pub capture: bool,
    pub exec: Exec,
    /// Sample the host side (`profile.rs`) while running.
    pub profile: bool,
    /// A session recorded by `run` (`remote::Recorder`), replayed.
    pub replay: crate::remote::Recording,
    /// Render dumped frames 16:9 with the widescreen hack.
    pub widescreen: bool,
    /// Write the sound the game plays here, as a WAV file.
    pub wav: Option<PathBuf>,
    /// Build draws on a thread of their own, as `run` does by default.
    pub threaded: bool,
    /// Keep the original multiplayer pedestal (`[game]
    /// original_multiplayer`).
    pub original_multiplayer: bool,
    /// The addons picked in setup (`[addons]`).
    pub addons: rumble_runtime::hle::addons::Addons,
}

pub fn run(build_dir: &Path, steps: u64, dump: Option<Dump>, presses: &[Press], opts: &Options) -> Result<()> {
    let data = read_dol(build_dir)?;
    let dol = Dol::parse(&data)?;
    let prog = analysis::analyze(&dol);
    // Headless runs (tests, replays) keep their own NAND so they never touch
    // the player's saves.
    // `RUMBLE_NAND` names another folder in build/, for two runs at once.
    let nand = std::env::var("RUMBLE_NAND").unwrap_or_else(|_| "nand-boot".into());
    let (mut cpu, mut mem) = start(build_dir, &nand, &dol, &prog)?;
    rumble_runtime::hle::multiplay::configure(&mut mem, opts.original_multiplayer);
    rumble_runtime::hle::addons::configure(&mut mem, opts.addons);
    let block_addrs = install_blocks(&mut cpu, &mut mem, opts.exec, &prog);
    // Functions whose code differs from the DOL from the start (replaced
    // SDK functions), to tell them from code the game changes as it runs.
    let plan = blocks::plan(&prog, 64);
    let spans = || plan.iter().flat_map(|f| f.blocks.iter().map(move |b| (b.start, b.body.len() as u32, b.hash(), f.start)));
    let patched = blocks::differing_functions(&mut mem, spans());
    if !presses.is_empty() || !opts.replay.is_empty() {
        mem.input.remotes.connect(0, true);
    }

    let mut renderer = match &dump {
        Some(d) => {
            std::fs::create_dir_all(&d.dir)?;
            mem.gx.capture = true;
            let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
            let (_, device, queue) = pollster::block_on(render::request_device(&instance, None))?;
            let mut r = Renderer::new(device, queue, 1);
            if opts.widescreen {
                r.set_widescreen((16.0 / 9.0) / crate::display::GAME_ASPECT);
                mem.gx.widescreen = r.widescreen();
            }
            Some(r)
        }
        None => None,
    };
    let mut frame_no = 0u64;
    let mut sound: Vec<i16> = Vec::new();
    let mut drift_reported = false;
    let mut players = rumble_runtime::snd::PlayerVolumes::default();
    let (mut last_xfb, mut last_frame_at, mut frame_times) = (0u64, std::time::Instant::now(), Vec::new());
    let mut saved = 0u64;

    // Count function entries so we can show where time went.
    let mut entered: HashMap<u32, u64> = HashMap::new();
    let mut recent: Vec<u32> = Vec::new();
    if opts.capture {
        mem.gx.capture = true;
    }
    if opts.threaded && mem.gx.capture {
        mem.start_gx_worker();
    }
    let mut trace = |pc: u32| {
        if opts.no_trace {
            return;
        }
        if prog.functions.binary_search_by_key(&pc, |f| f.start).is_ok() {
            *entered.entry(pc).or_default() += 1;
            if recent.len() == 16 {
                recent.remove(0);
            }
            recent.push(pc);
        }
    };
    // With `RUMBLE_PROFILE_FRAMES=FROM-TO`, only those frames are profiled.
    let window = std::env::var("RUMBLE_PROFILE_FRAMES").ok().and_then(|v| {
        let (a, b) = v.split_once('-')?;
        Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?))
    });
    let mut profiler = (opts.profile && window.is_none()).then(crate::profile::start);
    let mut speed_log = std::env::var_os("RUMBLE_SPEED_LOG")
        .map(|p| std::io::BufWriter::new(std::fs::File::create(p).expect("creating RUMBLE_SPEED_LOG")));
    let mut produced = 0u64;
    let t = std::time::Instant::now();
    let mut n = 0;
    let stop = loop {
        let sliced = renderer.is_some() || !presses.is_empty() || !opts.replay.is_empty();
        let slice = if sliced { SLICE.min(steps - n) } else { steps - n };
        let frame = mem.gx.stats.xfb_copies;
        if let Some((from, to)) = window.filter(|_| opts.profile) {
            if frame >= from && frame < to && profiler.is_none() {
                profiler = Some(crate::profile::start());
            } else if frame >= to {
                if let Some(p) = profiler.take() {
                    println!("profile of frames {from}-{to}:");
                    p.report(&block_addrs);
                }
            }
        }
        let replayed = opts.replay.buttons(frame, n);
        // The first time the replay is in another frame than the session
        // was at the same instruction, it has drifted: say where.
        if let Some((at, f)) = opts.replay.frame_at(n) {
            if !drift_reported && at + SLICE > n && f != frame {
                println!("replay drifted by instruction {at}: session in frame {f}, replay in frame {frame}");
                drift_reported = true;
            }
        }
        mem.input.remotes.set_scripted(0, scripted_buttons(presses, frame) | replayed);
        // As `run` does before every slice.
        mem.input.remotes.latch();
        if let Some(s) = opts.replay.settings(n) {
            // Dumped frames are drawn as the session's were.
            if let Some(r) = renderer.as_mut().filter(|r| r.widescreen() != s.widescreen) {
                r.set_widescreen(s.widescreen);
            }
            mem.gx.widescreen = s.widescreen;
            players.apply(&mut mem, s.volumes);
        }
        let (stop, ran) = cpu.run(&mut mem, slice, &mut trace);
        n += ran;
        // Host time per emulated frame, to find stalls.
        let xfb = mem.gx.stats.xfb_copies;
        if xfb != last_xfb {
            let now = std::time::Instant::now();
            frame_times.push((last_xfb, now - last_frame_at));
            (last_xfb, last_frame_at) = (xfb, now);
        }
        // With `RUMBLE_SPEED_LOG=FILE`: host time against the sound made,
        // to find where the emulator falls behind the audio.
        if let Some(log) = speed_log.as_mut() {
            produced += mem.audio.len() as u64 / 2;
            let _ = writeln!(log, "{} {frame} {produced} {}", t.elapsed().as_micros(), mem.audio_rate);
        }
        if opts.wav.is_some() {
            sound.append(&mut mem.audio);
        } else if speed_log.is_some() {
            mem.audio.clear();
        }
        if let (Some(r), Some(d)) = (renderer.as_mut(), &dump) {
            // Every frame is rendered, as copies in one are drawn from in
            // later ones; every Nth is saved.
            for frame in mem.gx.take_frames() {
                r.render_frame(&frame);
                if frame_no % d.every == 0 {
                    r.save_png(&d.dir.join(format!("frame_{frame_no:05}.png")))?;
                    saved += 1;
                }
                frame_no += 1;
            }
        }
        if stop != Stop::StepLimit || n >= steps {
            break stop;
        }
    };
    let secs = t.elapsed().as_secs_f64();
    frame_times.sort_by(|a, b| b.1.cmp(&a.1));
    let slowest: Vec<String> =
        frame_times.iter().take(8).map(|(f, d)| format!("{f} ({:.0} ms)", d.as_secs_f64() * 1000.0)).collect();
    println!("slowest frames: {}", slowest.join(", "));
    if let Some(path) = &opts.wav {
        write_wav(path, &sound, mem.audio_rate)?;
        println!("sound: {:.1} s to {}", sound.len() as f64 / 2.0 / mem.audio_rate as f64, path.display());
    }
    if let Some(p) = profiler {
        p.report(&block_addrs);
    }

    println!("ran {n} instructions in {secs:.2}s ({:.1} MIPS)", n as f64 / secs / 1e6);
    println!("stopped: {stop:?}");
    let pc = match stop {
        Stop::Unimplemented { pc, .. } | Stop::Fault { pc, .. } => pc,
        Stop::StepLimit => cpu.pc,
    };
    if let Some(f) = prog.function_at(pc) {
        println!("  in fn_{:08x} (+0x{:x}); insn: {}", f.start, pc - f.start, prog.insn(pc).map_or("?".into(), |i| i.to_string()));
    }
    println!("  lr 0x{:08x}  r1 0x{:08x}  r3 0x{:08x}", cpu.spr[8], cpu.gpr[1], cpu.gpr[3]);
    println!("functions entered: {} distinct", entered.len());
    let mut hot: Vec<_> = entered.into_iter().collect();
    hot.sort_by(|a, b| b.1.cmp(&a.1));
    for (f, c) in hot.iter().take(8) {
        println!("  fn_{f:08x}  x{c}");
    }
    println!("recent calls: {}", recent.iter().map(|a| format!("{a:08x}")).collect::<Vec<_>>().join(" "));
    println!("VI interrupts: {} (timebase {})", mem.vi.interrupts, cpu.tb);
    // Compiled code for these would be stale: they need invalidating.
    let mut changed: Vec<u32> = blocks::differing_functions(&mut mem, spans()).difference(&patched).copied().collect();
    changed.sort();
    println!(
        "functions whose code changed while running: {}{}",
        changed.len(),
        changed.iter().take(8).map(|f| format!(" fn_{f:08x}")).collect::<String>()
    );
    print_threads(&mut mem, &prog);
    println!("audio: {} DMA buffers, {} AX frames; first task mail {:08x?}", mem.dsp.ai_buffers, mem.dsp.frames, &mem.dsp.task_mail[..mem.dsp.task_mail.len().min(6)]);
    let gx = &mem.gx.stats;
    println!(
        "gx: {} bursts, {} commands, {} draws ({} vertices), {} display lists, {} draw-done, {} tokens, {} XFB copies",
        gx.bursts, gx.commands, gx.draws, gx.vertices, gx.display_lists, gx.draw_done, gx.tokens, gx.xfb_copies
    );
    if mem.gx.capture {
        println!("  draw building took {:.2}s", gx.draw_building.as_secs_f64());
    }
    if let Some((op, after)) = gx.first_bad {
        println!("  {} bytes out of step; first 0x{op:02x} after {after} commands", gx.bad_bytes);
    }
    if let Some(d) = &dump {
        println!(
            "  rendered {saved} of {frame_no} frames to {} ({} textures undecodable, {} line/point draws skipped)",
            d.dir.display(),
            gx.bad_textures,
            gx.skipped_lines_points
        );
    }
    println!("IOS requests: {}", mem.ios.log.len());
    let log = &mem.ios.log;
    // Wii Remote memory calls are rare and easily lost in the middle.
    for line in log.iter().filter(|l| l.starts_with("WPAD")) {
        println!("  remote: {line}");
    }
    let head = if log.len() > 80 { 40 } else { log.len() };
    for line in &log[..head] {
        println!("  {line}");
    }
    if log.len() > 80 {
        println!("  ... {} more ...", log.len() - 80);
        for line in &log[log.len() - 40..] {
            println!("  {line}");
        }
    }
    println!("hardware registers touched: {}", mem.mmio_log.len());
    for ((addr, w), c) in mem.mmio_log.iter().take(24) {
        println!("  {} 0x{addr:08x}  x{c}", if *w { "W" } else { "R" });
    }
    Ok(())
}
