//! Keyboard and gamepad input as Wii Remote buttons, through bindings the
//! player can change in the menu (saved in the config's `[controls]`).
//! Pokémon Rumble is played with the remote held sideways (D-pad on the
//! left), so directions are bound as seen that way: "Left" sends the
//! remote's Up.
//!
//! Default bindings:
//!
//! | Remote   | Keyboard              | Gamepad (Xbox / PlayStation)   |
//! |----------|-----------------------|--------------------------------|
//! | D-pad    | arrows, WASD          | D-pad, left stick              |
//! | 2        | X, K                  | A / Cross                      |
//! | 1        | Z, J                  | B / Circle                     |
//! | A        | Enter, Space          | X / Square                     |
//! | B        | Backspace, Left Shift | Y / Triangle                   |
//! | + / -    | = / -                 | Start / Back (Select)          |
//! | Home     | H                     | Guide                          |

use rumble_runtime::input::{button as b, CHANNELS};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use winit::keyboard::KeyCode;

/// A Wii Remote input the player binds keys and buttons to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Action {
    Up,
    Down,
    Left,
    Right,
    A,
    B,
    One,
    Two,
    Plus,
    Minus,
    Home,
}

impl Action {
    pub const ALL: [Action; 11] = [
        Action::Up,
        Action::Down,
        Action::Left,
        Action::Right,
        Action::A,
        Action::B,
        Action::One,
        Action::Two,
        Action::Plus,
        Action::Minus,
        Action::Home,
    ];

    /// The name in the config and the menu.
    pub fn name(self) -> &'static str {
        match self {
            Action::Up => "Up",
            Action::Down => "Down",
            Action::Left => "Left",
            Action::Right => "Right",
            Action::A => "A",
            Action::B => "B",
            Action::One => "1",
            Action::Two => "2",
            Action::Plus => "+",
            Action::Minus => "-",
            Action::Home => "Home",
        }
    }

    fn from_name(name: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.name() == name)
    }

    /// The remote buttons it presses: screen directions with the remote
    /// held sideways.
    pub fn bits(self) -> u32 {
        match self {
            Action::Up => b::RIGHT,
            Action::Down => b::LEFT,
            Action::Left => b::UP,
            Action::Right => b::DOWN,
            Action::A => b::A,
            Action::B => b::B,
            Action::One => b::ONE,
            Action::Two => b::TWO,
            Action::Plus => b::PLUS,
            Action::Minus => b::MINUS,
            Action::Home => b::HOME,
        }
    }
}

/// Keys that can be bound, by the names the config uses.
const KEYS: &[KeyCode] = {
    use KeyCode as K;
    &[
        K::KeyA, K::KeyB, K::KeyC, K::KeyD, K::KeyE, K::KeyF, K::KeyG, K::KeyH, K::KeyI, K::KeyJ, K::KeyK, K::KeyL,
        K::KeyM, K::KeyN, K::KeyO, K::KeyP, K::KeyQ, K::KeyR, K::KeyS, K::KeyT, K::KeyU, K::KeyV, K::KeyW, K::KeyX,
        K::KeyY, K::KeyZ, K::Digit0, K::Digit1, K::Digit2, K::Digit3, K::Digit4, K::Digit5, K::Digit6, K::Digit7,
        K::Digit8, K::Digit9, K::ArrowUp, K::ArrowDown, K::ArrowLeft, K::ArrowRight, K::Space, K::Enter, K::Tab,
        K::Backspace, K::ShiftLeft, K::ShiftRight, K::ControlLeft, K::ControlRight, K::AltLeft, K::AltRight,
        K::Minus, K::Equal, K::BracketLeft, K::BracketRight, K::Backslash, K::Semicolon, K::Quote, K::Comma,
        K::Period, K::Slash, K::Backquote, K::Insert, K::Delete, K::Home, K::End, K::PageUp, K::PageDown,
        K::Numpad0, K::Numpad1, K::Numpad2, K::Numpad3, K::Numpad4, K::Numpad5, K::Numpad6, K::Numpad7, K::Numpad8,
        K::Numpad9, K::NumpadAdd, K::NumpadSubtract, K::NumpadMultiply, K::NumpadDivide, K::NumpadEnter,
        K::NumpadDecimal, K::F1, K::F2, K::F3, K::F4, K::F5, K::F6, K::F7, K::F8, K::F9, K::F10, K::F11, K::F12,
    ]
};

pub fn key_name(code: KeyCode) -> String {
    format!("{code:?}")
}

pub fn key_from_name(name: &str) -> Option<KeyCode> {
    KEYS.iter().copied().find(|&k| key_name(k) == name)
}

/// Whether a key can be bound (Esc opens the menu).
pub fn bindable_key(code: KeyCode) -> bool {
    KEYS.contains(&code)
}

/// Gamepad buttons that can be bound.
const PAD_BUTTONS: &[gilrs::Button] = {
    use gilrs::Button as G;
    &[
        G::South, G::East, G::West, G::North, G::LeftTrigger, G::RightTrigger, G::LeftTrigger2, G::RightTrigger2,
        G::Select, G::Start, G::Mode, G::LeftThumb, G::RightThumb, G::DPadUp, G::DPadDown, G::DPadLeft, G::DPadRight,
    ]
};

pub fn pad_name(button: gilrs::Button) -> String {
    format!("{button:?}")
}

fn pad_from_name(name: &str) -> Option<gilrs::Button> {
    PAD_BUTTONS.iter().copied().find(|&p| pad_name(p) == name)
}

/// How far the left stick must be pushed (of its full reach, as a circle) to
/// count, and how much further it must come back to stop, so it doesn't
/// flicker at the edge.
const STICK_DEADZONE: f32 = 0.3;
const STICK_RELEASE: f32 = 0.22;
/// Past a direction's 45-degree sector by up to this (radians), the stick
/// keeps that direction, so it doesn't flicker between neighbours.
const STICK_HYSTERESIS: f32 = 0.12;

/// The left stick as one of eight directions (0: right, then
/// counter-clockwise in 45-degree steps), or none at rest. `last` is the
/// direction it gave before.
fn stick_direction([x, y]: [f32; 2], last: Option<u8>) -> Option<u8> {
    use std::f32::consts::{FRAC_PI_4, FRAC_PI_8, TAU};
    let reach = x.hypot(y);
    if reach < if last.is_some() { STICK_RELEASE } else { STICK_DEADZONE } {
        return None;
    }
    let angle = y.atan2(x).rem_euclid(TAU);
    if let Some(l) = last {
        let off = (angle - l as f32 * FRAC_PI_4 + TAU + std::f32::consts::PI).rem_euclid(TAU) - std::f32::consts::PI;
        if off.abs() <= FRAC_PI_8 + STICK_HYSTERESIS {
            return Some(l);
        }
    }
    Some(((angle / FRAC_PI_4).round() as u8) % 8)
}

/// The remote buttons for one of `stick_direction`'s directions.
fn direction_bits(dir: u8) -> u32 {
    const DIRS: [&[Action]; 8] = [
        &[Action::Right],
        &[Action::Right, Action::Up],
        &[Action::Up],
        &[Action::Left, Action::Up],
        &[Action::Left],
        &[Action::Left, Action::Down],
        &[Action::Down],
        &[Action::Right, Action::Down],
    ];
    DIRS[dir as usize % 8].iter().fold(0, |acc, a| acc | a.bits())
}

/// The `[controls]` config section: per action, the keys and gamepad
/// buttons bound to it (by name), and how the stick is read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlsConfig {
    pub keyboard: BTreeMap<String, Vec<String>>,
    pub gamepad: BTreeMap<String, Vec<String>>,
    /// "True stick controls": the left stick steers in any direction, at
    /// any speed, as the game's own Classic Controller support does,
    /// instead of acting as the D-pad's eight directions.
    pub true_stick: bool,
    /// With local multiplayer: the player (1-4) each gamepad plays as, in
    /// the order they are listed. A pad without an entry plays as its
    /// place in that order (the first is player 1, with the keyboard).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pad_players: Vec<u8>,
}

impl Default for ControlsConfig {
    fn default() -> Self {
        use gilrs::Button as G;
        use KeyCode as K;
        let keys: &[(Action, &[KeyCode])] = &[
            (Action::Up, &[K::ArrowUp, K::KeyW]),
            (Action::Down, &[K::ArrowDown, K::KeyS]),
            (Action::Left, &[K::ArrowLeft, K::KeyA]),
            (Action::Right, &[K::ArrowRight, K::KeyD]),
            (Action::Two, &[K::KeyX, K::KeyK]),
            (Action::One, &[K::KeyZ, K::KeyJ]),
            (Action::A, &[K::Enter, K::Space]),
            (Action::B, &[K::Backspace, K::ShiftLeft]),
            (Action::Plus, &[K::Equal, K::NumpadAdd]),
            (Action::Minus, &[K::Minus, K::NumpadSubtract]),
            (Action::Home, &[K::KeyH]),
        ];
        let pad: &[(Action, &[gilrs::Button])] = &[
            (Action::Up, &[G::DPadUp]),
            (Action::Down, &[G::DPadDown]),
            (Action::Left, &[G::DPadLeft]),
            (Action::Right, &[G::DPadRight]),
            (Action::Two, &[G::South]),
            (Action::One, &[G::East]),
            (Action::A, &[G::West]),
            (Action::B, &[G::North]),
            (Action::Plus, &[G::Start]),
            (Action::Minus, &[G::Select]),
            (Action::Home, &[G::Mode]),
        ];
        Self {
            keyboard: keys.iter().map(|(a, ks)| (a.name().into(), ks.iter().map(|&k| key_name(k)).collect())).collect(),
            gamepad: pad.iter().map(|(a, ps)| (a.name().into(), ps.iter().map(|&p| pad_name(p)).collect())).collect(),
            true_stick: false,
            pad_players: Vec::new(),
        }
    }
}

impl ControlsConfig {
    /// Binds `key` to `action`, taking it off any other action.
    pub fn bind_key(&mut self, action: Action, key: KeyCode) {
        let name = key_name(key);
        for keys in self.keyboard.values_mut() {
            keys.retain(|k| k != &name);
        }
        self.keyboard.entry(action.name().into()).or_default().push(name);
    }

    /// The Wii Remote channel (0-3) the `index`th gamepad plays on.
    pub fn pad_channel(&self, index: usize) -> usize {
        let player = self.pad_players.get(index).copied().map_or(index + 1, usize::from);
        player.clamp(1, CHANNELS) - 1
    }

    /// Makes the `index`th gamepad play as `player` (1-4).
    pub fn set_pad_player(&mut self, index: usize, player: u8) {
        while self.pad_players.len() <= index {
            self.pad_players.push((self.pad_players.len() + 1).min(CHANNELS) as u8);
        }
        self.pad_players[index] = player;
    }

    /// Binds `button` to `action`, taking it off any other action.
    pub fn bind_pad(&mut self, action: Action, button: gilrs::Button) {
        let name = pad_name(button);
        for buttons in self.gamepad.values_mut() {
            buttons.retain(|b| b != &name);
        }
        self.gamepad.entry(action.name().into()).or_default().push(name);
    }
}

/// The bindings in a form quick to look up.
pub struct Bindings {
    keys: Vec<(KeyCode, u32)>,
    pad: Vec<(gilrs::Button, u32)>,
    /// The stick gives directions (else it is read as analog).
    stick_directions: bool,
}

impl Bindings {
    pub fn new(c: &ControlsConfig) -> Self {
        let mut keys = Vec::new();
        for (action, names) in &c.keyboard {
            let Some(a) = Action::from_name(action) else { continue };
            keys.extend(names.iter().filter_map(|n| key_from_name(n)).map(|k| (k, a.bits())));
        }
        let mut pad = Vec::new();
        for (action, names) in &c.gamepad {
            let Some(a) = Action::from_name(action) else { continue };
            pad.extend(names.iter().filter_map(|n| pad_from_name(n)).map(|p| (p, a.bits())));
        }
        Self { keys, pad, stick_directions: !c.true_stick }
    }

    /// Remote buttons a key presses.
    pub fn key(&self, code: KeyCode) -> u32 {
        self.keys.iter().filter(|(k, _)| *k == code).fold(0, |acc, (_, bits)| acc | bits)
    }
}

/// Direction axes that have been seen at rest. Pads Windows doesn't know as
/// Xbox-style ones have their axes mapped by index, so a trigger (resting at
/// -1 or +1) can land on the stick or D-pad axes and read as a direction held
/// forever. An axis counts only once it has read near zero; a real stick
/// does at once, a trigger never does until it is pressed halfway.
#[derive(Default)]
struct CentredAxes(HashSet<(usize, gilrs::Axis)>);

impl CentredAxes {
    /// The axis value, or 0 if the axis hasn't been seen within `deadzone`
    /// of zero yet.
    fn read(&mut self, pad: usize, axis: gilrs::Axis, value: f32, deadzone: f32) -> f32 {
        if self.0.contains(&(pad, axis)) {
            return value;
        }
        if value.abs() < deadzone {
            self.0.insert((pad, axis));
        }
        0.0
    }

    fn forget(&mut self, pad: usize) {
        self.0.retain(|&(p, _)| p != pad);
    }
}

/// Connected gamepads, polled each frame.
pub struct Gamepads {
    gilrs: Option<gilrs::Gilrs>,
    /// The last button pressed since `take_pressed`, for binding it.
    pressed: Option<gilrs::Button>,
    centred: CentredAxes,
    /// Each pad's stick direction last time (`stick_direction`).
    directions: std::collections::HashMap<usize, u8>,
}

/// A stick for analog movement: nothing within a small circle round the
/// centre (where a resting stick drifts), then the rest of its reach
/// stretched to 0-1, in the same direction.
pub fn analog_stick([x, y]: [f32; 2]) -> [f32; 2] {
    const DEAD: f32 = 0.15;
    let reach = x.hypot(y);
    if reach <= DEAD {
        return [0.0; 2];
    }
    let k = ((reach - DEAD) / (1.0 - DEAD)).min(1.0) / reach;
    [x * k, y * k]
}

/// Of several pads' sticks, the one pushed furthest.
pub fn strongest(sticks: impl IntoIterator<Item = [f32; 2]>) -> [f32; 2] {
    sticks.into_iter().fold([0.0; 2], |a, s| if s[0].hypot(s[1]) > a[0].hypot(a[1]) { s } else { a })
}

/// A gamepad as read: its name, the remote buttons it holds, and its left
/// stick (-1 to 1, y up).
pub struct Pad {
    pub name: String,
    pub buttons: u32,
    pub stick: [f32; 2],
}

impl Gamepads {
    pub fn new() -> Self {
        let gilrs = match gilrs::Gilrs::new() {
            Ok(g) => {
                for (_, pad) in g.gamepads() {
                    println!("gamepad: {} ({:?})", pad.name(), pad.mapping_source());
                }
                Some(g)
            }
            Err(e) => {
                eprintln!("gamepads unavailable: {e}");
                None
            }
        };
        Self { gilrs, pressed: None, centred: CentredAxes::default(), directions: Default::default() }
    }

    /// The gamepad button pressed most recently since the last call.
    pub fn take_pressed(&mut self) -> Option<gilrs::Button> {
        self.poll();
        self.pressed.take()
    }

    fn poll(&mut self) {
        let Some(gilrs) = self.gilrs.as_mut() else { return };
        // Events update the state gilrs reports.
        while let Some(e) = gilrs.next_event() {
            match e.event {
                gilrs::EventType::ButtonPressed(button, _) if PAD_BUTTONS.contains(&button) => {
                    self.pressed = Some(button);
                }
                // A reconnected pad starts over: its axes may map differently.
                gilrs::EventType::Connected | gilrs::EventType::Disconnected => {
                    self.centred.forget(e.id.into());
                }
                _ => {}
            }
        }
    }

    /// Each connected gamepad's name, the remote buttons it holds and its
    /// left stick (-1 to 1, y up), in a steady order (by when gilrs first
    /// saw it). With the stick giving directions, it holds those too.
    pub fn pads(&mut self, bindings: &Bindings) -> Vec<Pad> {
        use gilrs::Axis;
        self.poll();
        let Some(gilrs) = self.gilrs.as_ref() else { return Vec::new() };
        let mut pads = Vec::new();
        for (id, pad) in gilrs.gamepads() {
            let id = usize::from(id);
            let mut bits = 0;
            let mut axis = |a: Axis, deadzone| self.centred.read(id, a, pad.value(a), deadzone);
            for &(button, bit) in &bindings.pad {
                if pad.is_pressed(button) {
                    bits |= bit;
                }
            }
            // Some pads report the D-pad as an axis pair rather than as
            // buttons; read it like the D-pad buttons' default bindings.
            let (dx, dy) = (axis(Axis::DPadX, 0.5), axis(Axis::DPadY, 0.5));
            for (on, a) in [(dx < -0.5, Action::Left), (dx > 0.5, Action::Right), (dy > 0.5, Action::Up), (dy < -0.5, Action::Down)] {
                if on {
                    bits |= a.bits();
                }
            }
            let stick = [axis(Axis::LeftStickX, STICK_DEADZONE), axis(Axis::LeftStickY, STICK_DEADZONE)];
            if bindings.stick_directions {
                match stick_direction(stick, self.directions.get(&id).copied()) {
                    Some(d) => {
                        self.directions.insert(id, d);
                        bits |= direction_bits(d);
                    }
                    None => {
                        self.directions.remove(&id);
                    }
                }
            }
            pads.push(Pad { name: pad.name().to_string(), buttons: bits, stick });
        }
        pads
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bindings_are_sideways() {
        let b = Bindings::new(&ControlsConfig::default());
        assert_eq!(b.key(KeyCode::ArrowLeft), b::UP);
        assert_eq!(b.key(KeyCode::ArrowUp), b::RIGHT);
        assert_eq!(b.key(KeyCode::Enter), b::A);
        assert_eq!(b.key(KeyCode::KeyX), b::TWO);
        assert_eq!(b.key(KeyCode::F1), 0);
    }

    /// Binding a key moves it off its old action; the config round-trips.
    #[test]
    fn rebinding_moves_a_key() {
        let mut c = ControlsConfig::default();
        c.bind_key(Action::B, KeyCode::KeyX);
        let b = Bindings::new(&c);
        assert_eq!(b.key(KeyCode::KeyX), b::B);
        let text = toml::to_string(&c).unwrap();
        assert_eq!(toml::from_str::<ControlsConfig>(&text).unwrap(), c);
    }

    /// Pads play as their place in the list unless set otherwise; players
    /// past 4 can't happen.
    #[test]
    fn pad_players() {
        let mut c = ControlsConfig::default();
        assert_eq!((0..5).map(|i| c.pad_channel(i)).collect::<Vec<_>>(), [0, 1, 2, 3, 3]);
        c.set_pad_player(2, 1);
        assert_eq!(c.pad_players, [1, 2, 1]);
        assert_eq!(c.pad_channel(2), 0);
        assert_eq!(c.pad_channel(3), 3);
    }

    /// The stick gives eight equal directions from a third of its reach,
    /// and holds a direction a little past its edge.
    #[test]
    fn stick_directions() {
        assert_eq!(stick_direction([0.2, 0.0], None), None);
        assert_eq!(stick_direction([1.0, 0.0], None), Some(0));
        assert_eq!(stick_direction([0.0, 1.0], None), Some(2));
        assert_eq!(stick_direction([-0.7, -0.7], None), Some(5));
        // 25 degrees: nearer the diagonal edge than right's centre.
        let (s, c) = 25f32.to_radians().sin_cos();
        assert_eq!(stick_direction([c, s], None), Some(1));
        assert_eq!(stick_direction([c, s], Some(0)), Some(0), "held a little past the edge");
        assert_eq!(stick_direction([0.25, 0.0], Some(0)), Some(0), "released further in");
        assert_eq!(stick_direction([0.1, 0.0], Some(0)), None);
        assert_eq!(direction_bits(1), Action::Right.bits() | Action::Up.bits());
    }

    /// A trigger mapped onto a stick axis rests at -1 or +1; it mustn't read
    /// as a held direction until it has been seen at centre.
    #[test]
    fn axes_count_only_after_resting_at_centre() {
        use gilrs::Axis;
        let mut c = CentredAxes::default();
        for _ in 0..3 {
            assert_eq!(c.read(0, Axis::LeftStickX, 1.0, 0.5), 0.0);
            assert_eq!(c.read(0, Axis::LeftStickY, -1.0, 0.5), 0.0);
        }
        // A real stick reads near zero at once, then moves freely.
        assert_eq!(c.read(0, Axis::LeftStickX, 0.02, 0.5), 0.0);
        assert_eq!(c.read(0, Axis::LeftStickX, -0.9, 0.5), -0.9);
        // Still stuck, and per pad.
        assert_eq!(c.read(0, Axis::LeftStickY, -1.0, 0.5), 0.0);
        assert_eq!(c.read(1, Axis::LeftStickX, 1.0, 0.5), 0.0);
        c.forget(0);
        assert_eq!(c.read(0, Axis::LeftStickX, -0.9, 0.5), 0.0);
    }
}
