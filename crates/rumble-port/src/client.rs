//! The joiner's app (`Pokemon Rumble Client.exe`, or `rumble-port
//! client`): finds a host's online session, joins it after the player says
//! yes and types the password, then shows the host's picture, plays its
//! sound and sends this PC's keyboard and gamepads as the player given (see
//! `docs/online.md`). It needs no game files.

use crate::audio::{Audio, Mix};
use crate::config::Config;
use crate::controls::{Bindings, Gamepads};
use crate::net::{Client, Session, ToClient, QUALITIES};
use anyhow::Result;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use winit::keyboard::KeyCode;

/// How long a toast stays up.
const TOAST_TIME: Duration = Duration::from_secs(4);

/// Where the app is.
enum Stage {
    /// Name, address and picture quality.
    Connect,
    /// Asking the address whose session it is.
    Finding(Receiver<Result<Session, String>>),
    /// "X's multiplayer session found. Would you like to connect?", then
    /// the password.
    Found { session: Session, asking_password: bool },
    Joining(Receiver<Result<Client, String>>),
    Playing(Playing),
}

/// In a session.
struct Playing {
    client: Client,
    /// The latest picture, decoded, waiting to be shown.
    picture: Arc<Mutex<Option<egui::ColorImage>>>,
    texture: Option<egui::TextureHandle>,
    toasts: Arc<Mutex<Vec<(String, Instant)>>>,
    closed: Arc<AtomicBool>,
    /// Kept open while playing.
    _audio: Option<Audio>,
    menu_open: bool,
}

struct ClientApp {
    cfg: Config,
    config_path: PathBuf,
    stage: Stage,
    password: String,
    /// What went wrong last, shown on the connect screen.
    error: Option<String>,
    bindings: Bindings,
    gamepads: Gamepads,
    mix: Arc<Mix>,
}

impl ClientApp {
    fn new(cfg: Config, config_path: PathBuf) -> Self {
        let mix = Arc::new(Mix::default());
        mix.set(&cfg.audio, false);
        Self {
            bindings: Bindings::new(&cfg.controls),
            gamepads: Gamepads::new(),
            mix,
            cfg,
            config_path,
            stage: Stage::Connect,
            password: String::new(),
            error: None,
        }
    }

    fn back_to_connect(&mut self, error: Option<String>) {
        self.stage = Stage::Connect;
        self.password.clear();
        self.error = error;
    }

    fn connect_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Join a multiplayer session");
        ui.add_space(10.0);
        let o = &mut self.cfg.online;
        egui::Grid::new("connect").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Your name");
            ui.add(egui::TextEdit::singleline(&mut o.name).char_limit(crate::net::MAX_NAME).hint_text("Shown to other players"));
            ui.end_row();
            ui.label("Host");
            ui.add(egui::TextEdit::singleline(&mut o.last_host).hint_text("ip:port"));
            ui.end_row();
            ui.label("Picture");
            egui::ComboBox::from_id_salt("quality").selected_text(quality_label(o.quality)).show_ui(ui, |ui| {
                for q in QUALITIES {
                    ui.selectable_value(&mut o.quality, q, quality_label(q));
                }
            });
            ui.end_row();
        });
        ui.small("A sharper picture needs a faster connection, at the host's end most of all.");
        ui.add_space(8.0);
        let ready = !crate::net::clean_name(&o.name).is_empty() && !o.last_host.trim().is_empty();
        if ui.add_enabled(ready, egui::Button::new("Connect").min_size(egui::vec2(120.0, 28.0))).clicked() {
            let _ = self.cfg.save(&self.config_path);
            let addr = self.cfg.online.last_host.clone();
            let (tx, rx) = channel();
            std::thread::spawn(move || {
                let _ = tx.send(Session::find(&addr).map_err(|e| format!("{e:#}")));
            });
            self.error = None;
            self.stage = Stage::Finding(rx);
        }
        if let Some(e) = &self.error {
            ui.add_space(6.0);
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
    }

    fn found_screen(&mut self, ui: &mut egui::Ui) {
        let Stage::Found { session, asking_password } = &mut self.stage else { return };
        let players = if session.players == 1 { "1 player".to_string() } else { format!("{} players", session.players) };
        ui.heading(format!("{}'s multiplayer session found", session.host));
        ui.label(format!("{players} in it."));
        ui.add_space(10.0);
        if !*asking_password {
            ui.label("Would you like to connect?");
            let (mut yes, mut no) = (false, false);
            ui.horizontal(|ui| {
                yes = ui.button("Yes").clicked();
                no = ui.button("No").clicked();
            });
            if yes {
                *asking_password = true;
            } else if no {
                self.back_to_connect(None);
            }
            return;
        }
        ui.label("Password");
        let field = ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
        field.request_focus();
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        let mut join = false;
        let mut cancel = false;
        ui.horizontal(|ui| {
            join = ui.add_enabled(!self.password.is_empty(), egui::Button::new("Join")).clicked();
            cancel = ui.button("Cancel").clicked();
        });
        if cancel {
            self.back_to_connect(None);
        } else if (join || enter) && !self.password.is_empty() {
            let Stage::Found { session, .. } = std::mem::replace(&mut self.stage, Stage::Connect) else { return };
            let (password, name, quality) = (self.password.clone(), self.cfg.online.name.clone(), self.cfg.online.quality);
            let (tx, rx) = channel();
            std::thread::spawn(move || {
                // H.264 if this PC can decode it (the decoding thread opens
                // its own decoder).
                let h264 = crate::video::Decoder::new().is_ok();
                let _ = tx.send(session.join(&password, &name, quality, h264).map_err(|e| format!("{e:#}")));
            });
            self.stage = Stage::Joining(rx);
        }
    }

    /// Starts playing in a session just joined.
    fn play(&mut self, mut client: Client) {
        let audio = Audio::open(self.mix.clone()).map_err(|e| eprintln!("sound off: {e:#}")).ok();
        let queue = audio.as_ref().map(|a| a.queue.clone());
        let picture = Arc::new(Mutex::new(None));
        let toasts = Arc::new(Mutex::new(vec![(format!("Joined {}'s session as Player {}", client.host, client.player), Instant::now())]));
        let closed = Arc::new(AtomicBool::new(false));
        let messages = client.messages();
        // Pictures are decoded on a thread of their own, newest first: one
        // arriving while another decodes replaces any still waiting, so
        // they never fall behind, and sound never waits for them.
        let jpeg: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let (wake, woken) = std::sync::mpsc::sync_channel::<()>(1);
        let (j, p) = (jpeg.clone(), picture.clone());
        std::thread::spawn(move || {
            for () in woken {
                let Some(data) = j.lock().unwrap().take() else { continue };
                if let Some(image) = decode(&data) {
                    *p.lock().unwrap() = Some(image);
                }
            }
        });
        // H.264 frames are all decoded, in order (each builds on the last),
        // but only the newest of those waiting becomes a picture.
        let (frames, waiting) = channel::<(u16, u16, Vec<u8>)>();
        let p = picture.clone();
        std::thread::spawn(move || {
            let mut decoder = None;
            while let Ok(first) = waiting.recv() {
                let batch: Vec<_> = std::iter::once(first).chain(waiting.try_iter()).collect();
                let last = batch.len() - 1;
                for (i, (width, height, data)) in batch.into_iter().enumerate() {
                    if decoder.is_none() {
                        match crate::video::Decoder::new() {
                            Ok(d) => decoder = Some(d),
                            Err(e) => {
                                eprintln!("H.264: {e:#}");
                                return;
                            }
                        }
                    }
                    let Some(d) = decoder.as_mut() else { return };
                    match d.decode(&data, width as u32, height as u32, i == last) {
                        Ok(Some(shown)) => {
                            let size = [shown.width as usize, shown.height as usize];
                            *p.lock().unwrap() = Some(egui::ColorImage::from_rgba_unmultiplied(size, &shown.rgba));
                        }
                        Ok(None) => {}
                        Err(e) => eprintln!("H.264: {e:#}"),
                    }
                }
            }
        });
        let (t, c) = (toasts.clone(), closed.clone());
        std::thread::spawn(move || {
            for msg in messages {
                match msg {
                    Some(ToClient::Picture(data)) => {
                        *jpeg.lock().unwrap() = Some(data);
                        let _ = wake.try_send(());
                    }
                    Some(ToClient::Video { width, height, data, .. }) => {
                        let _ = frames.send((width, height, data));
                    }
                    Some(ToClient::Sound { rate, samples }) => {
                        if let Some(q) = &queue {
                            q.push(&samples, rate);
                        }
                    }
                    Some(ToClient::Toast(text)) => t.lock().unwrap().push((text, Instant::now())),
                    Some(ToClient::Welcome { .. }) => {}
                    None => {
                        c.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        });
        self.stage = Stage::Playing(Playing { client, picture, texture: None, toasts, closed, _audio: audio, menu_open: false });
    }

    /// The Wii Remote buttons this PC holds (keys and gamepads, through the
    /// config's bindings) and its left stick.
    fn held(&mut self, ctx: &egui::Context) -> (u32, [f32; 2]) {
        let pads = self.gamepads.pads(&self.bindings);
        let mut bits = pads.iter().fold(0, |acc, p| acc | p.buttons);
        let stick = crate::controls::analog_stick(crate::controls::strongest(pads.iter().map(|p| p.stick)));
        ctx.input(|i| {
            for key in i.keys_down.iter().filter_map(|&k| key_code(k)) {
                bits |= self.bindings.key(key);
            }
            if i.modifiers.shift {
                bits |= self.bindings.key(KeyCode::ShiftLeft);
            }
            if i.modifiers.ctrl {
                bits |= self.bindings.key(KeyCode::ControlLeft);
            }
            if i.modifiers.alt {
                bits |= self.bindings.key(KeyCode::AltLeft);
            }
        });
        (bits, stick)
    }

    fn playing(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.focused);
        let menu_open = matches!(&self.stage, Stage::Playing(p) if p.menu_open);
        let (held, stick) = if focused && !menu_open { self.held(ctx) } else { (0, [0.0; 2]) };
        let Stage::Playing(p) = &mut self.stage else { return };
        if p.closed.load(Ordering::Relaxed) || p.client.send_input(held, stick).is_err() {
            let host = p.client.host.clone();
            self.back_to_connect(Some(format!("Lost the connection to {host}'s session.")));
            return;
        }
        if let Some(image) = p.picture.lock().unwrap().take() {
            match &mut p.texture {
                Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                None => p.texture = Some(ctx.load_texture("picture", image, egui::TextureOptions::LINEAR)),
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            p.menu_open = !p.menu_open;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::F11)) {
            let full = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!full));
        }

        egui::CentralPanel::default().frame(egui::Frame::new().fill(egui::Color32::BLACK)).show(ctx, |ui| {
            let area = ui.max_rect();
            match &p.texture {
                Some(t) => {
                    // Fit the picture, keeping its shape.
                    let size = t.size_vec2();
                    let scale = (area.width() / size.x).min(area.height() / size.y);
                    let rect = egui::Rect::from_center_size(area.center(), size * scale);
                    ui.painter().image(t.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
                }
                None => {
                    ui.centered_and_justified(|ui| ui.label(egui::RichText::new("Waiting for the host's picture…").color(egui::Color32::GRAY).size(20.0)));
                }
            }
        });

        let mut toasts = p.toasts.lock().unwrap();
        toasts.retain(|(_, at)| at.elapsed() < TOAST_TIME);
        if !toasts.is_empty() {
            egui::Area::new(egui::Id::new("toasts")).anchor(egui::Align2::CENTER_TOP, [0.0, 16.0]).interactable(false).show(ctx, |ui| {
                for (text, _) in toasts.iter() {
                    egui::Frame::new()
                        .fill(egui::Color32::from_black_alpha(190))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(14, 8))
                        .show(ui, |ui| ui.add(egui::Label::new(egui::RichText::new(text).size(20.0).color(egui::Color32::WHITE)).extend()));
                    ui.add_space(6.0);
                }
            });
        }
        drop(toasts);

        let mut leave = false;
        if p.menu_open {
            egui::Window::new("Paused").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                ui.label(format!("Playing as Player {} in {}'s session.", p.client.player, p.client.host));
                ui.small("The game keeps going while this is open. F11: fullscreen.");
                ui.horizontal(|ui| {
                    if ui.button("Resume").clicked() {
                        p.menu_open = false;
                    }
                    leave = ui.button("Leave session").clicked();
                });
            });
        }
        if leave {
            if let Stage::Playing(p) = std::mem::replace(&mut self.stage, Stage::Connect) {
                p.client.leave();
            }
            self.back_to_connect(None);
        }
    }
}

fn quality_label(height: u16) -> String {
    match height {
        2160 => "4K (2160p)".into(),
        h => format!("{h}p"),
    }
}

/// A JPEG picture as an egui image.
fn decode(jpeg: &[u8]) -> Option<egui::ColorImage> {
    let mut decoder = jpeg_decoder::Decoder::new(jpeg);
    let pixels = decoder.decode().ok()?;
    let info = decoder.info()?;
    let (w, h) = (info.width as usize, info.height as usize);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => Some(egui::ColorImage::from_rgb([w, h], &pixels)),
        _ => None,
    }
}

/// The key a binding names for an egui key.
fn key_code(key: egui::Key) -> Option<KeyCode> {
    use egui::Key as E;
    use KeyCode as K;
    Some(match key {
        E::A => K::KeyA, E::B => K::KeyB, E::C => K::KeyC, E::D => K::KeyD, E::E => K::KeyE, E::F => K::KeyF,
        E::G => K::KeyG, E::H => K::KeyH, E::I => K::KeyI, E::J => K::KeyJ, E::K => K::KeyK, E::L => K::KeyL,
        E::M => K::KeyM, E::N => K::KeyN, E::O => K::KeyO, E::P => K::KeyP, E::Q => K::KeyQ, E::R => K::KeyR,
        E::S => K::KeyS, E::T => K::KeyT, E::U => K::KeyU, E::V => K::KeyV, E::W => K::KeyW, E::X => K::KeyX,
        E::Y => K::KeyY, E::Z => K::KeyZ,
        E::Num0 => K::Digit0, E::Num1 => K::Digit1, E::Num2 => K::Digit2, E::Num3 => K::Digit3, E::Num4 => K::Digit4,
        E::Num5 => K::Digit5, E::Num6 => K::Digit6, E::Num7 => K::Digit7, E::Num8 => K::Digit8, E::Num9 => K::Digit9,
        E::ArrowUp => K::ArrowUp, E::ArrowDown => K::ArrowDown, E::ArrowLeft => K::ArrowLeft, E::ArrowRight => K::ArrowRight,
        E::Space => K::Space, E::Enter => K::Enter, E::Tab => K::Tab, E::Backspace => K::Backspace,
        E::Minus => K::Minus, E::Equals => K::Equal, E::Plus => K::NumpadAdd,
        E::OpenBracket => K::BracketLeft, E::CloseBracket => K::BracketRight, E::Backslash => K::Backslash,
        E::Semicolon => K::Semicolon, E::Quote => K::Quote, E::Comma => K::Comma, E::Period => K::Period,
        E::Slash => K::Slash, E::Backtick => K::Backquote, E::Insert => K::Insert, E::Delete => K::Delete,
        E::Home => K::Home, E::End => K::End, E::PageUp => K::PageUp, E::PageDown => K::PageDown,
        E::F1 => K::F1, E::F2 => K::F2, E::F3 => K::F3, E::F4 => K::F4, E::F5 => K::F5, E::F6 => K::F6,
        E::F7 => K::F7, E::F8 => K::F8, E::F9 => K::F9, E::F10 => K::F10, E::F12 => K::F12,
        _ => return None,
    })
}

impl eframe::App for ClientApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Answers from the network threads.
        match &self.stage {
            Stage::Finding(rx) => match rx.try_recv() {
                Ok(Ok(session)) => self.stage = Stage::Found { session, asking_password: false },
                Ok(Err(e)) => self.back_to_connect(Some(e)),
                Err(_) => {}
            },
            Stage::Joining(rx) => match rx.try_recv() {
                Ok(Ok(client)) => self.play(client),
                Ok(Err(e)) => self.back_to_connect(Some(format!("Couldn't join: {e}"))),
                Err(_) => {}
            },
            _ => {}
        }
        if matches!(self.stage, Stage::Playing(_)) {
            self.playing(ctx);
            ctx.request_repaint();
            return;
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.set_max_width(420.0);
                match &self.stage {
                    Stage::Connect => self.connect_screen(ui),
                    Stage::Finding(_) => {
                        ui.heading("Looking for the session…");
                        ui.spinner();
                    }
                    Stage::Found { .. } => self.found_screen(ui),
                    Stage::Joining(_) => {
                        ui.heading("Joining…");
                        ui.spinner();
                    }
                    Stage::Playing(_) => {}
                }
            });
        });
        // Keep polling the network threads.
        ctx.request_repaint_after(Duration::from_millis(50));
    }
}

/// Opens the client window.
pub fn run(cfg: Config, config_path: PathBuf) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 720.0]).with_title("Pokémon Rumble — Join a session"),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native("Pokémon Rumble Client", options, Box::new(|cc| {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        Ok(Box::new(ClientApp::new(cfg, config_path)))
    }))
    .map_err(|e| anyhow::anyhow!("{e}"))
}
