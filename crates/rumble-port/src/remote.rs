//! Remote control of `rumble-port run` through files in `build/remote/`, so
//! a script (or an assistant without access to the window) can drive the
//! running game alongside the keyboard and gamepads:
//!
//! - `commands.txt`: append one command per line; lines already in the file
//!   when the game starts are skipped.
//! - `log.txt`: each command as it runs, with the frame, and its result.
//! - `status.txt`: rewritten every second with the frame, speed and timings.
//! - `input.log`: every change to remote 1's buttons, for `boot --replay`.
//! - `ios.log`: every IOS request the game makes (file access, devices).
//! - `sessions/<unix time>/`: both logs again for each session, so starting
//!   the game again doesn't lose the last one, and in `nand/` the saves
//!   (game and Wii Remotes) as they were when it started. Copied into
//!   `build/nand-boot/`, they let `boot --replay` start where the player
//!   did.
//!
//! Commands, run in order; `wait` holds back the ones after it:
//!
//! | Command | Effect |
//! |---|---|
//! | `press BUTTONS [FRAMES]` | hold for FRAMES frames (default 10), e.g. `press a`, `press right+1 30` |
//! | `hold BUTTONS` / `release [BUTTONS]` | hold until released (no buttons: release all) |
//! | `wait FRAMES` | run FRAMES frames before the next command |
//! | `screenshot [NAME]` | save the last presented frame as `NAME` (default `shot_FRAME.png`) |
//! | `pause` / `resume` | stop and restart emulation |
//!
//! Buttons are the Wii Remote's (`a`, `b`, `1`, `2`, `plus`, `minus`,
//! `home`, `up`/`down`/`left`/`right` in the remote's upright orientation),
//! joined with `+`. Frames count copies to the external framebuffer.

use crate::boot::PRESS_FRAMES;
use rumble_runtime::input::button;
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// What the window thread does for a command.
pub enum Action {
    Screenshot(PathBuf),
    Pause,
    Resume,
}

/// The command reader, run on the emulation thread so presses and waits
/// line up with frames.
pub struct Remote {
    dir: PathBuf,
    /// How much of `commands.txt` has been read.
    read: u64,
    /// The end of a line not yet terminated.
    partial: String,
    queue: VecDeque<String>,
    /// Timed presses: buttons and the frame they are let go.
    presses: Vec<(u32, u64)>,
    held: u32,
    wait_until: u64,
}

impl Remote {
    pub fn new(build_dir: &Path) -> std::io::Result<Self> {
        let dir = build_dir.join("remote");
        std::fs::create_dir_all(&dir)?;
        let commands = dir.join("commands.txt");
        let read = match std::fs::metadata(&commands) {
            Ok(m) => m.len(),
            Err(_) => {
                std::fs::write(&commands, "")?;
                0
            }
        };
        let remote =
            Remote { dir, read, partial: String::new(), queue: VecDeque::new(), presses: Vec::new(), held: 0, wait_until: 0 };
        remote.log(&format!("ready; append commands to {}", commands.display()));
        Ok(remote)
    }

    /// Appends a line to `log.txt`.
    pub fn log(&self, msg: &str) {
        log(&self.dir, msg);
    }

    /// Reads new commands and runs those due at `frame`, returning what the
    /// window thread has to do.
    pub fn poll(&mut self, frame: u64) -> Vec<Action> {
        self.read_commands();
        let mut actions = Vec::new();
        while frame >= self.wait_until {
            let Some(line) = self.queue.pop_front() else { break };
            match self.run(&line, frame) {
                Ok(action) => {
                    self.log(&format!("[frame {frame}] {line}"));
                    actions.extend(action);
                }
                Err(e) => self.log(&format!("[frame {frame}] {line}: error: {e}")),
            }
        }
        self.presses.retain(|&(_, until)| frame < until);
        actions
    }

    /// The buttons held by commands at `frame`.
    pub fn buttons(&self, frame: u64) -> u32 {
        self.presses.iter().filter(|&&(_, until)| frame < until).fold(self.held, |acc, &(b, _)| acc | b)
    }

    fn read_commands(&mut self) {
        let path = self.dir.join("commands.txt");
        let Ok(mut file) = std::fs::File::open(&path) else { return };
        let len = file.metadata().map_or(0, |m| m.len());
        if len < self.read {
            // Truncated or replaced: start over from the top.
            self.read = 0;
            self.partial.clear();
        }
        if len == self.read || file.seek(SeekFrom::Start(self.read)).is_err() {
            return;
        }
        let mut new = String::new();
        if file.read_to_string(&mut new).is_err() {
            return;
        }
        self.read += new.len() as u64;
        self.partial.push_str(&new);
        while let Some(end) = self.partial.find('\n') {
            let line = self.partial[..end].trim().to_string();
            self.partial.drain(..=end);
            if !line.is_empty() && !line.starts_with('#') {
                self.queue.push_back(line);
            }
        }
    }

    fn run(&mut self, line: &str, frame: u64) -> Result<Option<Action>, String> {
        let mut words = line.split_whitespace();
        let cmd = words.next().unwrap_or_default().to_ascii_lowercase();
        let arg = words.next();
        let count = |w: Option<&str>, default: u64| w.map_or(Ok(default), |w| w.parse().map_err(|_| format!("bad count {w:?}")));
        match cmd.as_str() {
            "press" => {
                let bits = buttons(arg.ok_or("press what?")?)?;
                self.presses.push((bits, frame + count(words.next(), PRESS_FRAMES)?));
            }
            "hold" => self.held |= buttons(arg.ok_or("hold what?")?)?,
            "release" => self.held &= !arg.map_or(Ok(u32::MAX), buttons)?,
            "wait" => self.wait_until = frame + count(arg, 1)?,
            "screenshot" => {
                let name = arg.map_or_else(|| format!("shot_{frame}.png"), str::to_string);
                return Ok(Some(Action::Screenshot(self.dir.join(name))));
            }
            "pause" => return Ok(Some(Action::Pause)),
            "resume" => return Ok(Some(Action::Resume)),
            _ => return Err("unknown command".into()),
        }
        Ok(None)
    }
}

/// `a+right` → the button bits.
fn buttons(names: &str) -> Result<u32, String> {
    names.split('+').filter(|n| !n.is_empty()).try_fold(0, |acc, n| {
        button::by_name(n).map(|b| acc | b).ok_or_else(|| format!("unknown button {n:?}"))
    })
}

/// Appends a line to `dir/log.txt`.
pub fn log(dir: &Path, msg: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("log.txt")) {
        let _ = writeln!(f, "{msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording keyed by instruction count gives each change from its
    /// instruction on, and the settings in force there.
    #[test]
    fn recording_by_instruction_count() {
        let dir = std::env::temp_dir().join(format!("rumble-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("input.log"), "10 0x0800 5000000\n12 0x0000 6000000\n").unwrap();
        std::fs::write(dir.join("settings.log"), "0 1.3328 0.65 1 1\n").unwrap();
        let r = Recording::load(&dir.join("input.log")).unwrap();
        assert_eq!([r.buttons(10, 4_000_000), r.buttons(10, 5_000_000), r.buttons(12, 6_000_000)], [0, 0x800, 0]);
        let s = r.settings(1_000_000).unwrap();
        assert_eq!((s.widescreen, s.volumes.music), (1.3328, 0.65));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn button_combinations() {
        assert_eq!(buttons("a+right"), Ok(button::A | button::RIGHT));
        assert!(buttons("x").is_err());
    }

    #[test]
    fn commands_run_in_frame_order() {
        let dir = std::env::temp_dir().join(format!("rumble-remote-{}", std::process::id()));
        let mut r = Remote::new(&dir).unwrap();
        std::fs::write(dir.join("remote/commands.txt"), "press a 5\nwait 3\nhold 2\nrelease\n").unwrap();
        assert!(r.poll(100).is_empty());
        assert_eq!(r.buttons(100), button::A);
        r.poll(102);
        assert_eq!(r.buttons(102), button::A, "still waiting");
        r.poll(103);
        assert_eq!(r.buttons(104), button::A, "hold then release ran together");
        r.poll(105);
        assert_eq!(r.buttons(105), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Records a session: remote 1's buttons (keyboard, gamepads and commands
/// together) as `FRAME BUTTONS` lines, one per change, for `boot --replay`,
/// and the IOS requests. Each goes to `build/remote/` (the latest session)
/// and to the session's own folder under `sessions/`.
pub struct Recorder {
    input: Vec<std::fs::File>,
    ios: Vec<std::fs::File>,
    settings: Vec<std::fs::File>,
    last: u32,
    last_settings: Option<Settings>,
    /// IOS log lines already written.
    ios_written: usize,
}

/// Copies the directory tree `from` to `to` (nothing if `from` is missing).
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    if !from.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

impl Recorder {
    pub fn new(build_dir: &Path) -> Self {
        let dir = build_dir.join("remote");
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let session = dir.join("sessions").join(secs.to_string());
        let open = |name: &str| -> Vec<std::fs::File> {
            [dir.join(name), session.join(name)]
                .iter()
                .filter_map(|p| {
                    std::fs::create_dir_all(p.parent()?).ok()?;
                    std::fs::File::create(p).map_err(|e| eprintln!("recording {}: {e}", p.display())).ok()
                })
                .collect()
        };
        for part in ["title", "shared2", "wiimote"] {
            if let Err(e) = copy_dir(&build_dir.join("nand").join(part), &session.join("nand").join(part)) {
                eprintln!("saving the session's starting saves: {e}");
            }
        }
        Recorder {
            input: open("input.log"),
            ios: open("ios.log"),
            settings: open("settings.log"),
            last: 0,
            last_settings: None,
            ios_written: 0,
        }
    }

    /// Notes the buttons the game sees from instruction `steps` (at
    /// `frame`), when they change.
    pub fn record(&mut self, frame: u64, steps: u64, buttons: u32) {
        if buttons == self.last {
            return;
        }
        self.last = buttons;
        for f in &mut self.input {
            let _ = writeln!(f, "{frame} {buttons:#06x} {steps}");
        }
    }

    /// Notes the settings the game runs with from instruction `steps`, when
    /// they change: they change its RAM, so a replay must use them too.
    pub fn settings(&mut self, steps: u64, s: Settings) {
        if self.last_settings == Some(s) {
            return;
        }
        self.last_settings = Some(s);
        let v = s.volumes;
        for f in &mut self.settings {
            let _ = writeln!(f, "{steps} {} {} {} {}", s.widescreen, v.music, v.effects, v.voices);
        }
    }

    /// Writes the IOS requests logged since the last call.
    pub fn ios(&mut self, frame: u64, log: &[String]) {
        for line in log.get(self.ios_written..).unwrap_or_default() {
            for f in &mut self.ios {
                let _ = writeln!(f, "[frame {frame}] {line}");
            }
        }
        self.ios_written = log.len();
    }
}

/// Settings that change the game's RAM, so a replay must match them:
/// the widescreen factor (`hle::mtx` widens the game's cameras) and the
/// sound players' volumes (`rumble_runtime::snd`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub widescreen: f32,
    pub volumes: rumble_runtime::snd::Volumes,
}

/// A session recorded by [`Recorder`]: the buttons, by instruction count
/// (older recordings: by frame), and the settings by instruction count.
#[derive(Default)]
pub struct Recording {
    buttons: Vec<(u64, u32)>,
    by_steps: bool,
    /// For a recording by instruction count: (instruction, frame) of each
    /// change, to check a replay keeps in step.
    frames: Vec<(u64, u64)>,
    settings: Vec<(u64, Settings)>,
}

impl Recording {
    /// `input.log`, and `settings.log` beside it if there is one.
    pub fn load(input_log: &Path) -> std::io::Result<Recording> {
        let text = std::fs::read_to_string(input_log)?;
        let lines: Vec<Vec<&str>> = text.lines().map(|l| l.split_whitespace().collect()).collect();
        let by_steps = !lines.is_empty() && lines.iter().all(|l| l.len() >= 3);
        let buttons = lines
            .iter()
            .filter_map(|l| {
                let key = if by_steps { l.get(2)? } else { l.first()? };
                Some((key.parse().ok()?, u32::from_str_radix(l.get(1)?.trim_start_matches("0x"), 16).ok()?))
            })
            .collect();
        let frames = if by_steps {
            lines.iter().filter_map(|l| Some((l.get(2)?.parse().ok()?, l.first()?.parse().ok()?))).collect()
        } else {
            Vec::new()
        };
        let settings = std::fs::read_to_string(input_log.with_file_name("settings.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let v: Vec<f32> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
                let steps = l.split_whitespace().next()?.parse().ok()?;
                let [widescreen, music, effects, voices] = v[..] else { return None };
                let volumes = rumble_runtime::snd::Volumes { music, effects, voices };
                Some((steps, Settings { widescreen, volumes }))
            })
            .collect();
        Ok(Recording { buttons, by_steps, frames, settings })
    }

    pub fn is_empty(&self) -> bool {
        self.buttons.is_empty()
    }

    /// The buttons held at instruction `steps`, in frame `frame`.
    pub fn buttons(&self, frame: u64, steps: u64) -> u32 {
        let key = if self.by_steps { steps } else { frame };
        match self.buttons.partition_point(|&(k, _)| k <= key) {
            0 => 0,
            k => self.buttons[k - 1].1,
        }
    }

    /// The frame the recorded session was in at the last change at or
    /// before instruction `steps`, and that change's instruction.
    pub fn frame_at(&self, steps: u64) -> Option<(u64, u64)> {
        match self.frames.partition_point(|&(s, _)| s <= steps) {
            0 => None,
            k => Some(self.frames[k - 1]),
        }
    }

    /// The settings in force at instruction `steps`, if recorded.
    pub fn settings(&self, steps: u64) -> Option<Settings> {
        self.settings.iter().rev().find(|(s, _)| *s <= steps).map(|&(_, s)| s)
    }
}
