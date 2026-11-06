use crate::audio::AudioConfig;
use crate::controls::ControlsConfig;
use crate::display::DisplayConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub paths: Paths,
    pub display: DisplayConfig,
    pub controls: ControlsConfig,
    pub audio: AudioConfig,
    pub game: GameConfig,
    pub mods: ModsConfig,
    pub addons: AddonsConfig,
    pub online: OnlineConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GameConfig {
    /// Pause while the window is in the background.
    pub pause_in_background: bool,
    /// Build frames on a thread of their own (`gx::worker`), which keeps
    /// the game (and its sound) up to speed in busy scenes. Read at start.
    pub threaded_graphics: bool,
    /// Show the game's frames per second in the top right corner.
    pub show_fps: bool,
    /// Multiplayer: off, local (extra gamepads play as players 2-4) or
    /// online (players join over the network too).
    /// With it on, the hub has the game's own multiplayer pedestal and the
    /// pause menu its Multiplayer entry. Read at start.
    pub multiplayer: Multiplayer,
    /// Keep the hub's original multiplayer pedestal and its prompt instead
    /// of Ditto (`docs/multiplayer-ditto.md`) even with multiplayer off.
    /// The menu doesn't show it. Read at start.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub original_multiplayer: bool,
}

/// How other players join.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Multiplayer {
    #[default]
    Off,
    /// Players 2-4 on this PC's gamepads.
    Local,
    /// Players 2-4 on this PC's gamepads or joining over the network.
    Online,
}

/// Online play: this player's name, the port a hosted session listens
/// on, and for joining, the last host and the picture height asked for.
/// Passwords are asked each time and never saved.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OnlineConfig {
    pub name: String,
    pub port: u16,
    pub last_host: String,
    pub quality: u16,
}

impl Default for OnlineConfig {
    fn default() -> Self {
        Self { name: String::new(), port: crate::net::DEFAULT_PORT, last_host: String::new(), quality: 1080 }
    }
}

impl Default for GameConfig {
    fn default() -> Self {
        Self {
            pause_in_background: false,
            threaded_graphics: true,
            show_fps: false,
            multiplayer: Multiplayer::Off,
            original_multiplayer: false,
        }
    }
}

impl GameConfig {
    /// Whether the game keeps its own multiplayer pedestal and pause entry.
    pub fn original_pedestal(&self) -> bool {
        self.original_multiplayer || self.multiplayer != Multiplayer::Off
    }
}

/// Optional changes picked in setup, for Rumble Weekend builds. Read at
/// start.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AddonsConfig {
    /// Shinies sparkle with stars, in yellow and cyan.
    pub custom_sparkle: bool,
}

impl AddonsConfig {
    pub fn runtime(&self) -> rumble_runtime::hle::addons::Addons {
        rumble_runtime::hle::addons::Addons { custom_sparkle: self.custom_sparkle }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Paths {
    /// The user's own clean USA WAD dump.
    pub wad: PathBuf,
    /// 16-byte Wii common key dumped from the user's console.
    pub common_key: PathBuf,
    pub mods_dir: PathBuf,
    pub build_dir: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModsConfig {
    /// Mod folder names, applied in this order.
    pub enabled: Vec<String>,
    /// Extra BPS patches the setup app applies, in order, after the mods.
    pub patches: Vec<PathBuf>,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            wad: "Pokemon Rumble (USA) (WiiWare).wad".into(),
            common_key: "keys/common-key".into(),
            mods_dir: "mods".into(),
            build_dir: "build".into(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            paths: Paths::default(),
            display: DisplayConfig::default(),
            controls: ControlsConfig::default(),
            audio: AudioConfig::default(),
            game: GameConfig::default(),
            mods: ModsConfig::default(),
            addons: AddonsConfig::default(),
            online: OnlineConfig::default(),
        }
    }
}

impl Config {
    /// Loads `path`, writing a default config there if it does not exist.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if !path.exists() {
            let cfg = Config::default();
            std::fs::write(path, toml::to_string_pretty(&cfg)?)
                .with_context(|| format!("writing {}", path.display()))?;
            println!("created default {}", path.display());
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, toml::to_string_pretty(self)?).with_context(|| format!("writing {}", path.display()))
    }
}
