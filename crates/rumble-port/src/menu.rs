//! The Esc menu, drawn with egui over the game: Resume / Quit and tabs for
//! display settings, control bindings and game options. It edits the
//! config; the window applies and saves what changed.
//!
//! Binding a control: the menu shows "press a key" (or a gamepad button)
//! and the window hands it the next one (`capturing`, `bind_key`,
//! `bind_pad`); Esc cancels.

use crate::net::PortOpening;
use crate::config::{Config, Multiplayer};
use crate::controls::{Action, ControlsConfig, StickTurn};
use crate::display::{Aspect, NamedResolution, Resolution};
use winit::event::WindowEvent;
use winit::keyboard::KeyCode;
use winit::window::Window;

/// What the window shows besides the config: gamepads, toasts, hosting.
#[derive(Default)]
pub struct Overlay {
    /// The connected gamepads' names, in the order players are given to them.
    pub pads: Vec<String>,
    /// Messages shown for a few seconds over the game, newest last.
    pub toasts: Vec<String>,
    /// The password to host with, as typed (never saved).
    pub password: String,
    /// The session being hosted, if any.
    pub hosting: Option<Hosting>,
    /// Why hosting couldn't start.
    pub host_error: Option<String>,
    /// Multiplayer ends at the next Pokemon selection screen.
    pub ending_multiplayer: bool,
}

/// A session being hosted.
pub struct Hosting {
    pub port: u16,
    /// This PC's address on its local network.
    pub lan: Option<std::net::IpAddr>,
    /// Opening the port on the router.
    pub port_opening: crate::net::PortOpening,
    /// Player number and name, the host first.
    pub players: Vec<(u8, String)>,
}

/// What the user did in the menu this frame.
#[derive(Default)]
pub struct Actions {
    pub resume: bool,
    pub quit: bool,
    pub display_changed: bool,
    pub controls_changed: bool,
    pub audio_changed: bool,
    pub game_changed: bool,
    /// "Import save" / "Export save" were clicked.
    pub import_save: bool,
    pub export_save: bool,
    /// "Start hosting" / "Stop hosting" were clicked.
    pub start_hosting: bool,
    pub stop_hosting: bool,
    /// "End multiplayer" was clicked.
    pub end_multiplayer: bool,
}

impl Actions {
    /// Whether anything in the config changed.
    pub fn config_changed(&self) -> bool {
        self.display_changed || self.controls_changed || self.audio_changed || self.game_changed
    }
}

/// The menu's pages.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Display,
    Controls,
    Audio,
    Game,
}

/// Which input a binding being made comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Keyboard,
    Gamepad,
}

/// A binding waiting for the player to press what it should be.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Capture {
    action: Action,
    device: Device,
}

pub struct Menu {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    tab: Tab,
    capture: Option<Capture>,
    /// The outcome of the last save import or export, shown in the Game
    /// tab: (message, whether it failed).
    pub save_status: Option<(String, bool)>,
}

const WINDOW_SIZES: &[(u32, u32)] = &[(1280, 720), (1600, 900), (1920, 1080), (2560, 1440), (3840, 2160)];

const ASPECTS: &[(Aspect, &str)] = &[
    (Aspect::Auto, "Match window"),
    (Aspect::Standard, "4:3"),
    (Aspect::Wide, "16:9"),
    (Aspect::Wide16x10, "16:10"),
    (Aspect::Ultrawide, "21:9"),
    (Aspect::SuperUltrawide, "32:9"),
    (Aspect::Stretch, "Stretch to fill"),
];

fn resolution_label(r: &Resolution) -> String {
    match r {
        Resolution::Named(NamedResolution::Native) => "Native (desktop)".into(),
        Resolution::Fixed { width, height } => format!("{width} x {height}"),
    }
}

/// What a key is called in the menu: "X" rather than "KeyX".
fn key_label(name: &str) -> String {
    let short = name.strip_prefix("Key").or_else(|| name.strip_prefix("Digit")).unwrap_or(name);
    match short {
        "ArrowUp" => "Up arrow",
        "ArrowDown" => "Down arrow",
        "ArrowLeft" => "Left arrow",
        "ArrowRight" => "Right arrow",
        "ShiftLeft" => "Left Shift",
        "ShiftRight" => "Right Shift",
        "ControlLeft" => "Left Ctrl",
        "ControlRight" => "Right Ctrl",
        "AltLeft" => "Left Alt",
        "AltRight" => "Right Alt",
        "Equal" => "=",
        "Minus" => "-",
        "NumpadAdd" => "Numpad +",
        "NumpadSubtract" => "Numpad -",
        other => other,
    }
    .into()
}

/// What a gamepad button is called in the menu, Xbox / PlayStation.
fn pad_label(name: &str) -> String {
    match name {
        "South" => "A / Cross",
        "East" => "B / Circle",
        "West" => "X / Square",
        "North" => "Y / Triangle",
        "LeftTrigger" => "LB / L1",
        "RightTrigger" => "RB / R1",
        "LeftTrigger2" => "LT / L2",
        "RightTrigger2" => "RT / R2",
        "Select" => "Back / Select",
        "Start" => "Start",
        "Mode" => "Guide",
        "LeftThumb" => "L3",
        "RightThumb" => "R3",
        "DPadUp" => "D-pad Up",
        "DPadDown" => "D-pad Down",
        "DPadLeft" => "D-pad Left",
        "DPadRight" => "D-pad Right",
        other => other,
    }
    .into()
}

/// What an action is called in the menu.
fn action_label(a: Action) -> &'static str {
    match a {
        Action::Up => "Up",
        Action::Down => "Down",
        Action::Left => "Left",
        Action::Right => "Right",
        Action::A => "A",
        Action::B => "B",
        Action::One => "1",
        Action::Two => "2 (attack)",
        Action::Plus => "+ (pause)",
        Action::Minus => "-",
        Action::Home => "Home",
    }
}

impl Menu {
    pub fn new(window: &Window, device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let ctx = egui::Context::default();
        ctx.set_visuals(egui::Visuals::dark());
        ctx.style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(8.0, 6.0);
            s.spacing.button_padding = egui::vec2(8.0, 4.0);
            for (style, size) in [
                (egui::TextStyle::Heading, 22.0),
                (egui::TextStyle::Body, 16.0),
                (egui::TextStyle::Button, 16.0),
                (egui::TextStyle::Small, 13.0),
            ] {
                if let Some(font) = s.text_styles.get_mut(&style) {
                    font.size = size;
                }
            }
        });
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let renderer = egui_wgpu::Renderer::new(device, format, None, 1, false);
        Self { ctx, state, renderer, tab: Tab::Display, capture: None, save_status: None }
    }

    /// Called when the menu opens.
    pub fn open(&mut self) {
        self.capture = None;
    }

    /// The input a binding is waiting for, if any.
    pub fn capturing(&self) -> Option<Device> {
        self.capture.map(|c| c.device)
    }

    /// Stops waiting for a binding.
    pub fn cancel_capture(&mut self) {
        self.capture = None;
    }

    /// Binds `key` to the action waiting for one. Returns whether it did.
    pub fn bind_key(&mut self, controls: &mut ControlsConfig, key: KeyCode) -> bool {
        match self.capture.take() {
            Some(Capture { action, device: Device::Keyboard }) if crate::controls::bindable_key(key) => {
                controls.bind_key(action, key);
                true
            }
            other => {
                self.capture = other;
                false
            }
        }
    }

    /// Binds `button` to the action waiting for one. Returns whether it did.
    pub fn bind_pad(&mut self, controls: &mut ControlsConfig, button: gilrs::Button) -> bool {
        match self.capture.take() {
            Some(Capture { action, device: Device::Gamepad }) => {
                controls.bind_pad(action, button);
                true
            }
            other => {
                self.capture = other;
                false
            }
        }
    }

    /// Feeds a window event to the menu; returns whether it needs redrawing.
    pub fn on_event(&mut self, window: &Window, event: &WindowEvent) -> bool {
        self.state.on_window_event(window, event).repaint
    }

    /// Draws over `target` the menu if `open`, and the FPS counter if
    /// there is one, and returns what the user did.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &mut self,
        window: &Window,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        target: &wgpu::TextureView,
        size: (u32, u32),
        cfg: &mut Config,
        overlay: &mut Overlay,
        open: bool,
        fps: Option<&str>,
    ) -> Actions {
        let input = self.state.take_egui_input(window);
        let mut actions = Actions::default();
        let mut page = Page {
            cfg,
            tab: &mut self.tab,
            capture: &mut self.capture,
            save_status: self.save_status.as_ref(),
            overlay,
            actions: &mut actions,
            open,
            fps,
        };
        let output = self.ctx.run(input, |ctx| page.ui(ctx));
        self.state.handle_platform_output(window, output.platform_output);

        let jobs = self.ctx.tessellate(output.shapes, output.pixels_per_point);
        for (id, delta) in &output.textures_delta.set {
            self.renderer.update_texture(device, queue, *id, delta);
        }
        let screen = egui_wgpu::ScreenDescriptor { size_in_pixels: [size.0, size.1], pixels_per_point: output.pixels_per_point };
        let mut enc = device.create_command_encoder(&Default::default());
        let extra = self.renderer.update_buffers(device, queue, &mut enc, &jobs, &screen);
        {
            let pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("menu"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            self.renderer.render(&mut pass.forget_lifetime(), &jobs, &screen);
        }
        queue.submit(extra.into_iter().chain([enc.finish()]));
        for id in &output.textures_delta.free {
            self.renderer.free_texture(id);
        }
        actions
    }
}

/// One frame of the menu's UI.
struct Page<'a> {
    cfg: &'a mut Config,
    tab: &'a mut Tab,
    save_status: Option<&'a (String, bool)>,
    capture: &'a mut Option<Capture>,
    overlay: &'a mut Overlay,
    actions: &'a mut Actions,
    open: bool,
    fps: Option<&'a str>,
}

impl Page<'_> {
    fn ui(&mut self, ctx: &egui::Context) {
        if let Some(fps) = self.fps {
            egui::Area::new(egui::Id::new("fps"))
                .anchor(egui::Align2::RIGHT_TOP, [-12.0, 12.0])
                .interactable(false)
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::new()
                        .fill(egui::Color32::from_black_alpha(160))
                        .corner_radius(4.0)
                        .inner_margin(egui::Margin::symmetric(8, 4))
                        .show(ui, |ui| ui.add(egui::Label::new(egui::RichText::new(fps).monospace().size(16.0).color(egui::Color32::WHITE)).extend()));
                });
        }
        if !self.overlay.toasts.is_empty() {
            egui::Area::new(egui::Id::new("toasts"))
                .anchor(egui::Align2::CENTER_TOP, [0.0, 16.0])
                .interactable(false)
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    for toast in &self.overlay.toasts {
                        egui::Frame::new()
                            .fill(egui::Color32::from_black_alpha(190))
                            .corner_radius(6.0)
                            .inner_margin(egui::Margin::symmetric(14, 8))
                            .show(ui, |ui| ui.add(egui::Label::new(egui::RichText::new(toast).size(20.0).color(egui::Color32::WHITE)).extend()));
                        ui.add_space(6.0);
                    }
                });
        }
        if !self.open {
            return;
        }
        // Dim the paused game behind the menu.
        let screen = ctx.screen_rect();
        ctx.layer_painter(egui::LayerId::background()).rect_filled(screen, 0.0, egui::Color32::from_black_alpha(170));

        egui::Window::new("Paused")
            .collapsible(false)
            .resizable(false)
            .fixed_size([640.0, 0.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (tab, label) in
                        [
                        (Tab::Display, "Display"),
                        (Tab::Controls, "Controls"),
                        (Tab::Audio, "Audio"),
                        (Tab::Game, "Game"),
                    ]
                    {
                        if ui.selectable_label(*self.tab == tab, label).clicked() {
                            *self.tab = tab;
                            *self.capture = None;
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Quit").clicked() {
                            self.actions.quit = true;
                        }
                        if ui.button("Resume").clicked() {
                            self.actions.resume = true;
                        }
                    });
                });
                ui.separator();
                match *self.tab {
                    Tab::Display => self.display(ui),
                    Tab::Controls => self.controls(ui),
                    Tab::Audio => self.audio(ui),
                    Tab::Game => self.game(ui),
                }
            });
    }

    fn display(&mut self, ui: &mut egui::Ui) {
        let d = &mut self.cfg.display;
        let mut changed = false;
        egui::Grid::new("display").num_columns(2).spacing([24.0, 10.0]).show(ui, |ui| {
            ui.label("Fullscreen");
            changed |= ui.checkbox(&mut d.fullscreen, "").changed();
            ui.end_row();

            ui.label("Window size");
            let mut choices = vec![Resolution::Named(NamedResolution::Native)];
            choices.extend(WINDOW_SIZES.iter().map(|&(width, height)| Resolution::Fixed { width, height }));
            if !choices.contains(&d.resolution) {
                choices.push(d.resolution);
            }
            egui::ComboBox::from_id_salt("resolution").selected_text(resolution_label(&d.resolution)).show_ui(ui, |ui| {
                for r in choices {
                    changed |= ui.selectable_value(&mut d.resolution, r, resolution_label(&r)).changed();
                }
            });
            ui.end_row();

            ui.label("Aspect ratio");
            let current = ASPECTS.iter().find(|(a, _)| *a == d.aspect).map_or("?", |(_, l)| l);
            egui::ComboBox::from_id_salt("aspect").selected_text(current).show_ui(ui, |ui| {
                for &(a, label) in ASPECTS {
                    changed |= ui.selectable_value(&mut d.aspect, a, label).changed();
                }
            });
            ui.end_row();

            ui.label("Widescreen");
            changed |= ui.checkbox(&mut d.widescreen_hack, "").changed();
            ui.end_row();

            ui.label("Render scale");
            changed |= ui.add(egui::Slider::new(&mut d.render_scale, 0.5..=4.0).step_by(0.5).suffix("x")).changed();
            ui.end_row();

            ui.label("VSync");
            changed |= ui.checkbox(&mut d.vsync, "").changed();
            ui.end_row();
        });
        self.actions.display_changed |= changed;
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        ui.label("Wii Remote buttons, held sideways. Click a binding to remove it, + to add one.");
        ui.add_space(4.0);
        egui::Grid::new("bindings").num_columns(3).striped(true).spacing([16.0, 6.0]).show(ui, |ui| {
            ui.strong("Wii Remote");
            ui.strong("Keyboard");
            ui.strong("Gamepad");
            ui.end_row();
            for action in Action::ALL {
                ui.label(action_label(action));
                for device in [Device::Keyboard, Device::Gamepad] {
                    let c = &mut self.cfg.controls;
                    let list = match device {
                        Device::Keyboard => c.keyboard.entry(action.name().into()).or_default(),
                        Device::Gamepad => c.gamepad.entry(action.name().into()).or_default(),
                    };
                    let label = |n: &str| if device == Device::Keyboard { key_label(n) } else { pad_label(n) };
                    ui.horizontal(|ui| {
                        let mut remove = None;
                        for (i, name) in list.iter().enumerate() {
                            if ui.button(label(name)).on_hover_text("Click to remove").clicked() {
                                remove = Some(i);
                            }
                        }
                        if let Some(i) = remove {
                            list.remove(i);
                            changed = true;
                        }
                        let waiting = *self.capture == Some(Capture { action, device });
                        if waiting {
                            let prompt = match device {
                                Device::Keyboard => "Press a key (Esc cancels)",
                                Device::Gamepad => "Press a gamepad button (Esc cancels)",
                            };
                            ui.colored_label(egui::Color32::LIGHT_YELLOW, prompt);
                        } else if ui.small_button("+").on_hover_text("Add a binding").clicked() {
                            *self.capture = Some(Capture { action, device });
                        }
                    });
                }
                ui.end_row();
            }
        });
        ui.add_space(8.0);
        ui.separator();
        let c = &mut self.cfg.controls;
        egui::Grid::new("stick").num_columns(2).spacing([24.0, 10.0]).show(ui, |ui| {
            ui.label("Left stick");
            changed |= ui.checkbox(&mut c.stick, "Use the left stick to move (on by default)").changed();
            ui.end_row();

            ui.label("Stick dead zone");
            changed |= ui.add(egui::Slider::new(&mut c.stick_deadzone, 0.1..=0.9).step_by(0.05)).changed();
            ui.end_row();

            ui.label("Stick orientation");
            let current = StickTurn::ALL.iter().find(|(t, _)| *t == c.stick_turn).map_or("?", |(_, l)| l);
            egui::ComboBox::from_id_salt("stick_turn").selected_text(current).show_ui(ui, |ui| {
                for &(t, label) in &StickTurn::ALL {
                    changed |= ui.selectable_value(&mut c.stick_turn, t, label).changed();
                }
            });
            ui.end_row();
        });
        if self.cfg.game.multiplayer != Multiplayer::Off {
            ui.add_space(8.0);
            ui.separator();
            ui.strong("Players");
            ui.label("The keyboard is player 1. Each gamepad plays as the player set here.");
            if self.overlay.pads.is_empty() {
                ui.weak("No gamepads connected.");
            }
            let c = &mut self.cfg.controls;
            egui::Grid::new("players").num_columns(2).spacing([24.0, 6.0]).show(ui, |ui| {
                for (i, name) in self.overlay.pads.iter().enumerate() {
                    ui.label(format!("{}. {name}", i + 1));
                    let mut player = c.pad_channel(i) as u8 + 1;
                    egui::ComboBox::from_id_salt(("pad_player", i)).selected_text(format!("Player {player}")).show_ui(ui, |ui| {
                        for p in 1..=4 {
                            if ui.selectable_value(&mut player, p, format!("Player {p}")).changed() {
                                c.set_pad_player(i, p);
                                changed = true;
                            }
                        }
                    });
                    ui.end_row();
                }
            });
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Reset controls to defaults").clicked() {
                self.cfg.controls = ControlsConfig::default();
                *self.capture = None;
                changed = true;
            }
            ui.weak("Esc always opens this menu.");
        });
        self.actions.controls_changed |= changed;
    }

    fn audio(&mut self, ui: &mut egui::Ui) {
        let a = &mut self.cfg.audio;
        let mut changed = false;
        egui::Grid::new("audio").num_columns(2).spacing([24.0, 10.0]).show(ui, |ui| {
            for (label, v) in [
                ("Master volume", &mut a.master),
                ("Music", &mut a.music),
                ("Sound effects", &mut a.effects),
                ("Pokémon voices", &mut a.voices),
            ] {
                ui.label(label);
                let mut percent = (*v * 100.0).round();
                if ui.add(egui::Slider::new(&mut percent, 0.0..=100.0).step_by(1.0).suffix("%")).changed() {
                    *v = percent / 100.0;
                    changed = true;
                }
                ui.end_row();
            }

            ui.label("Output");
            ui.horizontal(|ui| {
                changed |= ui.radio_value(&mut a.mono, false, "Stereo").changed();
                changed |= ui.radio_value(&mut a.mono, true, "Mono").changed();
            });
            ui.end_row();

            ui.label("In the background");
            changed |= ui.checkbox(&mut a.mute_in_background, "Mute while the window is in the background").changed();
            ui.end_row();
        });
        self.actions.audio_changed |= changed;
    }

    /// Hosting an online session: name, port and password, then who has
    /// joined.
    fn online(&mut self, ui: &mut egui::Ui, changed: &mut bool) {
        let o = &mut self.cfg.online;
        let hosting = self.overlay.hosting.is_some();
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.add_enabled_ui(!hosting, |ui| {
                egui::Grid::new("online").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                    ui.label("Your name");
                    *changed |= ui.add(egui::TextEdit::singleline(&mut o.name).char_limit(crate::net::MAX_NAME).hint_text("Shown to other players")).changed();
                    ui.end_row();
                    ui.label("Port");
                    *changed |= ui.add(egui::DragValue::new(&mut o.port).range(1024..=65535)).changed();
                    ui.end_row();
                    ui.label("Password");
                    ui.add(egui::TextEdit::singleline(&mut self.overlay.password).password(true).hint_text("Players need it to join"));
                    ui.end_row();
                });
            });
            ui.add_space(4.0);
            match &self.overlay.hosting {
                None => {
                    let ready = !crate::net::clean_name(&o.name).is_empty() && !self.overlay.password.is_empty();
                    if ui.add_enabled(ready, egui::Button::new("Start hosting")).clicked() {
                        self.actions.start_hosting = true;
                    }
                    if !ready {
                        ui.small("Set your name and a password to host.");
                    }
                    if let Some(e) = &self.overlay.host_error {
                        ui.colored_label(egui::Color32::LIGHT_RED, e);
                    }
                }
                Some(h) => {
                    if ui.button("Stop hosting").clicked() {
                        self.actions.stop_hosting = true;
                    }
                    let lan = h.lan.map_or("this PC's address".to_string(), |ip| ip.to_string());
                    ui.label(format!("Hosting on port {}. On your network, friends join at {lan}:{}.", h.port, h.port));
                    match &h.port_opening {
                        PortOpening::Trying => {
                            ui.small("Opening the port on your router…");
                        }
                        PortOpening::Open(Some(public)) => {
                            ui.label(format!("Port opened on your router. From elsewhere, friends join at {public}:{}.", h.port));
                        }
                        PortOpening::Open(None) => {
                            ui.small(format!("Port opened on your router. From elsewhere, friends use your public IP address with port {}.", h.port));
                        }
                        PortOpening::Failed => {
                            ui.small(format!(
                                "Couldn't open the port on your router automatically (UPnP is off or unsupported). For friends elsewhere, forward TCP port {} to {lan} in your router's settings; they join at your public IP address with port {}.",
                                h.port, h.port
                            ));
                        }
                    }
                    ui.add_space(4.0);
                    for (player, name) in &h.players {
                        ui.label(format!("Player {player}: {name}"));
                    }
                    if h.players.len() < 2 {
                        ui.colored_label(egui::Color32::LIGHT_YELLOW, "Waiting for players…");
                    }
                }
            }
        });
    }

    fn game(&mut self, ui: &mut egui::Ui) {
        let g = &mut self.cfg.game;
        let mut changed = ui.checkbox(&mut g.pause_in_background, "Pause while the window is in the background").changed();
        ui.small("Input is ignored in the background either way.");
        ui.add_space(8.0);
        changed |= ui.checkbox(&mut g.threaded_graphics, "Build frames on a separate thread").changed();
        ui.small("Faster, so the game and its sound keep up in busy scenes. Turn off if something looks wrong. Takes effect when the game is restarted.");
        ui.add_space(8.0);
        changed |= ui.checkbox(&mut g.show_fps, "Show FPS counter").changed();
        ui.small("The game's frames per second, in the top right corner.");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("Multiplayer");
            changed |= ui.radio_value(&mut g.multiplayer, Multiplayer::Off, "Off").changed();
            changed |= ui.radio_value(&mut g.multiplayer, Multiplayer::Local, "Local").changed();
            changed |= ui.radio_value(&mut g.multiplayer, Multiplayer::Online, "Online").changed();
        });
        ui.small("Local: extra gamepads play as players 2-4 (choose who plays as whom under Controls). Online: friends join over the network too. The hub's multiplayer pedestal comes back when the game is restarted.");
        if g.multiplayer == Multiplayer::Online {
            ui.add_space(4.0);
            self.online(ui, &mut changed);
        }
        if self.cfg.game.multiplayer != Multiplayer::Off {
            ui.add_space(4.0);
            if self.overlay.ending_multiplayer {
                ui.colored_label(egui::Color32::LIGHT_YELLOW, "Ending multiplayer: open the game's pause menu (+), choose Multiplayer, and pick your Pokémon. Players 2-4 are removed there.");
            } else if ui.button("End multiplayer").on_hover_text("Continue alone: players 2-4 are removed at the next Pokémon selection").clicked() {
                self.actions.end_multiplayer = true;
            }
        }
        self.actions.game_changed |= changed;

        ui.add_space(16.0);
        ui.horizontal(|ui| {
            self.actions.import_save |= ui.button("Import save (data.bin)…").clicked();
            self.actions.export_save |= ui.button("Export save (data.bin)…").clicked();
        });
        ui.small("A save copied to an SD card by a Wii (Data Management), or exported by Dolphin. Importing backs up the current save to save-backups next to it and restarts the game.");
        if let Some((msg, failed)) = self.save_status {
            if *failed {
                ui.colored_label(egui::Color32::LIGHT_RED, msg);
            } else {
                ui.colored_label(egui::Color32::LIGHT_GREEN, msg);
            }
        }
    }
}
