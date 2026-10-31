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
    /// Keep the hub's original multiplayer pedestal and its prompt instead
    /// of Ditto (`docs/multiplayer-ditto.md`). For whoever implements
    /// multiplayer; the menu doesn't show it. Read at start.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub original_multiplayer: bool,
}

impl Default for GameConfig {
    fn default() -> Self {
        Self { pause_in_background: false, threaded_graphics: true, show_fps: false, original_multiplayer: false }
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
