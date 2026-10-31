//! Importing and exporting the game's save as a Wii SD-card `data.bin`
//! (`rumble_formats::wiisave`), from the menu's Game tab.
//!
//! An import is checked whole before anything is touched. The emulation
//! thread applies it between runs, when no game is using the NAND: the
//! current save folder is copied to `build/save-backups/<time>/` first,
//! then replaced (keeping `banner.bin`), and the game boots again. An
//! export only reads the save folder.

use crate::config::Config;
use anyhow::{bail, Context, Result};
use rumble_formats::wiisave::{self, Device, Keys, Kind, Save, SaveFile};
use std::path::{Path, PathBuf};

/// Pokémon Rumble (USA).
pub const TITLE_ID: u64 = 0x0001_0001_5750_5345;

/// The game's save folder in the NAND `run` uses.
pub fn data_dir(build_dir: &Path) -> PathBuf {
    build_dir.join("nand/title/00010001/57505345/data")
}

fn keys_dir(cfg: &Config) -> PathBuf {
    cfg.paths.common_key.parent().map_or_else(|| PathBuf::from("keys"), Path::to_path_buf)
}

fn key16(dir: &Path, name: &str) -> Result<[u8; 16]> {
    let path = dir.join(name);
    let b = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    b.try_into().map_err(|_| anyhow::anyhow!("{} must be exactly 16 bytes", path.display()))
}

/// The SD card keys, which are the same on every Wii.
const SD_KEYS: Keys = Keys {
    sd_key: [0xab, 0x01, 0xb9, 0xd8, 0xe1, 0x62, 0x2b, 0x08, 0xaf, 0xba, 0xd8, 0x4d, 0xbf, 0xc2, 0xa5, 0x5d],
    sd_iv: [0x21, 0x67, 0x12, 0xe6, 0xaa, 0x1f, 0x68, 0x9f, 0x95, 0xc5, 0xa2, 0x23, 0x24, 0xdc, 0x6a, 0x98],
    md5_blanker: [0x0e, 0x65, 0x37, 0x81, 0x99, 0xbe, 0x45, 0x17, 0xab, 0x06, 0xec, 0x22, 0x45, 0x1a, 0x57, 0x93],
};

/// The SD card keys: the files next to the common key if they are there,
/// else the built-in ones.
pub fn keys(cfg: &Config) -> Result<Keys> {
    let dir = keys_dir(cfg);
    if !dir.join("sd-key").exists() {
        return Ok(SD_KEYS);
    }
    Ok(Keys { sd_key: key16(&dir, "sd-key")?, sd_iv: key16(&dir, "sd-iv")?, md5_blanker: key16(&dir, "md5-blanker")? })
}

/// The console to sign exports as: a BootMii `keys.bin` next to the common
/// key if there is one, else Dolphin's default console.
fn device(cfg: &Config) -> Result<(Device, &'static str)> {
    let path = keys_dir(cfg).join("keys.bin");
    match std::fs::read(&path) {
        Ok(b) => Ok((Device::from_keys_bin(&b).with_context(|| format!("reading {}", path.display()))?, "your console's keys.bin")),
        Err(_) => Ok((Device::default(), "Dolphin's default console")),
    }
}

/// Reads and checks a data.bin for import.
pub fn read_import(cfg: &Config, path: &Path) -> Result<Save> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let save = wiisave::parse(&data, &keys(cfg)?, TITLE_ID).map_err(|e| match e {
        wiisave::SaveError::WrongTitle { found, .. } if found.starts_with("WPS") => anyhow::anyhow!(
            "this is a save for {found} (another region's Pokémon Rumble); only USA (WPSE) saves load here"
        ),
        e => e.into(),
    })?;
    if !save.files.iter().any(|f| f.path == "savedata.bin" && matches!(f.kind, Kind::File(_))) {
        bail!("the save has no savedata.bin");
    }
    Ok(save)
}

/// A folder name for now, in UTC: 2026-10-30_14-05-09.
fn timestamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02}_{:02}-{:02}-{:02}", rem / 3600, rem / 60 % 60, rem % 60)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), &target)?;
        }
    }
    Ok(())
}

/// Backs up the save folder, then replaces its contents with `save`'s
/// (keeping banner.bin). Returns the backup's path.
pub fn apply_import(build_dir: &Path, save: &Save) -> Result<PathBuf> {
    let dir = data_dir(build_dir);
    let backups = build_dir.join("save-backups");
    let stamp = timestamp();
    let mut backup = backups.join(&stamp);
    let mut n = 1;
    while backup.exists() {
        n += 1;
        backup = backups.join(format!("{stamp}-{n}"));
    }
    if dir.exists() {
        copy_dir(&dir, &backup).with_context(|| format!("backing up the save to {}", backup.display()))?;
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            if e.file_name() == "banner.bin" {
                continue;
            }
            if e.file_type()?.is_dir() {
                std::fs::remove_dir_all(e.path())?;
            } else {
                std::fs::remove_file(e.path())?;
            }
        }
    }
    std::fs::create_dir_all(&dir)?;
    for f in &save.files {
        let path = dir.join(&f.path);
        match &f.kind {
            Kind::Dir => std::fs::create_dir_all(&path)?,
            Kind::File(data) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, data).with_context(|| format!("writing {}", path.display()))?;
            }
        }
    }
    if !dir.join("banner.bin").exists() {
        std::fs::write(dir.join("banner.bin"), &save.banner)?;
    }
    Ok(backup)
}

/// The save folder as a `Save`: banner.bin as the banner, everything else
/// as files and folders, each folder before its contents.
fn read_save(dir: &Path) -> Result<Save> {
    let banner = std::fs::read(dir.join("banner.bin")).with_context(|| format!("reading {}/banner.bin", dir.display()))?;
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<SaveFile>) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
        // Files before folders, as a real Wii lists them.
        entries.sort_by_key(|e| (e.file_type().map_or(false, |t| t.is_dir()), e.file_name()));
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            if prefix.is_empty() && name == "banner.bin" {
                continue;
            }
            let path = format!("{prefix}{name}");
            // As a real Wii's export of this game has them.
            let (permissions, attributes) = (0x3c, 0);
            if e.file_type()?.is_dir() {
                out.push(SaveFile { path: path.clone(), permissions, attributes, kind: Kind::Dir });
                walk(&e.path(), &format!("{path}/"), out)?;
            } else {
                out.push(SaveFile { path, permissions, attributes, kind: Kind::File(std::fs::read(e.path())?) });
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(dir, "", &mut files)?;
    Ok(Save { title_id: TITLE_ID, permissions: 0x30, banner, files })
}

/// Writes the current save to `path` as a data.bin, after reading the
/// result back and checking its signatures. Returns what signed it.
pub fn export(cfg: &Config, path: &Path) -> Result<&'static str> {
    let keys = keys(cfg)?;
    let save = read_save(&data_dir(&cfg.paths.build_dir))?;
    let (device, signer) = device(cfg)?;
    let out = wiisave::build(&save, &keys, &device)?;
    let back = wiisave::parse(&out, &keys, TITLE_ID).context("reading the export back")?;
    if back.files != save.files {
        bail!("the export did not read back the same (please report this)");
    }
    wiisave::verify_signature(&out).context("checking the export's signature")?;
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(signer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The built-in SD keys match the key files.
    #[test]
    fn built_in_keys() {
        let mut cfg = Config::default();
        cfg.paths.common_key = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../keys/common-key");
        if !cfg.paths.common_key.with_file_name("sd-key").exists() {
            eprintln!("no key files; skipped");
            return;
        }
        let files = keys(&cfg).unwrap();
        assert_eq!((files.sd_key, files.sd_iv, files.md5_blanker), (SD_KEYS.sd_key, SD_KEYS.sd_iv, SD_KEYS.md5_blanker));
        cfg.paths.common_key = std::env::temp_dir().join("no-keys-here/common-key");
        assert_eq!(keys(&cfg).unwrap().sd_key, SD_KEYS.sd_key);
    }

    /// Import into a scratch build folder, then export it again: the
    /// previous save is backed up, the import replaces it, and the export
    /// reads back as the import with a valid signature.
    #[test]
    fn import_then_export() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let Some(home) = std::env::var_os("USERPROFILE") else { return };
        let sample = PathBuf::from(home).join("Downloads/data.bin");
        if !sample.exists() || !root.join("keys/sd-key").exists() {
            eprintln!("no sample save or keys; skipped");
            return;
        }
        let build = std::env::temp_dir().join(format!("rumble-save-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&build);
        let dir = data_dir(&build);
        std::fs::create_dir_all(dir.join("backup")).unwrap();
        std::fs::write(dir.join("savedata.bin"), b"old save").unwrap();
        std::fs::write(dir.join("stale.bin"), b"gone after import").unwrap();
        std::fs::write(dir.join("banner.bin"), vec![0x11; 0xf0a0]).unwrap();

        let mut cfg = Config::default();
        cfg.paths.common_key = root.join("keys/common-key");
        cfg.paths.build_dir = build.clone();
        let save = read_import(&cfg, &sample).unwrap();
        let backup = apply_import(&build, &save).unwrap();
        assert_eq!(std::fs::read(backup.join("savedata.bin")).unwrap(), b"old save");
        assert!(!dir.join("stale.bin").exists());
        assert_eq!(std::fs::read(dir.join("banner.bin")).unwrap(), vec![0x11; 0xf0a0], "banner kept");
        assert_eq!(std::fs::read(dir.join("savedata.bin")).unwrap().len(), 212_992);
        assert!(dir.join("backup/savedata.bin").exists());
        // A second import makes a second backup, never overwriting one.
        let again = apply_import(&build, &save).unwrap();
        assert_ne!(again, backup);

        let out = build.join("export.bin");
        export(&cfg, &out).unwrap();
        let back = read_import(&cfg, &out).unwrap();
        assert_eq!(back.files, save.files.iter().map(|f| SaveFile { permissions: 0x3c, ..f.clone() }).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(&build);
    }
}
