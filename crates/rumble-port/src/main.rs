mod audio;
mod boot;
mod codegen;
mod config;
mod controls;
mod menu;
mod disasm;
mod display;
mod icon;
mod mods;
mod net;
mod video;
mod client;
mod join;
mod profile;
mod remote;
mod render;
mod run;
mod saves;
mod setup;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use rumble_formats::{bps, lh, lz11, u8arc, wad::Wad};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "rumble-port", about = "Pokémon Rumble PC port toolkit")]
struct Cli {
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the base WAD, enabled mods and resolved display settings.
    Info,
    /// Apply enabled mods to the WAD and extract the game filesystem.
    Build {
        /// Only produce the patched WAD (no decryption / extraction).
        #[arg(long)]
        wad_only: bool,
    },
    /// Apply a single BPS patch to a file.
    Patch { patch: PathBuf, input: PathBuf, output: PathBuf },
    /// List mods found in the mods folder.
    Mods,
    /// Build the game into data/<name>/ and copy this exe to <name>.exe in
    /// the current folder, as the setup app's Build button does.
    Setup {
        wad: PathBuf,
        common_key: PathBuf,
        /// Patch in Rumble Weekend (mods/weekend-edition).
        #[arg(long)]
        weekend: bool,
        /// Rumble Weekend's downloaded zip, to unpack into
        /// mods/weekend-edition first (implies --weekend).
        #[arg(long)]
        weekend_zip: Option<PathBuf>,
        /// An extra BPS patch, applied after Rumble Weekend. Repeatable.
        #[arg(long)]
        patch: Vec<PathBuf>,
    },
    /// Disassemble and analyze build/main.dol into build/recomp/.
    Disasm,
    /// Compile build/main.dol's code to Rust blocks in build/recomp/gen/,
    /// built into the next `cargo build`.
    Recomp {
        /// Longest block, in instructions. `boot --exact` only runs blocks
        /// shorter than 12.
        #[arg(long, default_value_t = 64)]
        max_len: u32,
    },
    /// Print the masked signature of the function at an address, for
    /// registering a high-level replacement.
    Sig {
        /// Function address, in hex.
        addr: String,
        /// Number of instructions (default: the whole function, up to 64).
        #[arg(long)]
        len: Option<usize>,
    },
    /// Run build/main.dol in the interpreter and report where it stops.
    Boot {
        #[arg(long, default_value_t = 50_000_000)]
        steps: u64,
        /// Render frames to PNGs in this folder.
        #[arg(long)]
        dump_frames: Option<PathBuf>,
        /// With --dump-frames, save every Nth frame (all are rendered).
        #[arg(long, default_value_t = 30)]
        dump_every: u64,
        /// Connect remote 1 and hold a button for 10 frames from a frame on,
        /// e.g. `--press a@300`. Buttons: a b 1 2 up down left right plus
        /// minus home (remote orientation). Repeatable.
        #[arg(long)]
        press: Vec<boot::Press>,
        /// Don't count function entries (for measuring speed).
        #[arg(long)]
        no_trace: bool,
        /// Build draw commands without rendering (for measuring speed).
        #[arg(long)]
        capture: bool,
        /// Interpret everything, ignoring recompiled blocks.
        #[arg(long)]
        interp: bool,
        /// Run recompiled blocks only where they can't move an interrupt,
        /// so the run matches the interpreter's exactly.
        #[arg(long, conflicts_with = "interp")]
        exact: bool,
        /// Translate the game's code with Cranelift at load time instead
        /// of using the recompiled Rust blocks.
        #[arg(long, conflicts_with_all = ["interp", "exact"])]
        jit: bool,
        /// Sample where the host spends its time and print the busiest
        /// functions (Windows).
        #[arg(long)]
        profile: bool,
        /// Replay buttons recorded by `run` (build/remote/input.log).
        #[arg(long)]
        replay: Option<PathBuf>,
        /// With --dump-frames, render 16:9 with the widescreen hack.
        #[arg(long)]
        widescreen: bool,
        /// Record the sound the game plays to this WAV file.
        #[arg(long)]
        wav: Option<PathBuf>,
        /// Build draws on a thread of their own, as `run` does by default.
        #[arg(long)]
        threaded: bool,
    },
    /// Run the game in a window.
    /// Join an online session from the console and play as the next free
    /// player with this PC's gamepads (without seeing the game yet).
    Join {
        /// The host, `ip:port` (asked for if left out).
        addr: Option<String>,
    },
    /// Open the client app, for joining an online session (what
    /// `Pokemon Rumble Client.exe` opens).
    Client,
    Run {
        /// Scripted presses on remote 1, as for `boot`; merged with the
        /// keyboard and gamepads.
        #[arg(long)]
        press: Vec<boot::Press>,
        /// Interpret everything, ignoring recompiled blocks.
        #[arg(long)]
        interp: bool,
        /// Translate the game's code with Cranelift at load time instead
        /// of using the recompiled Rust blocks.
        #[arg(long, conflicts_with = "interp")]
        jit: bool,
    },
}

fn main() -> Result<()> {
    // Opened without arguments (double-clicked): the setup app or the game,
    // by the exe's name.
    if std::env::args_os().len() <= 1 {
        return launch().inspect_err(|e| {
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("Pokémon Rumble")
                .set_description(format!("{e:#}"))
                .show();
        });
    }
    let cli = Cli::parse();
    let cfg = Config::load_or_create(&cli.config)?;
    match cli.cmd {
        Cmd::Info => info(&cfg),
        Cmd::Build { wad_only } => build(&cfg, wad_only),
        Cmd::Patch { patch, input, output } => {
            let out = bps::apply(&std::fs::read(&patch)?, &std::fs::read(&input)?)?;
            std::fs::write(&output, out)?;
            println!("wrote {}", output.display());
            Ok(())
        }
        Cmd::Boot { steps, dump_frames, dump_every, press, no_trace, capture, interp, exact, jit, profile, replay, widescreen, wav, threaded } => {
            let dump = dump_frames.map(|dir| boot::Dump { dir, every: dump_every.max(1) });
            let exec = boot::Exec::new(interp, exact, jit);
            let replay = match replay {
                Some(path) => remote::Recording::load(&path).with_context(|| format!("reading {}", path.display()))?,
                None => remote::Recording::default(),
            };
            let original_multiplayer = cfg.game.original_pedestal();
            let opts = boot::Options { no_trace, capture, exec, profile, replay, widescreen, wav, threaded, original_multiplayer, addons: cfg.addons.runtime() };
            boot::run(&cfg.paths.build_dir, steps, dump, &press, &opts)
        }
        Cmd::Join { addr } => join::run(&cfg, addr),
        Cmd::Client => client::run(cfg, cli.config.clone()),
        Cmd::Run { press, interp, jit } => run::run(cfg, &cli.config, press, boot::Exec::new(interp, false, jit)),
        Cmd::Disasm => disasm::run(&cfg.paths.build_dir),
        Cmd::Recomp { max_len } => codegen::run(&cfg.paths.build_dir, max_len.max(1)),
        Cmd::Sig { addr, len } => {
            let addr = u32::from_str_radix(addr.trim_start_matches("0x"), 16)?;
            disasm::sig(&cfg.paths.build_dir, addr, len)
        }
        Cmd::Setup { wad, common_key, weekend, weekend_zip, patch } => {
            if let Some(zip) = &weekend_zip {
                let n = setup::install_weekend(&cfg.paths.mods_dir, zip)?;
                println!("unpacked {n} Rumble Weekend patches");
            }
            let weekend = weekend || weekend_zip.is_some();
            let req = setup::Request { wad, key: common_key, weekend, patches: patch, mods_dir: cfg.paths.mods_dir.clone() };
            let name = setup::build(&req, &|f, stage| println!("{:3.0}% {stage}", f * 100.0))?;
            println!("built {name}.exe");
            Ok(())
        }
        Cmd::Mods => {
            let ids = mods::discover(&cfg.paths.mods_dir);
            for m in mods::load(&cfg.paths.mods_dir, &ids)? {
                let on = if cfg.mods.enabled.contains(&m.id) { "x" } else { " " };
                let man = &m.manifest;
                println!("[{on}] {} — {} {} by {}", m.id, man.name, man.version, man.author);
            }
            Ok(())
        }
    }
}

/// Runs `<name>.exe`'s build from `data/<name>/` (see `setup`), or opens
/// the setup app when this is `Pokemon Rumble Setup.exe` or there is no
/// build for the name yet.
fn launch() -> Result<()> {
    let exe = std::env::current_exe()?;
    if let Some(dir) = exe.parent() {
        std::env::set_current_dir(dir)?;
    }
    hide_console();
    let stem = exe.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let config_path = PathBuf::from("config.toml");
    let mut cfg = Config::load_or_create(&config_path)?;
    if stem.to_ascii_lowercase().contains("client") {
        return client::run(cfg, config_path);
    }
    let data = setup::data_dir(&stem);
    if !setup::is_setup(&stem) && data.join("main.dol").is_file() {
        cfg.paths.build_dir = data;
        return run::run(cfg, &config_path, Vec::new(), boot::Exec::default());
    }
    setup::run_app(cfg, config_path)
}

/// Closes the console window Windows opens for a double-clicked exe (one no
/// other program shares), keeping it when started from a terminal.
fn hide_console() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
        let mut ids = [0u32; 2];
        if GetConsoleProcessList(ids.as_mut_ptr(), 2) == 1 {
            FreeConsole();
        }
    }
}

fn info(cfg: &Config) -> Result<()> {
    let data = std::fs::read(&cfg.paths.wad).with_context(|| format!("reading {}", cfg.paths.wad.display()))?;
    let wad = Wad::parse(&data)?;
    println!("WAD: {} ({} bytes, crc32 {:08x})", cfg.paths.wad.display(), data.len(), crc32fast::hash(&data));
    println!("  title {:016x} [{}] v{}", wad.title_id, wad.game_code(), wad.title_version);
    for c in &wad.contents {
        println!("  content {:2} id {:08x} type {:04x} size {}", c.index, c.id, c.kind, c.size);
    }
    println!("Mods (load order): {:?}", cfg.mods.enabled);
    let d = cfg.display.resolve();
    println!("Display: window {}x{}, viewport {:?}", d.window.0, d.window.1, d.viewport);
    println!(
        "  scene aspect {:.3}, projection x-scale {:.3}, HUD {:?}, render target {}x{}",
        d.scene_aspect, d.projection_x_scale, d.hud_viewport(), d.render_target.0, d.render_target.1
    );
    Ok(())
}

fn read_common_key(path: &Path) -> Result<Option<[u8; 16]>> {
    match std::fs::read(path) {
        Ok(k) if k.len() == 16 => Ok(Some(k.try_into().unwrap())),
        Ok(k) => bail!("{} must be exactly 16 bytes (got {})", path.display(), k.len()),
        Err(_) => Ok(None),
    }
}

fn build(cfg: &Config, wad_only: bool) -> Result<()> {
    let out = &cfg.paths.build_dir;
    std::fs::create_dir_all(out)?;

    let mut data = std::fs::read(&cfg.paths.wad).with_context(|| format!("reading {}", cfg.paths.wad.display()))?;
    let enabled = mods::load(&cfg.paths.mods_dir, &cfg.mods.enabled)?;
    for m in &enabled {
        data = m.patch_wad(data)?;
    }
    let wad_path = out.join("game.wad");
    std::fs::write(&wad_path, &data)?;
    println!("wrote {} (crc32 {:08x})", wad_path.display(), crc32fast::hash(&data));
    if wad_only {
        return Ok(());
    }

    let Some(common_key) = read_common_key(&cfg.paths.common_key)? else {
        println!(
            "no common key at {}: skipping decryption. Dump it from your own Wii \
             (e.g. with xyzzy-mod) to extract the game files.",
            cfg.paths.common_key.display()
        );
        return Ok(());
    };
    let wad = Wad::parse(&data)?;
    let title_key = wad.title_key(&common_key);
    let fs_root = out.join("fs");
    if fs_root.exists() {
        std::fs::remove_dir_all(&fs_root)?;
    }
    for rec in &wad.contents {
        let content = wad.decrypt_content(rec, &title_key)?;
        let name = format!("{:08x}.app", rec.id);
        std::fs::create_dir_all(out.join("contents"))?;
        std::fs::write(out.join("contents").join(&name), &content)?;
        if u8arc::is_u8(&content) {
            let files = u8arc::files(&content)?;
            for f in &files {
                let p = fs_root.join(&f.path);
                std::fs::create_dir_all(p.parent().unwrap())?;
                std::fs::write(&p, f.data)?;
            }
            println!("  {name}: U8, {} files", files.len());
        } else if lz11::is_lz11(&content) {
            // The game's main executable ships LZ11-compressed.
            let dol = lz11::decompress(&content)?;
            std::fs::write(out.join("main.dol"), &dol)?;
            println!("  {name}: LZ11 -> main.dol ({} bytes)", dol.len());
        } else {
            println!("  {name}: {} bytes (not U8)", content.len());
        }
    }
    for m in &enabled {
        if let Some(dir) = m.files_dir() {
            let n = mods::overlay(&dir, &fs_root)?;
            println!("  [{}] overlaid {n} loose files", m.id);
        }
    }
    println!("game filesystem at {}", fs_root.display());

    let assets = out.join("assets");
    if assets.exists() {
        std::fs::remove_dir_all(&assets)?;
    }
    let n = unpack_tree(&fs_root, &fs_root, &assets)?;
    println!("unpacked {n} compressed archives to {}", assets.display());
    Ok(())
}

/// Decompresses every `*.cx` file under `dir` and expands U8 archives, so
/// `fs/stage/forest/01.arc.cx` becomes the folder `assets/stage/forest/01.arc/`.
fn unpack_tree(root: &Path, dir: &Path, assets: &Path) -> Result<usize> {
    let mut n = 0;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            n += unpack_tree(root, &path, assets)?;
            continue;
        }
        let Some(rel) = path.strip_prefix(root)?.to_str().and_then(|s| s.strip_suffix(".cx")) else {
            continue;
        };
        let data = lh::decompress(&std::fs::read(&path)?)
            .with_context(|| format!("decompressing {}", path.display()))?;
        write_expanded(&assets.join(rel), &data)?;
        n += 1;
    }
    Ok(n)
}

/// Writes `data` at `dest`, expanding U8 archives (including nested ones)
/// into folders of the same name.
fn write_expanded(dest: &Path, data: &[u8]) -> Result<()> {
    if !u8arc::is_u8(data) {
        std::fs::create_dir_all(dest.parent().unwrap())?;
        std::fs::write(dest, data)?;
        return Ok(());
    }
    for f in u8arc::files(data)? {
        write_expanded(&dest.join(f.path.trim_start_matches("./")), f.data)?;
    }
    Ok(())
}
