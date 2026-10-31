//! Sound output: what the game plays through its audio DMA
//! (`Memory::audio`, 16-bit stereo at 32 kHz) goes to the default output
//! device through cpal, resampled to the device's rate, with the player's
//! volume and mono/stereo setting (the config's `[audio]`). Music, sound
//! effect and Pokémon voice volumes are applied in the game, to its sound
//! players (`rumble_runtime::snd`), by the emulator thread.
//!
//! The emulator thread pushes samples into a queue (`AudioQueue::push`);
//! the device's callback takes them. Emulation comes in bursts (a stage
//! loading stalls it, then it catches up), so rather than drop sound the
//! queue is held near `TARGET` by playing it a few percent faster or
//! slower; after running dry, playing resumes once the target is queued
//! again. The target grows each time the queue runs dry (stage loads keep
//! the game busier than the host can emulate in real time for a moment),
//! up to `MAX_TARGET`, and eases back to `TARGET` while it doesn't. Only a
//! backlog past `MAX_QUEUED` is dropped.

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// The `[audio]` config section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// 0-1.
    pub master: f32,
    pub music: f32,
    pub effects: f32,
    pub voices: f32,
    /// Both speakers get the same mix.
    pub mono: bool,
    /// Silent while the window is in the background.
    pub mute_in_background: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self { master: 0.8, music: 1.0, effects: 1.0, voices: 1.0, mono: false, mute_in_background: false }
    }
}

/// Queue length aimed for, and beyond which the oldest is dropped
/// (seconds); the most the playback rate is bent to get back to `TARGET`.
const TARGET: f64 = 0.08;
const MAX_TARGET: f64 = 0.3;
const MAX_QUEUED: f64 = 0.8;
const MAX_BEND: f64 = 0.01;

/// Settings the device callback and the emulator thread read as they
/// change.
#[derive(Default)]
pub struct Mix {
    master: AtomicU32,
    mono: AtomicBool,
    muted: AtomicBool,
    /// Music, effects, voices.
    kinds: [AtomicU32; 3],
}

impl Mix {
    pub fn set(&self, cfg: &AudioConfig, muted: bool) {
        self.master.store(cfg.master.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        self.mono.store(cfg.mono, Ordering::Relaxed);
        self.muted.store(muted, Ordering::Relaxed);
        for (k, v) in self.kinds.iter().zip([cfg.music, cfg.effects, cfg.voices]) {
            k.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
    }

    /// The per-kind volumes, for the game's sound players.
    pub fn volumes(&self) -> rumble_runtime::snd::Volumes {
        let [music, effects, voices] = self.kinds.each_ref().map(|k| f32::from_bits(k.load(Ordering::Relaxed)));
        rumble_runtime::snd::Volumes { music, effects, voices }
    }
}

/// The game's samples waiting for the device: interleaved left/right.
pub struct AudioQueue {
    samples: Mutex<(VecDeque<i16>, u32)>,
}

impl AudioQueue {
    /// Adds interleaved stereo samples played at `rate`.
    pub fn push(&self, samples: &[i16], rate: u32) {
        let mut q = self.samples.lock().unwrap();
        q.1 = rate;
        q.0.extend(samples);
        let max = (rate as f64 * MAX_QUEUED) as usize * 2;
        if q.0.len() > max {
            let extra = q.0.len() - max;
            q.0.drain(..extra);
        }
    }
}

/// The open output device. Sound stops when it is dropped.
pub struct Audio {
    _stream: cpal::Stream,
    pub queue: Arc<AudioQueue>,
}

/// Linear resampling from the game's rate to the device's, across calls.
struct Resampler {
    /// Position between `prev` and the next queued frame, 0-1.
    pos: f64,
    prev: [f32; 2],
    next: [f32; 2],
}

impl Audio {
    pub fn open(mix: Arc<Mix>) -> Result<Audio> {
        let host = cpal::default_host();
        let device = host.default_output_device().context("no audio output device")?;
        let supported = device.default_output_config().context("reading the audio device's format")?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let channels = config.channels as usize;
        let device_rate = config.sample_rate.0 as f64;
        let queue = Arc::new(AudioQueue { samples: Mutex::new((VecDeque::new(), 32_000)) });
        let (q, m) = (queue.clone(), mix);
        let mut rs = Resampler { pos: 0.0, prev: [0.0; 2], next: [0.0; 2] };
        // Run dry: wait for the target before playing again.
        let mut starved = true;
        let mut target = TARGET;
        // Fills `out` (interleaved, `channels` per frame) as f32.
        let mut fill = move |out: &mut [f32]| {
            let master = f32::from_bits(m.master.load(Ordering::Relaxed));
            let (mono, muted) = (m.mono.load(Ordering::Relaxed), m.muted.load(Ordering::Relaxed));
            let mut q = q.samples.lock().unwrap();
            let rate = q.1 as f64;
            let queued = q.0.len() as f64 / 2.0 / rate;
            if starved && queued >= target {
                starved = false;
            }
            // Eases back over about a minute without running dry.
            let secs = out.len() as f64 / channels as f64 / device_rate;
            target = (target - secs * (target - TARGET) / 60.0).max(TARGET);
            // Faster when behind, slower when running low.
            let bend = ((queued - target) / target * MAX_BEND).clamp(-MAX_BEND, MAX_BEND);
            let step = rate / device_rate * (1.0 + bend);
            for frame in out.chunks_mut(channels) {
                if starved {
                    // Fade what was playing out rather than hold it.
                    rs.prev = rs.prev.map(|s| s * 0.995);
                    rs.next = rs.prev;
                } else {
                    rs.pos += step;
                    while rs.pos >= 1.0 {
                        rs.pos -= 1.0;
                        rs.prev = rs.next;
                        if q.0.len() >= 2 {
                            let (l, r) = (q.0.pop_front().unwrap(), q.0.pop_front().unwrap());
                            rs.next = [l as f32 / 32768.0, r as f32 / 32768.0];
                        } else if !starved {
                            starved = true;
                            target = (target * 1.5).min(MAX_TARGET);
                        }
                    }
                }
                let t = rs.pos as f32;
                let mut lr = [0, 1].map(|c| rs.prev[c] + (rs.next[c] - rs.prev[c]) * t);
                if mono {
                    lr = [(lr[0] + lr[1]) * 0.5; 2];
                }
                let gain = if muted { 0.0 } else { master };
                for (c, s) in frame.iter_mut().enumerate() {
                    *s = if c < 2 { lr[c] * gain } else { 0.0 };
                }
            }
        };
        let err = |e| eprintln!("audio: {e}");
        let stream = match format {
            cpal::SampleFormat::F32 => device.build_output_stream(&config, move |d: &mut [f32], _| fill(d), err, None)?,
            cpal::SampleFormat::I16 => {
                let mut buf = Vec::new();
                device.build_output_stream(
                    &config,
                    move |d: &mut [i16], _| {
                        buf.resize(d.len(), 0.0);
                        fill(&mut buf);
                        for (o, s) in d.iter_mut().zip(&buf) {
                            *o = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        }
                    },
                    err,
                    None,
                )?
            }
            other => anyhow::bail!("unsupported audio sample format {other:?}"),
        };
        stream.play()?;
        Ok(Audio { _stream: stream, queue })
    }
}
