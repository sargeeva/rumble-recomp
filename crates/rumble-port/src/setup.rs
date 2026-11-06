//! The setup app, for people who download the game rather than build it
//! from source. One exe does both jobs, told apart by its name:
//!
//! - `Pokemon Rumble Setup.exe` opens a window that takes the user's own
//!   WAD and Wii common key, a Rumble Weekend checkbox and any extra BPS
//!   patches. It builds the game into `data/<name>/` and copies itself to
//!   `<name>.exe` next to it, `name` being "Pokemon Rumble" or
//!   "Pokemon Rumble Weekend".
//! - `<name>.exe` plays the build in `data/<name>/`.
//! - `Pokemon Rumble Client.exe`, also made by each build, joins someone
//!   else's online game (`client`): it shows the host's picture, so it
//!   needs no game files of its own, and one serves both builds.
//!
//! Rebuilding replaces a build's game files and keeps its save
//! (`data/<name>/nand`). The two builds keep separate saves. Each built exe's
//! icon is the title logo from its own game files.
//!
//! Rumble Weekend's patches don't come with the app: the user downloads the
//! zip from its page (the button opens it) and chooses it, and setup keeps
//! its `.bps` files in `mods/weekend-edition/`.

use crate::config::Config;
use crate::icon::{self, Image};
use crate::mods;
use anyhow::{bail, Context, Result};
use rumble_formats::{bps, lh, lz11, u8arc, wad::Wad};
use rumble_runtime::gx::texture::{self, TextureKey};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The mod folder Rumble Weekend is kept in.
pub const WEEKEND: &str = "weekend-edition";

/// Where Rumble Weekend is downloaded from. Its files are only given out
/// through this page, so the user downloads the zip and chooses it.
pub const WEEKEND_PAGE: &str = "https://projectpokemon.org/home/files/file/4256-pokemon-rumble-weekend-edition/";

/// Puts Rumble Weekend's patches from the zip the user downloaded (or a
/// `.bps` from it) in `mods_dir/weekend-edition/`, with a `mod.toml`
/// listing them. Returns how many patches it found.
pub fn install_weekend(mods_dir: &Path, file: &Path) -> Result<usize> {
    let data = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    let file_name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let is_bps = |n: &str| n.to_ascii_lowercase().ends_with(".bps");
    let mut patches = Vec::new();
    if is_bps(&file_name(file)) {
        patches.push((file_name(file), data));
    } else {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(data)).context("this isn't a zip file")?;
        for k in 0..zip.len() {
            let mut entry = zip.by_index(k)?;
            // Only the file's own name: nothing outside the mod folder.
            let Some(name) = entry.enclosed_name().map(|p| file_name(&p)) else { continue };
            if is_bps(&name) {
                let mut patch = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut patch)?;
                patches.push((name, patch));
            }
        }
    }
    if patches.is_empty() {
        bail!("there's no .bps patch in {}", file_name(file));
    }
    for (name, patch) in &patches {
        bps::info(patch).with_context(|| format!("{name} isn't a BPS patch"))?;
    }

    let dir = mods_dir.join(WEEKEND);
    std::fs::create_dir_all(&dir)?;
    for (name, patch) in &patches {
        std::fs::write(dir.join(name), patch)?;
    }
    let list: Vec<String> = patches.iter().map(|(n, _)| toml::Value::String(n.clone()).to_string()).collect();
    std::fs::write(
        dir.join("mod.toml"),
        format!(
            "name = \"Pokémon Rumble: Weekend Edition\"\nauthor = \"WindyPrairie\"\n# Variants for different dumps; the one matching the WAD is used.\nwad_patches = [{}]\n",
            list.join(", ")
        ),
    )?;
    Ok(patches.len())
}

/// The exe name (without `.exe`) for a build.
pub fn build_name(weekend: bool) -> &'static str {
    if weekend { "Pokemon Rumble Weekend" } else { "Pokemon Rumble" }
}

/// Where the build named `name` keeps its game files and save, relative to
/// the exe's folder.
pub fn data_dir(name: &str) -> PathBuf {
    Path::new("data").join(name)
}

/// The client app's exe name (without `.exe`); `main` opens the client for
/// an exe whose name has "client" in it.
pub const CLIENT_NAME: &str = "Pokemon Rumble Client";

/// Whether `stem` (an exe's file name without `.exe`) is the setup app.
pub fn is_setup(stem: &str) -> bool {
    stem.to_ascii_lowercase().contains("setup")
}

/// What to build.
#[derive(Clone)]
pub struct Request {
    pub wad: PathBuf,
    pub key: PathBuf,
    pub weekend: bool,
    /// Extra BPS patches, applied in order after Rumble Weekend.
    pub patches: Vec<PathBuf>,
    pub mods_dir: PathBuf,
}

/// Builds the game for `req` in the current folder, reporting progress
/// (0-1, and what it is doing) as it goes. Returns the build's name.
pub fn build(req: &Request, progress: &dyn Fn(f32, &str)) -> Result<&'static str> {
    let name = build_name(req.weekend);

    progress(0.0, "Reading the WAD");
    let mut data = std::fs::read(&req.wad).with_context(|| format!("reading {}", req.wad.display()))?;
    let key: [u8; 16] = std::fs::read(&req.key)
        .with_context(|| format!("reading {}", req.key.display()))?
        .try_into()
        .map_err(|_| anyhow::anyhow!("the common key must be a 16-byte file (the Wii common key dumped from your console)"))?;
    {
        let wad = Wad::parse(&data).context("this isn't a WAD file")?;
        let code = wad.game_code();
        if code != "WPSE" {
            bail!("this WAD is {code}; only Pokémon Rumble (USA, WPSE) is supported");
        }
        // The first content is small: decrypting it checks the key before
        // the long steps.
        let first = wad.contents.iter().min_by_key(|c| c.size).context("the WAD has no contents")?;
        wad.decrypt_content(first, &wad.title_key(&key))
            .map_err(|_| anyhow::anyhow!("the common key doesn't decrypt this WAD; it must be the Wii common key from your own console"))?;
    }

    let steps = req.weekend as usize + req.patches.len();
    let mut step = 0;
    let mut patch_progress = |what: &str| {
        progress(0.1 + 0.4 * step as f32 / steps.max(1) as f32, what);
        step += 1;
    };
    if req.weekend {
        let m = mods::load(&req.mods_dir, &[WEEKEND.to_string()])
            .context("Rumble Weekend isn't included in this download (mods/weekend-edition)")?
            .pop()
            .unwrap();
        patch_progress(&format!("Applying {} {}", m.manifest.name, m.manifest.version));
        data = m.patch_wad(data)?;
    }
    for path in &req.patches {
        let file = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        patch_progress(&format!("Applying {file}"));
        let patch = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let info = bps::info(&patch).with_context(|| format!("{file} isn't a BPS patch"))?;
        if !bps::matches_source(&info, &data) {
            bail!(
                "{file} was made for a different WAD{}",
                if steps > 1 { " (or for a different order or set of the patches above)" } else { "" }
            );
        }
        data = bps::apply(&patch, &data).with_context(|| format!("applying {file}"))?;
    }

    // Built next to the old one, then swapped in, so a failed build leaves
    // the previous one (and its save) as it was.
    let dir = data_dir(name);
    let new = dir.with_extension("new");
    if new.exists() {
        std::fs::remove_dir_all(&new)?;
    }
    std::fs::create_dir_all(new.join("contents"))?;
    let wad = Wad::parse(&data).context("the patched WAD is damaged")?;
    if wad.game_code() != "WPSE" {
        bail!("the patches changed the game's title to {}", wad.game_code());
    }
    let title_key = wad.title_key(&key);
    let mut dol = false;
    let mut logo = None;
    for (k, rec) in wad.contents.iter().enumerate() {
        progress(0.5 + 0.45 * k as f32 / wad.contents.len() as f32, &format!("Decrypting content {} of {}", k + 1, wad.contents.len()));
        let content = wad.decrypt_content(rec, &title_key).context("decrypting the patched WAD")?;
        if lz11::is_lz11(&content) {
            // The game's main executable ships LZ11-compressed.
            std::fs::write(new.join("main.dol"), lz11::decompress(&content)?)?;
            dol = true;
        } else if logo.is_none() && u8arc::is_u8(&content) {
            logo = title_logo(&content)?;
        }
        std::fs::write(new.join("contents").join(format!("{:08x}.app", rec.id)), &content)?;
    }
    if !dol {
        bail!("the WAD has no game executable");
    }
    std::fs::write(new.join("game.wad"), &data)?;

    progress(0.95, "Installing");
    let old = dir.with_extension("old");
    if old.exists() {
        std::fs::remove_dir_all(&old)?;
    }
    if dir.exists() {
        std::fs::rename(&dir, &old).context("moving the previous build aside (is the game still open?)")?;
    }
    std::fs::rename(&new, &dir)?;
    if old.join("nand").exists() {
        std::fs::rename(old.join("nand"), dir.join("nand")).context("keeping the save")?;
    }
    if old.exists() {
        std::fs::remove_dir_all(&old)?;
    }
    let exe = std::env::current_exe()?;
    for (stem, what) in [(name, "the game"), (CLIENT_NAME, "the client")] {
        let target = PathBuf::from(format!("{stem}.exe"));
        if exe.canonicalize()? == target.canonicalize().unwrap_or_default() {
            continue;
        }
        std::fs::copy(&exe, &target).with_context(|| format!("writing {} (close {what} if it is open)", target.display()))?;
        // Without a logo (a patch moved it), the exe keeps setup's icon.
        if let Some(logo) = &logo {
            set_icon(&target, &logo.trimmed().icon_images()).with_context(|| format!("setting {}'s icon", target.display()))?;
        }
    }
    progress(1.0, "Done");
    Ok(name)
}

/// The title screen's logo, if `content` is the archive holding it: the
/// colour layer with its mask as alpha.
fn title_logo(content: &[u8]) -> Result<Option<Image>> {
    let Some(arc) = u8arc::files(content)?.into_iter().find(|f| f.path.ends_with("en_US/title2d.arc.cx")) else {
        return Ok(None);
    };
    let arc = lh::decompress(arc.data).context("decompressing the title screen")?;
    let files = u8arc::files(&arc)?;
    let tpl = |name: &str| -> Result<Image> {
        let f = files.iter().find(|f| f.path.ends_with(name)).with_context(|| format!("the title screen has no {name}"))?;
        decode_tpl(f.data).with_context(|| format!("decoding {name}"))
    };
    let (mut fore, mask) = (tpl("timg/logo_fore.tpl")?, tpl("timg/logo_mask.tpl")?);
    if (fore.width, fore.height) != (mask.width, mask.height) {
        bail!("the title logo's mask is a different size");
    }
    for (px, m) in fore.rgba.chunks_exact_mut(4).zip(mask.rgba.chunks_exact(4)) {
        px[3] = (px[3] as u16 * m[0] as u16 / 255) as u8;
    }
    Ok(Some(fore))
}

/// The first image in a TPL file.
fn decode_tpl(d: &[u8]) -> Result<Image> {
    let u32_at = |o: usize| d.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).context("TPL cut short");
    let table = u32_at(8)? as usize;
    let header = u32_at(table)? as usize;
    let size = u32_at(header)?;
    let (height, width) = (size >> 16, size & 0xffff);
    let key = TextureKey { addr: 0, width, height, format: u32_at(header + 4)?, tlut_offset: 0, tlut_format: 0 };
    let data = d.get(u32_at(header + 8)? as usize..).context("TPL cut short")?;
    let rgba = texture::decode(data, &key, &[]).context("unsupported texture format")?;
    Ok(Image { width, height, rgba })
}

/// Opens `url` in the default browser.
fn open_page(url: &str) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let (verb, url) = (wide("open"), wide(url));
        // SAFETY: both strings are NUL-terminated and outlive the call.
        unsafe {
            ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), url.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL);
        }
    }
    #[cfg(not(windows))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// Replaces `exe`'s icon (see `icon`).
#[cfg(windows)]
fn set_icon(exe: &Path, images: &[Vec<u8>]) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{BeginUpdateResourceW, EndUpdateResourceW, UpdateResourceW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{RT_GROUP_ICON, RT_ICON};
    let path: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    let group = icon::group(images);
    // SAFETY: the path is NUL-terminated and the data outlives each call.
    unsafe {
        let h = BeginUpdateResourceW(path.as_ptr(), 0);
        if h.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut ok = true;
        for (k, img) in images.iter().enumerate() {
            ok &= UpdateResourceW(h, RT_ICON, (k + 1) as _, 0, img.as_ptr().cast(), img.len() as u32) != 0;
        }
        ok &= UpdateResourceW(h, RT_GROUP_ICON, icon::GROUP as _, 0, group.as_ptr().cast(), group.len() as u32) != 0;
        let err = std::io::Error::last_os_error();
        // The file is only written when every update went in.
        if !ok {
            EndUpdateResourceW(h, 1);
            return Err(err.into());
        }
        if EndUpdateResourceW(h, 0) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn set_icon(_exe: &Path, _images: &[Vec<u8>]) -> Result<()> {
    Ok(())
}

/// A build running on its own thread.
#[derive(Default)]
struct Job {
    fraction: f32,
    stage: String,
    result: Option<Result<&'static str, String>>,
}

struct SetupApp {
    cfg: Config,
    config_path: PathBuf,
    wad: String,
    key: String,
    weekend: bool,
    /// Rumble Weekend's name and version, if it is in the mods folder.
    weekend_label: Option<String>,
    /// How choosing Rumble Weekend's zip went.
    weekend_note: Option<(String, bool)>,
    patches: Vec<PathBuf>,
    /// Addons for Rumble Weekend builds (`[addons]`).
    addons: crate::config::AddonsConfig,
    /// What the chosen WAD is, or what is wrong with it.
    wad_note: Option<(String, bool)>,
    job: Option<Arc<Mutex<Job>>>,
}

impl SetupApp {
    fn new(cfg: Config, config_path: PathBuf) -> Self {
        let weekend_label = weekend_label(&cfg.paths.mods_dir);
        let mut app = Self {
            // Only files that are there: a fresh config names defaults.
            wad: if cfg.paths.wad.is_file() { cfg.paths.wad.display().to_string() } else { String::new() },
            key: if cfg.paths.common_key.is_file() { cfg.paths.common_key.display().to_string() } else { String::new() },
            weekend: cfg.mods.enabled.iter().any(|m| m == WEEKEND),
            patches: cfg.mods.patches.clone(),
            addons: cfg.addons.clone(),
            weekend_label,
            weekend_note: None,
            wad_note: None,
            job: None,
            cfg,
            config_path,
        };
        app.check_wad();
        app
    }

    fn check_wad(&mut self) {
        let path = Path::new(&self.wad);
        self.wad_note = if !path.is_file() {
            None
        } else {
            Some(match std::fs::read(path).map_err(anyhow::Error::from).and_then(|d| Ok(Wad::parse(&d)?.game_code())) {
                Ok(code) if code == "WPSE" => ("Pokémon Rumble (USA)".to_string(), true),
                Ok(code) => (format!("{code}: only Pokémon Rumble (USA, WPSE) is supported"), false),
                Err(_) => ("not a WAD file".to_string(), false),
            })
        };
    }

    fn start(&mut self, ctx: &egui::Context) {
        let req = Request {
            wad: PathBuf::from(&self.wad),
            key: PathBuf::from(&self.key),
            weekend: self.weekend,
            patches: self.patches.clone(),
            mods_dir: self.cfg.paths.mods_dir.clone(),
        };
        // Remembered for the next rebuild.
        self.cfg.paths.wad = req.wad.clone();
        self.cfg.paths.common_key = req.key.clone();
        self.cfg.mods.enabled.retain(|m| m != WEEKEND);
        if req.weekend {
            self.cfg.mods.enabled.push(WEEKEND.to_string());
        }
        self.cfg.mods.patches = req.patches.clone();
        self.cfg.addons = self.addons.clone();
        let _ = self.cfg.save(&self.config_path);

        let job = Arc::new(Mutex::new(Job::default()));
        self.job = Some(job.clone());
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let report = |f: f32, s: &str| {
                let mut j = job.lock().unwrap();
                (j.fraction, j.stage) = (f, s.to_string());
                ctx.request_repaint();
            };
            let result = build(&req, &report).map_err(|e| format!("{e:#}"));
            job.lock().unwrap().result = Some(result);
            ctx.request_repaint();
        });
    }
}

/// Rumble Weekend's name, version and author, if it is in the mods folder.
fn weekend_label(mods_dir: &Path) -> Option<String> {
    let m = mods::load(mods_dir, &[WEEKEND.to_string()]).ok()?.pop()?;
    let man = m.manifest;
    let name = if man.version.is_empty() { man.name } else { format!("{} {}", man.name, man.version) };
    Some(if man.author.is_empty() { name } else { format!("{name} (by {})", man.author) })
}

fn pick(title: &str, filter: (&str, &[&str])) -> Option<PathBuf> {
    rfd::FileDialog::new().set_title(title).add_filter(filter.0, filter.1).add_filter("All files", &["*"]).pick_file()
}

impl eframe::App for SetupApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let running = self.job.as_ref().is_some_and(|j| j.lock().unwrap().result.is_none());
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Pokémon Rumble Setup");
            ui.label("Builds the game from your own Wii files. No game data or Wii common key comes with this app.");
            ui.add_space(10.0);

            ui.add_enabled_ui(!running, |ui| {
                egui::Grid::new("files").num_columns(3).spacing([8.0, 6.0]).show(ui, |ui| {
                    ui.label("Game (WAD)");
                    if ui.add(egui::TextEdit::singleline(&mut self.wad).hint_text("Choose your .wad file").desired_width(330.0)).changed() {
                        self.check_wad();
                    }
                    if ui.button("Browse…").clicked() {
                        if let Some(p) = pick("Pokémon Rumble WAD", ("WiiWare WAD", &["wad"])) {
                            self.wad = p.display().to_string();
                            self.check_wad();
                        }
                    }
                    ui.end_row();
                    ui.label("");
                    match &self.wad_note {
                        Some((note, true)) => ui.colored_label(egui::Color32::LIGHT_GREEN, note),
                        Some((note, false)) => ui.colored_label(egui::Color32::LIGHT_RED, note),
                        None => ui.weak("Pokémon Rumble (USA), dumped from your own Wii"),
                    };
                    ui.end_row();
                    ui.label("Wii common key");
                    ui.add(egui::TextEdit::singleline(&mut self.key).hint_text("Choose your common key file").desired_width(330.0));
                    if ui.button("Browse…").clicked() {
                        if let Some(p) = pick("Wii common key", ("Key file", &["bin", "key"])) {
                            self.key = p.display().to_string();
                        }
                    }
                    ui.end_row();
                    ui.label("");
                    ui.weak("16 bytes, dumped from your own Wii");
                    ui.end_row();
                });

                ui.add_space(10.0);
                let label = self.weekend_label.as_deref().unwrap_or("Rumble Weekend Edition (by WindyPrairie)");
                ui.checkbox(&mut self.weekend, format!("Patch in {label}"));
                if self.weekend && self.weekend_label.is_none() {
                    ui.indent("weekend", |ui| {
                        ui.label("Rumble Weekend's patch isn't here yet. Download its zip from its page, then choose it here.");
                        ui.horizontal(|ui| {
                            if ui.button("Get Rumble Weekend…").on_hover_text(WEEKEND_PAGE).clicked() {
                                open_page(WEEKEND_PAGE);
                            }
                            if ui.button("Choose the downloaded zip…").clicked() {
                                if let Some(p) = pick("Rumble Weekend zip", ("Zip or BPS", &["zip", "bps"])) {
                                    self.weekend_note = Some(match install_weekend(&self.cfg.paths.mods_dir, &p) {
                                        Ok(n) => (format!("Rumble Weekend is ready ({n} patch{}).", if n == 1 { "" } else { "es" }), true),
                                        Err(e) => (format!("{e:#}"), false),
                                    });
                                    self.weekend_label = weekend_label(&self.cfg.paths.mods_dir);
                                }
                            }
                        });
                    });
                }
                match &self.weekend_note {
                    Some((note, true)) => ui.colored_label(egui::Color32::LIGHT_GREEN, note),
                    Some((note, false)) => ui.colored_label(egui::Color32::LIGHT_RED, note),
                    None => ui.label(""),
                };
                if self.weekend {
                    ui.indent("addons", |ui| {
                        ui.label("Addons:");
                        ui.checkbox(&mut self.addons.custom_sparkle, "Custom sparkling effect")
                            .on_hover_text("Shiny Pokémon sparkle with stars, in yellow and cyan, at random angles.");
                    });
                }

                ui.add_space(10.0);
                ui.label("Other patches (.bps), applied in this order:");
                let mut remove = None;
                for (k, p) in self.patches.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.small_button("Remove").clicked() {
                            remove = Some(k);
                        }
                        ui.label(p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned()))
                            .on_hover_text(p.display().to_string());
                    });
                }
                if let Some(k) = remove {
                    self.patches.remove(k);
                }
                if ui.button("Add patch…").clicked() {
                    if let Some(p) = pick("BPS patch", ("BPS patch", &["bps"])) {
                        self.patches.push(p);
                    }
                }
            });

            ui.add_space(14.0);
            let name = build_name(self.weekend);
            ui.label(format!("Creates {name}.exe in this folder. Rebuilding keeps its save."));
            ui.label(format!("Also creates {CLIENT_NAME}.exe, for joining a friend's online game."));
            ui.add_space(6.0);
            let ready = self.wad_note.as_ref().is_some_and(|n| n.1)
                && Path::new(&self.key).is_file()
                && (!self.weekend || self.weekend_label.is_some());
            if ui.add_enabled(ready && !running, egui::Button::new("Build").min_size(egui::vec2(120.0, 30.0))).clicked() {
                self.start(ctx);
            }

            let mut play = None;
            if let Some(job) = &self.job {
                let j = job.lock().unwrap();
                ui.add_space(8.0);
                match &j.result {
                    None => {
                        ui.add(egui::ProgressBar::new(j.fraction).show_percentage().animate(true));
                        ui.label(&j.stage);
                    }
                    Some(Ok(built)) => {
                        ui.colored_label(egui::Color32::LIGHT_GREEN, format!("Done: {built}.exe is ready."));
                        if ui.button(format!("Play {built}")).clicked() {
                            play = Some(*built);
                        }
                    }
                    Some(Err(e)) => {
                        ui.colored_label(egui::Color32::LIGHT_RED, format!("Build failed: {e}"));
                    }
                }
            }
            if let Some(built) = play {
                let _ = std::process::Command::new(format!("{built}.exe")).spawn();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

/// The setup window's icon, also the setup exe's.
fn setup_icon() -> egui::IconData {
    let image = Image::from_png(include_bytes!("../assets/setup-icon.png")).expect("setup icon");
    egui::IconData { rgba: image.rgba, width: image.width, height: image.height }
}

/// Opens the setup window.
pub fn run_app(cfg: Config, config_path: PathBuf) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([600.0, 480.0])
            .with_title("Pokémon Rumble Setup")
            .with_icon(setup_icon()),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native("Pokémon Rumble Setup", options, Box::new(|_| Ok(Box::new(SetupApp::new(cfg, config_path)))))
        .map_err(|e| anyhow::anyhow!("{e}"))
}
