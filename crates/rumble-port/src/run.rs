//! `run` command: the game in a window. Emulation runs on its own thread
//! and hands each finished frame (the GX commands up to a copy to the
//! external framebuffer) to the window thread, which renders it with wgpu
//! and presents the result. The channel between them is small, so the
//! emulator waits when the display falls behind. Keyboard and gamepads are
//! Wii Remote 1 (`controls.rs`). Esc pauses the game and opens the menu
//! (`menu.rs`), whose changes are applied here and saved to the config.
//! Emulation is held to real time, and the title bar shows the speed it
//! actually reaches. `build/remote/` takes commands from a file
//! (`remote.rs`) and gets the same speed figures in `status.txt`.

use crate::boot;
use crate::config::Config;
use crate::audio::{Audio, AudioQueue, Mix};
use crate::controls::{Bindings, Gamepads};
use crate::display::{Aspect, DisplayConfig, NamedResolution, Resolution};
use crate::menu::{Actions, Device, Menu};
use crate::remote::{self, Action, Recorder, Remote, Settings};
use crate::saves;
use rumble_formats::wiisave::Save;
use crate::render::{self, Renderer};
use anyhow::{Context, Result};
use rumble_formats::dol::Dol;
use rumble_recomp::analysis;
use rumble_runtime::cpu::{Stop, TB_HZ};
use rumble_runtime::gx::draw::GpuCommand;
use rumble_runtime::input::Remotes;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};
use winit::window::{Fullscreen, Window, WindowId};

const TITLE: &str = "Pokémon Rumble";
/// Instructions per slice between handing frames over.
const SLICE: u64 = 1_000_000;
/// Frames that may wait for the window before the emulator blocks.
const QUEUED_FRAMES: usize = 2;
/// How often a paused emulator checks whether to carry on.
const PAUSE_POLL: Duration = Duration::from_millis(20);
/// How far emulation may fall behind real time before it stops trying to
/// catch up.
const MAX_LAG: Duration = Duration::from_millis(250);
/// How often the speed shown in the title bar is updated.
const SPEED_PERIOD: Duration = Duration::from_secs(1);

enum Event {
    Frame,
    /// Emulated time per real time, frames per second, and the share of
    /// real time spent running the CPU, over the last period, and the
    /// frame count.
    Speed { ratio: f64, fps: f64, busy: f64, frame: u64 },
    /// A remote `screenshot` command.
    Screenshot(PathBuf),
    Stopped(String),
    /// How a save import or export went: the message, and whether it
    /// failed.
    SaveStatus(String, bool),
}

/// Holds emulated time (the timebase) to real time.
struct Pace {
    start: Instant,
    start_tb: u64,
    period_start: Instant,
    period_tb: u64,
    period_frames: u64,
    /// Time spent in the CPU this period.
    busy: Duration,
}

impl Pace {
    fn new(tb: u64, frames: u64) -> Self {
        let now = Instant::now();
        Pace { start: now, start_tb: tb, period_start: now, period_tb: tb, period_frames: frames, busy: Duration::ZERO }
    }

    /// Sleeps while emulation is ahead of real time. When it has fallen
    /// too far behind, the lag is dropped rather than made up in a burst.
    fn wait(&mut self, tb: u64) {
        let emulated = Duration::from_secs_f64((tb - self.start_tb) as f64 / TB_HZ as f64);
        let real = self.start.elapsed();
        if emulated > real {
            std::thread::sleep(emulated - real);
        } else if real - emulated > MAX_LAG {
            self.start = Instant::now();
            self.start_tb = tb;
        }
    }

    /// The speed over the last period, once a period has passed.
    fn speed(&mut self, tb: u64, frames: u64) -> Option<Event> {
        let real = self.period_start.elapsed();
        if real < SPEED_PERIOD {
            return None;
        }
        let secs = real.as_secs_f64();
        let ratio = (tb - self.period_tb) as f64 / TB_HZ as f64 / secs;
        let fps = (frames - self.period_frames) as f64 / secs;
        let busy = self.busy.as_secs_f64() / secs;
        (self.period_start, self.period_tb, self.period_frames, self.busy) = (Instant::now(), tb, frames, Duration::ZERO);
        Some(Event::Speed { ratio, fps, busy, frame: frames })
    }
}

/// Flags the window thread sets for the emulation thread.
#[derive(Default)]
struct Control {
    quit: AtomicBool,
    paused: AtomicBool,
    /// `ResolvedDisplay::widescreen` as f32 bits (0 until the window
    /// exists), for the emulator thread to draw with.
    widescreen: AtomicU32,
    /// Stop the game and boot it again, first importing `import` if set.
    restart: AtomicBool,
    import: Mutex<Option<Save>>,
}

pub fn run(cfg: Config, config_path: &Path, presses: Vec<boot::Press>, exec: boot::Exec) -> Result<()> {
    let event_loop = EventLoop::<Event>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let (tx, rx) = sync_channel(QUEUED_FRAMES);
    let control = Arc::new(Control::default());
    let remotes = Arc::new(Remotes::default());
    remotes.connect(0, true);
    let mix = Arc::new(Mix::default());
    mix.set(&cfg.audio, false);
    let audio = Audio::open(mix.clone()).map_err(|e| eprintln!("sound off: {e:#}")).ok();
    let emu = {
        let sound = audio.as_ref().map(|a| a.queue.clone());
        let mix = mix.clone();
        let build_dir = cfg.paths.build_dir.clone();
        let control = control.clone();
        let remotes = remotes.clone();
        let threaded = cfg.game.threaded_graphics;
        let original_multiplayer = cfg.game.original_multiplayer;
        std::thread::Builder::new().name("emulation".into()).spawn(move || loop {
            let result = emulate(build_dir.clone(), remotes.clone(), &presses, exec, threaded, original_multiplayer, tx.clone(), sound.clone(), &mix, &proxy, &control);
            if control.quit.load(Ordering::Relaxed) || !control.restart.swap(false, Ordering::Relaxed) {
                if let Err(e) = result {
                    let _ = proxy.send_event(Event::Stopped(format!("{e:#}")));
                }
                break;
            }
            // The game is gone and nothing has the NAND open: apply an
            // import, then boot again.
            if let Some(save) = control.import.lock().unwrap().take() {
                let status = match saves::apply_import(&build_dir, &save) {
                    Ok(backup) => (format!("Save imported; the previous one is in {}. The game has restarted.", backup.display()), false),
                    Err(e) => (format!("Import failed: {e:#}"), true),
                };
                let _ = proxy.send_event(Event::SaveStatus(status.0, status.1));
            }
        })?
    };
    let mut app = App {
        bindings: Bindings::new(&cfg.controls),
        config: cfg,
        config_path: config_path.to_path_buf(),
        frames: rx,
        window: None,
        gpu: None,
        remotes,
        control: control.clone(),
        keys: HashSet::new(),
        gamepads: Gamepads::new(),
        focused: true,
        menu_open: false,
        fps: 0.0,
        render_time: Duration::ZERO,
        rendered: 0,
        _audio: audio,
        mix,
    };
    app.update_audio();
    event_loop.run_app(&mut app)?;
    control.quit.store(true, Ordering::Relaxed);
    control.paused.store(false, Ordering::Relaxed);
    // Dropping the receiver unblocks an emulator waiting to hand a frame over.
    drop(app);
    emu.join().map_err(|_| anyhow::anyhow!("emulation thread panicked"))
}

fn emulate(
    build_dir: PathBuf,
    remotes: Arc<Remotes>,
    presses: &[boot::Press],
    exec: boot::Exec,
    threaded: bool,
    original_multiplayer: bool,
    tx: SyncSender<Vec<GpuCommand>>,
    sound: Option<Arc<AudioQueue>>,
    mix: &Mix,
    proxy: &EventLoopProxy<Event>,
    control: &Control,
) -> Result<()> {
    let data = boot::read_dol(&build_dir)?;
    let dol = Dol::parse(&data)?;
    let (mut cpu, mut mem) = {
        let prog = analysis::analyze(&dol);
        let (mut cpu, mut mem) = boot::start(&build_dir, "nand", &dol, &prog)?;
        rumble_runtime::hle::multiplay::configure(&mut mem, original_multiplayer);
        boot::install_blocks(&mut cpu, &mut mem, exec, &prog);
        (cpu, mem)
    };
    mem.gx.capture = true;
    if threaded {
        mem.start_gx_worker();
    }
    mem.input.remotes = remotes;
    let mut pace = Pace::new(cpu.tb, mem.gx.stats.xfb_copies);
    let mut remote = Remote::new(&build_dir).map_err(|e| eprintln!("remote control off: {e}")).ok();
    let mut recorder = Recorder::new(&build_dir);
    let mut players = rumble_runtime::snd::PlayerVolumes::default();
    // Instructions run, which recordings are keyed by.
    let mut steps = 0u64;
    while !control.quit.load(Ordering::Relaxed) {
        if control.restart.load(Ordering::Relaxed) {
            return Ok(());
        }
        let frame = mem.gx.stats.xfb_copies;
        if let Some(r) = remote.as_mut() {
            for action in r.poll(frame) {
                match action {
                    Action::Pause => control.paused.store(true, Ordering::Relaxed),
                    Action::Resume => control.paused.store(false, Ordering::Relaxed),
                    Action::Screenshot(path) => {
                        let _ = proxy.send_event(Event::Screenshot(path));
                    }
                }
            }
        }
        if control.paused.load(Ordering::Relaxed) {
            std::thread::sleep(PAUSE_POLL);
            pace = Pace::new(cpu.tb, frame);
            continue;
        }
        let wide = f32::from_bits(control.widescreen.load(Ordering::Relaxed));
        mem.gx.widescreen = if wide >= 1.0 { wide } else { 1.0 };
        let remote_buttons = remote.as_ref().map_or(0, |r| r.buttons(frame));
        mem.input.remotes.set_scripted(0, boot::scripted_buttons(presses, frame) | remote_buttons);
        // Input and settings take effect at slice boundaries only, and are
        // recorded by instruction count, so a session replays exactly.
        mem.input.remotes.latch();
        recorder.record(frame, steps, mem.input.remotes.buttons(0));
        let volumes = mix.volumes();
        recorder.settings(steps, Settings { widescreen: mem.gx.widescreen, volumes });
        players.apply(&mut mem, volumes);
        recorder.ios(frame, &mem.ios.log);
        let t = Instant::now();
        let (stop, ran) = cpu.run(&mut mem, SLICE, &mut |_| {});
        steps += ran;
        pace.busy += t.elapsed();
        if let Some(q) = &sound {
            q.push(&mem.audio, mem.audio_rate);
            mem.audio.clear();
        }
        for frame in mem.gx.take_frames() {
            if tx.send(frame).is_err() {
                return Ok(());
            }
            let _ = proxy.send_event(Event::Frame);
        }
        pace.wait(cpu.tb);
        if let Some(speed) = pace.speed(cpu.tb, mem.gx.stats.xfb_copies) {
            let _ = proxy.send_event(speed);
        }
        if stop != Stop::StepLimit {
            anyhow::bail!("emulation stopped at 0x{:08x}: {stop:?}", cpu.pc);
        }
    }
    Ok(())
}

struct Gpu {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: Renderer,
    menu: Menu,
}

struct App {
    config: Config,
    config_path: PathBuf,
    frames: Receiver<Vec<GpuCommand>>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    remotes: Arc<Remotes>,
    control: Arc<Control>,
    /// Keyboard keys held down.
    keys: HashSet<KeyCode>,
    gamepads: Gamepads,
    /// The config's control bindings, ready to look up.
    bindings: Bindings,
    /// Input counts only while the window has focus: gamepads keep their
    /// last state in the background, which would hold a direction.
    focused: bool,
    menu_open: bool,
    /// The game's frames per second over the last speed period.
    fps: f64,
    /// Time spent building frames' GPU work, and how many, since the speed
    /// was last shown.
    render_time: Duration,
    rendered: u32,
    /// The sound device, if one could be opened (kept open while the game
    /// runs), and the settings it and the emulator thread read.
    _audio: Option<Audio>,
    mix: Arc<Mix>,
}

/// EFB resolution multiple for a window of this size.
fn efb_scale(display: &DisplayConfig, size: (u32, u32)) -> u32 {
    let target_height = display.resolve_for(size).render_target.1;
    (target_height as f32 / 528.0).round().max(1.0) as u32
}

fn present_mode(display: &DisplayConfig) -> wgpu::PresentMode {
    if display.vsync { wgpu::PresentMode::AutoVsync } else { wgpu::PresentMode::AutoNoVsync }
}

fn create_gpu(window: Arc<Window>, display: &DisplayConfig) -> Result<Gpu> {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
    let surface = instance.create_surface(window.clone())?;
    let (adapter, device, queue) = pollster::block_on(render::request_device(&instance, Some(&surface)))?;
    let size = window.inner_size();
    let mut config = surface
        .get_default_config(&adapter, size.width.max(1), size.height.max(1))
        .context("the window surface is not supported by this GPU")?;
    // The EFB holds display-ready values, so present to a non-sRGB format.
    let caps = surface.get_capabilities(&adapter);
    if let Some(&format) = caps.formats.iter().find(|f| !f.is_srgb()) {
        config.format = format;
    }
    config.present_mode = present_mode(display);
    surface.configure(&device, &config);
    let renderer = Renderer::new(device.clone(), queue.clone(), efb_scale(display, (config.width, config.height)));
    let menu = Menu::new(&window, &device, config.format);
    Ok(Gpu { surface, config, device, queue, renderer, menu })
}

impl App {
    /// Asks for a data.bin, checks it, and has the emulation thread import
    /// it and restart the game.
    fn import_save(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("Import save").add_filter("Wii save (data.bin)", &["bin"]);
        if let Some(w) = &self.window {
            dialog = dialog.set_parent(w.as_ref());
        }
        let Some(path) = dialog.pick_file() else { return };
        let status = match saves::read_import(&self.config, &path) {
            Ok(save) => {
                *self.control.import.lock().unwrap() = Some(save);
                self.control.restart.store(true, Ordering::Relaxed);
                ("Importing and restarting the game…".to_string(), false)
            }
            Err(e) => (format!("Not imported: {e:#}"), true),
        };
        self.set_save_status(status);
    }

    /// Asks where to write a data.bin and exports the save there.
    fn export_save(&mut self) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Export save")
            .add_filter("Wii save (data.bin)", &["bin"])
            .set_file_name("data.bin");
        if let Some(w) = &self.window {
            dialog = dialog.set_parent(w.as_ref());
        }
        let Some(path) = dialog.save_file() else { return };
        let status = match saves::export(&self.config, &path) {
            Ok(signer) => (format!("Exported to {} (signed as {signer}).", path.display()), false),
            Err(e) => (format!("Export failed: {e:#}"), true),
        };
        self.set_save_status(status);
    }

    fn set_save_status(&mut self, (msg, failed): (String, bool)) {
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.menu.save_status = Some((msg, failed));
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn remote_dir(&self) -> PathBuf {
        self.config.paths.build_dir.join("remote")
    }

    /// Sends what the keyboard and gamepads hold to remote 1 (nothing while
    /// the menu is open or the window is in the background).
    fn update_input(&mut self) {
        let bits = if self.menu_open || !self.focused {
            0
        } else {
            let keys = self.keys.iter().fold(0, |acc, &k| acc | self.bindings.key(k));
            keys | self.gamepads.buttons(&self.bindings)
        };
        self.remotes.set_buttons(0, bits);
    }

    fn set_menu(&mut self, open: bool) {
        self.menu_open = open;
        self.update_paused();
        if open {
            self.keys.clear();
            if let Some(gpu) = self.gpu.as_mut() {
                gpu.menu.open();
            }
        }
        self.update_input();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Hands the audio settings to the sound device.
    fn update_audio(&self) {
        self.mix.set(&self.config.audio, !self.focused && self.config.audio.mute_in_background);
    }

    /// Paused while the menu is open, or in the background if the config
    /// says so.
    fn update_paused(&self) {
        let background = !self.focused && self.config.game.pause_in_background;
        self.control.paused.store(self.menu_open || background, Ordering::Relaxed);
    }

    /// The menu has changed the controls: use and save them.
    fn controls_changed(&mut self) {
        self.bindings = Bindings::new(&self.config.controls);
        self.save_config();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// While the menu waits for a gamepad button to bind, hands it the
    /// next one pressed (polled each redraw, kept coming meanwhile).
    fn capture_pad(&mut self) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        let pressed = self.gamepads.take_pressed();
        if gpu.menu.capturing() != Some(Device::Gamepad) {
            return;
        }
        if let Some(button) = pressed {
            if gpu.menu.bind_pad(&mut self.config.controls, button) {
                self.controls_changed();
                return;
            }
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn save_config(&self) {
        if let Err(e) = self.config.save(&self.config_path) {
            eprintln!("{e:#}");
        }
    }

    /// Applies the display settings to the window, surface and renderer.
    fn apply_display(&mut self) {
        let display = self.config.display.clone();
        let Some(window) = self.window.clone() else { return };
        if display.fullscreen {
            window.set_fullscreen(Some(Fullscreen::Borderless(None)));
        } else {
            window.set_fullscreen(None);
            let (w, h) = match display.resolution {
                Resolution::Named(NamedResolution::Native) => crate::display::native_resolution(),
                Resolution::Fixed { width, height } => (width, height),
            };
            let _ = window.request_inner_size(PhysicalSize::new(w, h));
        }
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.config.present_mode = present_mode(&display);
            gpu.surface.configure(&gpu.device, &gpu.config);
        }
        self.fit_renderer();
        window.request_redraw();
    }

    /// Sizes the renderer's EFB for the window: resolution multiple and
    /// widescreen width, which the emulator thread draws with too.
    fn fit_renderer(&mut self) {
        let Some(gpu) = self.gpu.as_mut() else { return };
        let size = (gpu.config.width, gpu.config.height);
        gpu.renderer.set_scale(efb_scale(&self.config.display, size));
        gpu.renderer.set_widescreen(self.config.display.resolve_for(size).widescreen());
        self.control.widescreen.store(gpu.renderer.widescreen().to_bits(), Ordering::Relaxed);
    }

    fn redraw(&mut self) -> Actions {
        let Some(gpu) = self.gpu.as_mut() else { return Actions::default() };
        let frame = match gpu.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                return Actions::default();
            }
            Err(e) => {
                eprintln!("presenting: {e}");
                return Actions::default();
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let size = (gpu.config.width, gpu.config.height);
        let resolved = self.config.display.resolve_for(size);
        // Widescreen fills the display area, as does "stretch"; otherwise
        // the picture is 4:3.
        let vp = if self.config.display.aspect == Aspect::Stretch || resolved.widescreen() > 1.0 {
            resolved.viewport
        } else {
            resolved.hud_viewport()
        };
        gpu.renderer.present(&view, gpu.config.format, [vp.x as f32, vp.y as f32, vp.width as f32, vp.height as f32]);
        let mut actions = Actions::default();
        let fps = self.config.game.show_fps.then(|| format!("{:.0} FPS", self.fps));
        if self.menu_open || fps.is_some() {
            if let Some(window) = &self.window {
                actions = gpu.menu.draw(window, &gpu.device, &gpu.queue, &view, size, &mut self.config, self.menu_open, fps.as_deref());
            }
        }
        frame.present();
        actions
    }
}

impl ApplicationHandler<Event> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let display = &self.config.display;
        let size = display.resolve().window;
        let mut attrs = Window::default_attributes().with_title(TITLE).with_inner_size(PhysicalSize::new(size.0, size.1));
        if display.fullscreen {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        #[cfg(windows)]
        {
            // The exe's own icon: the title logo in an exe setup built.
            use winit::platform::windows::{IconExtWindows, WindowAttributesExtWindows};
            if let Ok(icon) = winit::window::Icon::from_resource(crate::icon::GROUP, None) {
                attrs = attrs.with_window_icon(Some(icon.clone())).with_taskbar_icon(Some(icon));
            }
        }
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("creating the window: {e}");
                event_loop.exit();
                return;
            }
        };
        match create_gpu(window.clone(), display) {
            Ok(gpu) => {
                self.gpu = Some(gpu);
                self.fit_renderer();
            }
            Err(e) => {
                eprintln!("{e:#}");
                event_loop.exit();
            }
        }
        self.window = Some(window);
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: Event) {
        match event {
            Event::Frame => {
                self.update_input();
                let Some(gpu) = self.gpu.as_mut() else { return };
                let mut any = false;
                // Every frame is rendered, since each builds on the EFB the
                // previous one left; only the newest is presented.
                while let Ok(frame) = self.frames.try_recv() {
                    let t = Instant::now();
                    gpu.renderer.render_frame(&frame);
                    self.render_time += t.elapsed();
                    self.rendered += 1;
                    any = true;
                }
                if any {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            Event::Speed { ratio, fps, busy, frame } => {
                self.fps = fps;
                let render_ms = self.render_time.as_secs_f64() * 1000.0 / self.rendered.max(1) as f64;
                (self.render_time, self.rendered) = (Duration::ZERO, 0);
                let speed = format!(
                    "{:.0}% speed, {fps:.0} fps | Game Thread {:.0}% busy, render {render_ms:.1} ms/frame",
                    ratio * 100.0,
                    busy * 100.0
                );
                if let Some(w) = &self.window {
                    w.set_title(&format!("{TITLE} | {speed}"));
                }
                let status = format!("frame {frame} | {speed} | menu {}
", if self.menu_open { "open" } else { "closed" });
                let _ = std::fs::write(self.remote_dir().join("status.txt"), status);
            }
            Event::Screenshot(path) => {
                let result = match self.gpu.as_ref() {
                    Some(gpu) => gpu.renderer.save_png(&path).map_err(|e| format!("{e:#}")),
                    None => Err("no window yet".into()),
                };
                let msg = match result {
                    Ok(()) => format!("saved {}", path.display()),
                    Err(e) => format!("screenshot {}: error: {e}", path.display()),
                };
                remote::log(&self.remote_dir(), &msg);
            }
            Event::SaveStatus(msg, failed) => self.set_save_status((msg, failed)),
            Event::Stopped(msg) => {
                eprintln!("{msg}");
                if let Some(w) = &self.window {
                    w.set_title(&format!("{TITLE} (stopped)"));
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if let WindowEvent::KeyboardInput {
            event: KeyEvent { logical_key: Key::Named(NamedKey::Escape), state: ElementState::Pressed, repeat: false, .. },
            ..
        } = event
        {
            // Esc while a binding waits for its key cancels just that.
            match self.gpu.as_mut() {
                Some(gpu) if self.menu_open && gpu.menu.capturing().is_some() => {
                    gpu.menu.cancel_capture();
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
                _ => self.set_menu(!self.menu_open),
            }
            return;
        }
        if let WindowEvent::KeyboardInput {
            event: KeyEvent { physical_key: PhysicalKey::Code(code), state: ElementState::Pressed, repeat: false, .. },
            ..
        } = event
        {
            if let Some(gpu) = self.gpu.as_mut() {
                if self.menu_open && gpu.menu.bind_key(&mut self.config.controls, code) {
                    self.controls_changed();
                    return;
                }
            }
        }
        if self.menu_open {
            if let (Some(gpu), Some(window)) = (self.gpu.as_mut(), self.window.as_ref()) {
                if gpu.menu.on_event(window, &event) {
                    window.request_redraw();
                }
            }
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event: KeyEvent { physical_key: PhysicalKey::Code(code), state, .. }, .. }
                if !self.menu_open =>
            {
                match state {
                    ElementState::Pressed => self.keys.insert(code),
                    ElementState::Released => self.keys.remove(&code),
                };
                self.update_input();
            }
            // Keys released while the window is in the background never
            // arrive, so let go of everything.
            WindowEvent::Focused(focused) => {
                self.focused = focused;
                self.keys.clear();
                self.update_input();
                self.update_paused();
                self.update_audio();
            }
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.config.width = size.width.max(1);
                    gpu.config.height = size.height.max(1);
                    gpu.surface.configure(&gpu.device, &gpu.config);
                }
                self.fit_renderer();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                let actions = self.redraw();
                if actions.quit {
                    event_loop.exit();
                    return;
                }
                if actions.display_changed {
                    self.apply_display();
                }
                if actions.controls_changed {
                    self.controls_changed();
                }
                if actions.config_changed() {
                    self.save_config();
                }
                if actions.game_changed {
                    self.update_paused();
                }
                if actions.audio_changed {
                    self.update_audio();
                }
                if self.menu_open {
                    self.capture_pad();
                }
                if actions.import_save {
                    self.import_save();
                }
                if actions.export_save {
                    self.export_save();
                }
                if actions.resume {
                    self.set_menu(false);
                }
            }
            _ => {}
        }
    }
}
