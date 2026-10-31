//! Mod discovery and application.
//!
//! A mod is a folder in `mods/` with a `mod.toml`:
//!
//! ```toml
//! name = "Weekend Edition"
//! version = "1.5"
//! # Alternative BPS patches for the base WAD; the first one whose source
//! # checksum matches the current WAD is applied.
//! wad_patches = ["RW 1.5.bps", "RW 1.5 alt.bps"]
//! ```
//!
//! Loose files under `<mod>/files/` replace or add files in the extracted
//! game filesystem, applied in load order (later mods win).

use anyhow::{bail, Context, Result};
use rumble_formats::bps;
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub wad_patches: Vec<String>,
}

#[derive(Debug)]
pub struct Mod {
    pub id: String,
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl Mod {
    pub fn files_dir(&self) -> Option<PathBuf> {
        let d = self.dir.join("files");
        d.is_dir().then_some(d)
    }

    /// Applies this mod's WAD patch, choosing the variant that matches `wad`.
    /// Returns the input unchanged if the mod has no WAD patches.
    pub fn patch_wad(&self, wad: Vec<u8>) -> Result<Vec<u8>> {
        if self.manifest.wad_patches.is_empty() {
            return Ok(wad);
        }
        let crc = crc32fast::hash(&wad);
        for name in &self.manifest.wad_patches {
            let path = self.dir.join(name);
            let patch = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let info = bps::info(&patch).with_context(|| format!("parsing {}", path.display()))?;
            if bps::matches_source(&info, &wad) {
                println!("  [{}] applying {name}", self.id);
                return bps::apply(&patch, &wad).with_context(|| format!("applying {name}"));
            }
        }
        bail!(
            "mod '{}': none of its patches match this WAD (size {}, crc32 {crc:08x}); \
             it may need a different dump or a different load order",
            self.id,
            wad.len()
        )
    }
}

/// Loads the mods named in `enabled`, in that order.
pub fn load(mods_dir: &Path, enabled: &[String]) -> Result<Vec<Mod>> {
    enabled
        .iter()
        .map(|id| {
            let dir = mods_dir.join(id);
            let text = std::fs::read_to_string(dir.join("mod.toml"))
                .with_context(|| format!("mod '{id}': missing {}/mod.toml", dir.display()))?;
            let manifest = toml::from_str(&text).with_context(|| format!("mod '{id}': bad mod.toml"))?;
            Ok(Mod { id: id.clone(), dir, manifest })
        })
        .collect()
}

/// Every mod folder present, enabled or not.
pub fn discover(mods_dir: &Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(mods_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("mod.toml").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort();
    ids
}

/// Copies a mod's loose files over the extracted filesystem.
pub fn overlay(src: &Path, dst: &Path) -> Result<usize> {
    let mut n = 0;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)?;
            n += overlay(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
            n += 1;
        }
    }
    Ok(n)
}
