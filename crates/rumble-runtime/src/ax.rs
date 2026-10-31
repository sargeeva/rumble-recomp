//! AX, the SDK's audio mixer, run on the host in place of its DSP
//! microcode (`dsp.rs` hands each frame's command list here).
//!
//! Every 3 ms frame the CPU builds a command list: where the voices'
//! parameter blocks are, where the mixed output goes. This mixes the
//! voices into the main left/right buses and writes the output buffer the
//! game then plays through the audio DMA (where the host picks it up,
//! `Memory::audio`). Each voice is a parameter block (PB) in RAM, linked
//! to the next; its playback state is written back, as the CPU follows it
//! (streams are refilled from where the voice has got to).
//!
//! Not yet: the aux buses (reverb, which the CPU applies between frames),
//! surround, the low-pass and biquad filters' finer points, and the Wii
//! Remote speakers.

use crate::mem::Memory;

/// Samples per frame: 3 ms at 32 kHz.
pub const FRAME: usize = 96;

/// Command list opcodes (this SDK's AX).
const CMD_SETUP: u16 = 0x00;
const CMD_PROCESS: u16 = 0x04;
const CMD_OUTPUT: u16 = 0x0b;
const CMD_END: u16 = 0x0e;

/// Argument words each command takes, or None for one not understood
/// (the list is abandoned there).
fn arg_words(cmd: u16) -> Option<u32> {
    Some(match cmd {
        0x00..=0x04 => 2, // setup, add/sub to main, process
        0x05..=0x07 => 5, // aux mixes: volume, write and read buffers
        0x0a => 4,
        0x0b | 0x0c => 5, // output: volume, surround and main buffers
        0x0d => 8,        // Wii Remote outputs
        _ => return None,
    })
}

/// Word offsets into a PB (u16 units).
mod pb {
    pub const SIZE: u32 = 0x140;
    pub const NEXT: usize = 0;
    pub const RUNNING: usize = 8;
    pub const IS_STREAM: usize = 9;
    pub const MIX_LEFT: usize = 10;
    pub const MIX_RIGHT: usize = 12;
    pub const VOL: usize = 53;
    pub const VOL_DELTA: usize = 54;
    pub const LOOPING: usize = 55;
    pub const FORMAT: usize = 56;
    pub const LOOP_ADDR: usize = 57;
    pub const END_ADDR: usize = 59;
    pub const CUR_ADDR: usize = 61;
    pub const COEFS: usize = 63;
    pub const PRED_SCALE: usize = 80;
    pub const YN1: usize = 81;
    pub const YN2: usize = 82;
    pub const RATIO: usize = 83;
    pub const FRAC: usize = 85;
    pub const HISTORY: usize = 86;
    pub const LOOP_PRED_SCALE: usize = 90;
    pub const LOOP_YN1: usize = 91;
    pub const LOOP_YN2: usize = 92;
    pub const LPF_ON: usize = 93;
    pub const LPF_YN1: usize = 94;
    pub const LPF_A0: usize = 95;
    pub const LPF_B0: usize = 96;
}

/// Sample formats.
const FMT_ADPCM: u16 = 0x00;
const FMT_PCM16: u16 = 0x0a;
const FMT_PCM8: u16 = 0x19;

/// The main buses for one frame.
struct Buses {
    left: [i32; FRAME],
    right: [i32; FRAME],
}

impl Buses {
    fn silent() -> Self {
        Self { left: [0; FRAME], right: [0; FRAME] }
    }
}

/// Runs the command list at `list` (a physical or virtual address).
pub fn run(mem: &mut Memory, list: u32) {
    let mut buses = Buses::silent();
    let base = list | 0x8000_0000;
    let word = |mem: &mut Memory, i: u32| mem.r16(base + 2 * i).unwrap_or(CMD_END);
    let long = |mem: &mut Memory, i: u32| (word(mem, i) as u32) << 16 | word(mem, i + 1) as u32;
    let mut i = 0;
    // A list is short; the bound guards against garbage.
    while i < 0x400 {
        let cmd = word(mem, i);
        if cmd == CMD_END {
            break;
        }
        let Some(args) = arg_words(cmd) else { break };
        match cmd {
            CMD_SETUP => buses = Buses::silent(),
            CMD_PROCESS => {
                let voices = long(mem, i + 1);
                process_voices(mem, voices, &mut buses);
            }
            CMD_OUTPUT => {
                let (volume, out) = (word(mem, i + 1) as i64, long(mem, i + 4));
                output(mem, out, &buses, volume);
            }
            _ => {}
        }
        i += 1 + args;
    }
}

/// Writes the main buses to `out` as interleaved big-endian 16-bit
/// left/right samples, at `volume` (0x8000 = 1).
fn output(mem: &mut Memory, out: u32, buses: &Buses, volume: i64) {
    let mut bytes = Vec::with_capacity(FRAME * 4);
    for k in 0..FRAME {
        for v in [buses.left[k], buses.right[k]] {
            bytes.extend_from_slice(&clamp16((v as i64 * volume) >> 15).to_be_bytes());
        }
    }
    let _ = mem.load(out | 0x8000_0000, &bytes);
}

fn clamp16(v: i64) -> i16 {
    v.clamp(i16::MIN as i64, i16::MAX as i64) as i16
}

fn process_voices(mem: &mut Memory, first: u32, buses: &mut Buses) {
    let mut addr = first;
    // A voice list is at most a few hundred long; the bound guards loops.
    for _ in 0..512 {
        if addr == 0 {
            break;
        }
        let a = addr | 0x8000_0000;
        let Ok(bytes) = mem.read_bytes(a, pb::SIZE) else { break };
        let mut w: Vec<u16> = bytes.chunks(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        let next = (w[pb::NEXT] as u32) << 16 | w[pb::NEXT + 1] as u32;
        if w[pb::RUNNING] != 0 {
            mix_voice(mem, &mut w, buses);
            let out: Vec<u8> = w.iter().flat_map(|v| v.to_be_bytes()).collect();
            let _ = mem.load(a, &out);
        }
        addr = next;
    }
}

fn long(w: &[u16], i: usize) -> u32 {
    (w[i] as u32) << 16 | w[i + 1] as u32
}

fn set_long(w: &mut [u16], i: usize, v: u32) {
    w[i] = (v >> 16) as u16;
    w[i + 1] = v as u16;
}

/// The voice's sample reader: its position, ADPCM state and loop.
struct Reader {
    cur: u32,
    end: u32,
    looping: bool,
    stream: bool,
    loop_addr: u32,
    format: u16,
    coefs: [i32; 16],
    pred_scale: u16,
    yn1: i32,
    yn2: i32,
    loop_pred_scale: u16,
    loop_yn1: i32,
    loop_yn2: i32,
    done: bool,
}

impl Reader {
    /// The next sample, stepping past the end to the loop or stopping.
    fn next(&mut self, mem: &Memory) -> i32 {
        if self.done {
            return 0;
        }
        let ram = mem.ram();
        let byte = |a: u32| ram.bytes(a, 1).map_or(0, |b| b[0]);
        let s = match self.format {
            FMT_PCM16 => {
                let a = self.cur.wrapping_mul(2);
                ram.bytes(a, 2).map_or(0, |b| i16::from_be_bytes([b[0], b[1]]) as i32)
            }
            FMT_PCM8 => (byte(self.cur) as i8 as i32) << 8,
            _ => {
                // 8-byte frames: a predictor/scale byte, then 14 nibbles.
                if self.cur & 15 == 0 {
                    self.pred_scale = byte(self.cur >> 1) as u16;
                    self.cur += 2;
                }
                let b = byte(self.cur >> 1);
                let nibble = if self.cur & 1 == 0 { (b >> 4) as i32 } else { (b & 15) as i32 };
                let nibble = if nibble >= 8 { nibble - 16 } else { nibble };
                let scale = 1i32 << (self.pred_scale & 15);
                let idx = ((self.pred_scale >> 4) & 7) as usize;
                let (c1, c2) = (self.coefs[idx * 2], self.coefs[idx * 2 + 1]);
                let v = ((nibble * scale) << 11) + c1 * self.yn1 + c2 * self.yn2;
                let v = ((v + 0x400) >> 11).clamp(-32768, 32767);
                self.yn2 = self.yn1;
                self.yn1 = v;
                v
            }
        };
        // At the end, or past it (a voice must never run on past its end
        // and keep its slot, which the game has only so many of).
        if self.cur >= self.end {
            if self.looping {
                self.cur = self.loop_addr;
                if self.format == FMT_ADPCM && !self.stream {
                    self.pred_scale = self.loop_pred_scale;
                    self.yn1 = self.loop_yn1;
                    self.yn2 = self.loop_yn2;
                }
            } else {
                self.done = true;
            }
        } else {
            self.cur = self.cur.wrapping_add(1);
        }
        s
    }
}

fn mix_voice(mem: &mut Memory, w: &mut [u16], buses: &mut Buses) {
    let mut r = Reader {
        cur: long(w, pb::CUR_ADDR),
        end: long(w, pb::END_ADDR),
        looping: w[pb::LOOPING] != 0,
        stream: w[pb::IS_STREAM] != 0,
        loop_addr: long(w, pb::LOOP_ADDR),
        format: w[pb::FORMAT],
        coefs: std::array::from_fn(|k| w[pb::COEFS + k] as i16 as i32),
        pred_scale: w[pb::PRED_SCALE],
        yn1: w[pb::YN1] as i16 as i32,
        yn2: w[pb::YN2] as i16 as i32,
        loop_pred_scale: w[pb::LOOP_PRED_SCALE],
        loop_yn1: w[pb::LOOP_YN1] as i16 as i32,
        loop_yn2: w[pb::LOOP_YN2] as i16 as i32,
        done: false,
    };
    // Resampling: 16.16 step through the source, linear between the last
    // two samples read.
    let ratio = long(w, pb::RATIO);
    let mut frac = w[pb::FRAC] as u32;
    let mut hist: [i32; 4] = std::array::from_fn(|k| w[pb::HISTORY + k] as i16 as i32);
    let mut vol = w[pb::VOL] as i32;
    let vol_delta = w[pb::VOL_DELTA] as i16 as i32;
    let lpf = w[pb::LPF_ON] != 0;
    let (a0, b0) = (w[pb::LPF_A0] as i32, w[pb::LPF_B0] as i32);
    let mut lpf_y = w[pb::LPF_YN1] as i16 as i32;
    let (left, right) = (w[pb::MIX_LEFT] as i64, w[pb::MIX_RIGHT] as i64);
    for k in 0..FRAME {
        let s = hist[2] + (((hist[3] - hist[2]) * frac as i32) >> 16);
        let mut s = ((s as i64 * vol as i64) >> 15) as i32;
        vol = (vol + vol_delta).clamp(0, 0xffff);
        if lpf {
            s = (a0 * s + b0 * lpf_y) >> 15;
            lpf_y = s;
        }
        buses.left[k] += ((s as i64 * left) >> 15) as i32;
        buses.right[k] += ((s as i64 * right) >> 15) as i32;
        frac += ratio;
        while frac >= 0x1_0000 {
            frac -= 0x1_0000;
            hist = [hist[1], hist[2], hist[3], r.next(mem)];
        }
    }
    set_long(w, pb::CUR_ADDR, r.cur);
    w[pb::PRED_SCALE] = r.pred_scale;
    w[pb::YN1] = r.yn1 as u16;
    w[pb::YN2] = r.yn2 as u16;
    w[pb::FRAC] = frac as u16;
    for (k, h) in hist.iter().enumerate() {
        w[pb::HISTORY + k] = *h as u16;
    }
    w[pb::VOL] = vol as u16;
    w[pb::LPF_YN1] = lpf_y as u16;
    if r.done {
        w[pb::RUNNING] = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: u32 = 0x8000_1000;
    const VOICE: u32 = 0x8000_2000;
    const SAMPLES: u32 = 0x8000_3000;
    const OUT: u32 = 0x8000_4000;

    fn put16(mem: &mut Memory, a: u32, words: &[u16]) {
        for (k, v) in words.iter().enumerate() {
            mem.w16(a + 2 * k as u32, *v).unwrap();
        }
    }

    /// A one-shot PCM16 voice at full volume, panned left, played at its
    /// own rate: the output carries its samples on the left, then stops.
    #[test]
    fn mixes_a_pcm_voice_into_the_output() {
        let mut mem = Memory::default();
        let samples: Vec<u16> = (0..200).map(|k| (k * 100) as u16).collect();
        put16(&mut mem, SAMPLES, &samples);
        let mut w = [0u16; (pb::SIZE / 2) as usize];
        w[pb::RUNNING] = 1;
        w[pb::MIX_LEFT] = 0x8000;
        w[pb::VOL] = 0x8000;
        w[pb::FORMAT] = FMT_PCM16;
        let first = (SAMPLES & 0x1fff_ffff) / 2;
        set_long(&mut w, pb::CUR_ADDR, first);
        set_long(&mut w, pb::END_ADDR, first + 149);
        set_long(&mut w, pb::RATIO, 0x1_0000);
        put16(&mut mem, VOICE, &w);
        put16(&mut mem, LIST, &[0x00, 0, 0, 0x04, (VOICE >> 16) as u16, VOICE as u16, 0x0b, 0x8000, 0, 0, (OUT >> 16) as u16, OUT as u16, 0x0e]);

        run(&mut mem, LIST);
        let left = |mem: &mut Memory, k: u32| mem.r16(OUT + 4 * k).unwrap() as i16;
        // Two frames of history delay the voice by two samples.
        assert_eq!((left(&mut mem, 2), left(&mut mem, 50)), (0, 4800));
        assert_eq!(mem.r16(OUT + 4 * 50 + 2).unwrap(), 0, "nothing on the right");
        assert_eq!(mem.r16(VOICE + 2 * pb::RUNNING as u32).unwrap(), 1);

        run(&mut mem, LIST);
        assert_eq!(mem.r16(VOICE + 2 * pb::RUNNING as u32).unwrap(), 0, "stopped at its end");
    }

    /// ADPCM: a frame header's predictor 0 and scale 0 make each nibble
    /// the sample itself, shifted up 11 then back.
    #[test]
    fn decodes_adpcm_nibbles() {
        let mut mem = Memory::default();
        mem.load(SAMPLES, &[0x00, 0x12, 0x3f]).unwrap();
        let mut r = Reader {
            cur: (SAMPLES & 0x1fff_ffff) * 2,
            end: u32::MAX,
            looping: false,
            stream: false,
            loop_addr: 0,
            format: FMT_ADPCM,
            coefs: [0; 16],
            pred_scale: 0,
            yn1: 0,
            yn2: 0,
            loop_pred_scale: 0,
            loop_yn1: 0,
            loop_yn2: 0,
            done: false,
        };
        let got: Vec<i32> = (0..4).map(|_| r.next(&mem)).collect();
        assert_eq!(got, [1, 2, 3, -1]);
    }
}
