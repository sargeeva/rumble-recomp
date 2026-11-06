//! H.264 for the online picture (see `docs/online.md`), with Windows'
//! Media Foundation: the host encodes what it shows, on the graphics card
//! when it has an encoder (Microsoft's software one otherwise), and the
//! client decodes it. Both ends work in NV12, converted from and to RGB
//! here with the same (BT.709, limited range) matrix.

use anyhow::Result;

/// The rate pictures are encoded for (a guide for the encoder's bitrate;
/// they come as fast as the host shows them).
const FPS: u32 = 60;

/// Bits per pixel per frame the encoder aims for: about 12 Mbit/s at
/// 1080p, capped at 50.
const BITS_PER_PIXEL: f32 = 0.1;
const MAX_BITRATE: u32 = 50_000_000;

/// An RGBA or BGRA picture as NV12: a full size Y plane, then a half size
/// plane of interleaved U and V. `width` and `height` are even.
pub fn to_nv12(pixels: &[u8], width: u32, height: u32, bgra: bool) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u8; w * h * 3 / 2];
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    let (r, b) = if bgra { (2, 0) } else { (0, 2) };
    // Two rows of Y and one of UV at a time, split over threads.
    let pairs = h / 2;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let per = pairs.div_ceil(threads);
    std::thread::scope(|scope| {
        for (chunk, (ys, uvs)) in y_plane.chunks_mut(per * 2 * w).zip(uv_plane.chunks_mut(per * w)).enumerate() {
            scope.spawn(move || {
                let first = chunk * per;
                for (i, uv_row) in uvs.chunks_mut(w).enumerate() {
                    let row = (first + i) * 2;
                    for x in (0..w).step_by(2) {
                        let (mut su, mut sv) = (0i32, 0i32);
                        for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                            let p = ((row + dy) * w + x + dx) * 4;
                            let (pr, pg, pb) = (pixels[p + r] as i32, pixels[p + 1] as i32, pixels[p + b] as i32);
                            ys[(i * 2 + dy) * w + x + dx] = ((47 * pr + 157 * pg + 16 * pb + 128) >> 8) as u8 + 16;
                            su += -26 * pr - 87 * pg + 112 * pb;
                            sv += 112 * pr - 102 * pg - 10 * pb;
                        }
                        uv_row[x] = ((su + 512) >> 10).wrapping_add(128).clamp(0, 255) as u8;
                        uv_row[x + 1] = ((sv + 512) >> 10).wrapping_add(128).clamp(0, 255) as u8;
                    }
                }
            });
        }
    });
    out
}

/// An NV12 picture (rows `pitch` bytes apart, the UV plane `uv` bytes in)
/// as RGBA, `width` by `height`.
fn from_nv12(data: &[u8], pitch: usize, uv: usize, width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 4];
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let per = height.div_ceil(threads).next_multiple_of(2);
    std::thread::scope(|scope| {
        for (chunk, rows) in out.chunks_mut(per * width * 4).enumerate() {
            scope.spawn(move || {
                for (i, row) in rows.chunks_mut(width * 4).enumerate() {
                    let y_row = chunk * per + i;
                    let (ys, uvs) = (&data[y_row * pitch..], &data[uv + y_row / 2 * pitch..]);
                    for x in 0..width {
                        let y = (ys[x] as i32 - 16) * 298;
                        let (u, v) = (uvs[x & !1] as i32 - 128, uvs[x | 1] as i32 - 128);
                        let px = &mut row[x * 4..x * 4 + 4];
                        px[0] = ((y + 459 * v + 128) >> 8).clamp(0, 255) as u8;
                        px[1] = ((y - 55 * u - 136 * v + 128) >> 8).clamp(0, 255) as u8;
                        px[2] = ((y + 541 * u + 128) >> 8).clamp(0, 255) as u8;
                        px[3] = 255;
                    }
                }
            });
        }
    });
    out
}

/// A decoded picture.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The NAL unit types in H.264 data (Annex B: units after 00 00 01).
fn nal_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    data.windows(4).filter(|w| w[..3] == [0, 0, 1]).map(|w| w[3] & 0x1f)
}

#[cfg(windows)]
pub use mf::{Decoder, Encoder};

#[cfg(windows)]
mod mf {
    use super::*;
    use anyhow::{bail, Context};
    use std::mem::ManuallyDrop;
    use windows::core::{Interface, GUID, VARIANT};
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};

    /// Media Foundation, for this thread.
    fn startup() -> Result<()> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().context("starting COM")?;
            MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET).context("starting Media Foundation")?;
        }
        Ok(())
    }

    /// A video media type: `subtype` (NV12 or H.264), `width` by `height`.
    unsafe fn video_type(subtype: &GUID, width: u32, height: u32) -> Result<IMFMediaType> {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, (width as u64) << 32 | height as u64)?;
        t.SetUINT64(&MF_MT_FRAME_RATE, (FPS as u64) << 32 | 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, 1 << 32 | 1)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        Ok(t)
    }

    /// A sample holding `data`.
    unsafe fn sample(data: &[u8]) -> Result<IMFSample> {
        let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(data.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        Ok(sample)
    }

    /// What one `ProcessOutput` gave.
    enum Output {
        Sample(IMFSample),
        NeedMoreInput,
        /// The output type changed: it is set again, so ask again.
        Changed,
    }

    /// Asks `mft` for output, in a sample of its own if it `provides` them
    /// or else one of `size` bytes.
    unsafe fn process_output(mft: &IMFTransform, provides: bool, size: u32) -> Result<Output> {
        let sample = if provides {
            None
        } else {
            let s = MFCreateSample()?;
            s.AddBuffer(&MFCreateMemoryBuffer(size.max(1 << 16))?)?;
            Some(s)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(sample), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
        let mut status = 0;
        let result = mft.ProcessOutput(0, &mut buffers, &mut status);
        let sample = ManuallyDrop::take(&mut buffers[0].pSample);
        drop(ManuallyDrop::take(&mut buffers[0].pEvents));
        match result {
            Ok(()) => Ok(sample.map_or(Output::NeedMoreInput, Output::Sample)),
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(Output::NeedMoreInput),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => Ok(Output::Changed),
            Err(e) => Err(e.into()),
        }
    }

    /// Whether `mft` gives output in samples of its own, and how big a
    /// sample to give it otherwise.
    unsafe fn output_needs(mft: &IMFTransform) -> Result<(bool, u32)> {
        let info = mft.GetOutputStreamInfo(0)?;
        let own = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
        Ok((info.dwFlags & own != 0, info.cbSize))
    }

    /// An H.264 encoder for pictures of one size.
    pub struct Encoder {
        mft: IMFTransform,
        codec: Option<ICodecAPI>,
        /// Asynchronous (hardware) encoders say through events when they
        /// want input and have output; `needs` counts the input asked for.
        events: Option<IMFMediaEventGenerator>,
        needs: u32,
        provides: bool,
        out_size: u32,
        /// The sequence header (SPS and PPS), for key frames without it.
        header: Option<Vec<u8>>,
        pub width: u32,
        pub height: u32,
        frame: u64,
        /// Which encoder it is, for the log.
        pub name: String,
    }

    impl Encoder {
        /// An encoder for `width` by `height` pictures (even), the graphics
        /// card's if it has one.
        pub fn new(width: u32, height: u32) -> Result<Self> {
            startup()?;
            let mut errors = Vec::new();
            for hardware in [true, false] {
                match unsafe { Self::open(width, height, hardware) } {
                    Ok(e) => return Ok(e),
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
            bail!("no H.264 encoder: {}", errors.join("; "))
        }

        unsafe fn open(width: u32, height: u32, hardware: bool) -> Result<Self> {
            let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
            let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
            let kind = if hardware { MFT_ENUM_FLAG_HARDWARE } else { MFT_ENUM_FLAG_SYNCMFT };
            let mut list = std::ptr::null_mut();
            let mut count = 0;
            MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, kind | MFT_ENUM_FLAG_SORTANDFILTER, Some(&input), Some(&output), &mut list, &mut count)?;
            let found: Vec<Option<IMFActivate>> = (0..count as usize).map(|i| std::ptr::read(list.add(i))).collect();
            CoTaskMemFree(Some(list as *const _));
            let mut errors = Vec::new();
            for activate in found.into_iter().flatten() {
                match Self::activate(&activate, width, height, hardware) {
                    Ok(e) => return Ok(e),
                    Err(e) => errors.push(format!("{e:#}")),
                }
            }
            bail!("{} encoder: {}", if hardware { "hardware" } else { "software" }, if errors.is_empty() { "none found".into() } else { errors.join("; ") })
        }

        unsafe fn activate(activate: &IMFActivate, width: u32, height: u32, hardware: bool) -> Result<Self> {
            let name = {
                let mut buf = [0u16; 128];
                let mut len = 0;
                match activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)) {
                    Ok(()) => String::from_utf16_lossy(&buf[..len as usize]),
                    Err(_) => "H.264 encoder".into(),
                }
            };
            let mft: IMFTransform = activate.ActivateObject()?;
            let attributes = mft.GetAttributes().ok();
            let asynchronous = hardware && attributes.as_ref().is_some_and(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0);
            if let Some(a) = &attributes {
                if asynchronous {
                    a.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                }
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let bitrate = ((width * height) as f32 * FPS as f32 * BITS_PER_PIXEL).min(MAX_BITRATE as f32) as u32;
            let codec: Option<ICodecAPI> = mft.cast().ok();
            if let Some(c) = &codec {
                // Not every encoder takes every setting.
                let _ = c.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                let _ = c.SetValue(&CODECAPI_AVEncCommonRateControlMode, &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32));
                let _ = c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate));
                let _ = c.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(FPS * 2));
                let _ = c.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
            }
            let out = video_type(&MFVideoFormat_H264, width, height)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            mft.SetOutputType(0, &out, 0).context("output type")?;
            mft.SetInputType(0, &video_type(&MFVideoFormat_NV12, width, height)?, 0).context("input type")?;
            let (provides, out_size) = output_needs(&mft)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events = if asynchronous { Some(mft.cast::<IMFMediaEventGenerator>()?) } else { None };
            let header = Self::sequence_header(&mft);
            Ok(Self { mft, codec, events, needs: 0, provides, out_size, header, width, height, frame: 0, name })
        }

        /// The output type's sequence header, if it has one.
        unsafe fn sequence_header(mft: &IMFTransform) -> Option<Vec<u8>> {
            let t = mft.GetOutputCurrentType(0).ok()?;
            let size = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
            let mut header = vec![0; size as usize];
            t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut header, None).ok()?;
            (!header.is_empty()).then_some(header)
        }

        /// Encodes an NV12 picture, as a key frame if `key`. Returns the
        /// frames that came out (Annex B data, and whether each is a key
        /// frame): usually just this one.
        pub fn encode(&mut self, nv12: &[u8], key: bool) -> Result<Vec<(Vec<u8>, bool)>> {
            unsafe {
                let input = sample(nv12)?;
                input.SetSampleTime(self.frame as i64 * 10_000_000 / FPS as i64)?;
                input.SetSampleDuration(10_000_000 / FPS as i64)?;
                self.frame += 1;
                if key {
                    if let Some(c) = &self.codec {
                        let _ = c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                    }
                }
                let mut out = Vec::new();
                match self.events.clone() {
                    None => {
                        self.mft.ProcessInput(0, &input, 0)?;
                        while self.output(&mut out)? {}
                    }
                    Some(events) => {
                        while self.needs == 0 {
                            self.event(&events, true, &mut out)?;
                        }
                        self.mft.ProcessInput(0, &input, 0)?;
                        self.needs -= 1;
                        // Its frame, unless it wants more input first.
                        let before = out.len();
                        loop {
                            let needs = self.needs;
                            self.event(&events, true, &mut out)?;
                            if out.len() > before || self.needs > needs {
                                break;
                            }
                        }
                        while self.event(&events, false, &mut out)? {}
                    }
                }
                Ok(out)
            }
        }

        /// Handles an asynchronous encoder's next event, waiting for one if
        /// `wait`. Returns whether there was one.
        unsafe fn event(&mut self, events: &IMFMediaEventGenerator, wait: bool, out: &mut Vec<(Vec<u8>, bool)>) -> Result<bool> {
            let flags = if wait { MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0) } else { MF_EVENT_FLAG_NO_WAIT };
            let event = match events.GetEvent(flags) {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
                Err(e) => return Err(e.into()),
            };
            match event.GetType()? {
                t if t == METransformNeedInput.0 as u32 => self.needs += 1,
                t if t == METransformHaveOutput.0 as u32 => {
                    // One frame, after any change of output type.
                    let before = out.len();
                    while self.output(out)? && out.len() == before {}
                }
                _ => {}
            }
            Ok(true)
        }

        /// Takes a frame of output, if there is one. Returns whether to ask
        /// again (a frame came, or the output type changed).
        unsafe fn output(&mut self, out: &mut Vec<(Vec<u8>, bool)>) -> Result<bool> {
            match process_output(&self.mft, self.provides, self.out_size)? {
                Output::NeedMoreInput => Ok(false),
                Output::Changed => {
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                    (self.provides, self.out_size) = output_needs(&self.mft)?;
                    self.header = Self::sequence_header(&self.mft).or(self.header.take());
                    Ok(true)
                }
                Output::Sample(s) => {
                    let buffer = s.ConvertToContiguousBuffer()?;
                    let (mut ptr, mut len) = (std::ptr::null_mut(), 0);
                    buffer.Lock(&mut ptr, None, Some(&mut len))?;
                    let mut data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
                    buffer.Unlock()?;
                    let key = s.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0 || nal_types(&data).any(|t| t == 5);
                    if key && !nal_types(&data).any(|t| t == 7) {
                        if let Some(header) = &self.header {
                            data.splice(0..0, header.iter().copied());
                        }
                    }
                    out.push((data, key));
                    Ok(true)
                }
            }
        }
    }

    impl Drop for Encoder {
        fn drop(&mut self) {
            unsafe {
                let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                if let Some(events) = &self.events {
                    let _ = events.cast::<IMFShutdown>().map(|s| s.Shutdown());
                }
            }
        }
    }

    /// Microsoft's H.264 decoder.
    pub struct Decoder {
        mft: IMFTransform,
        provides: bool,
        out_size: u32,
        /// The output's size as allocated (height rounded up, say to 16).
        frame: Option<(u32, u32)>,
    }

    impl Decoder {
        pub fn new() -> Result<Self> {
            startup()?;
            unsafe {
                let mft: IMFTransform = CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER).context("no H.264 decoder")?;
                if let Ok(a) = mft.GetAttributes() {
                    let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
                }
                if let Ok(c) = mft.cast::<ICodecAPI>() {
                    let _ = c.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                }
                let t = MFCreateMediaType()?;
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
                mft.SetInputType(0, &t, 0)?;
                let mut d = Self { mft, provides: false, out_size: 0, frame: None };
                // Until it has seen the stream, it may have no output types.
                let _ = d.set_output();
                d.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
                d.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
                Ok(d)
            }
        }

        /// Picks NV12 output.
        unsafe fn set_output(&mut self) -> Result<()> {
            for i in 0.. {
                let t = self.mft.GetOutputAvailableType(0, i)?;
                if t.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    self.mft.SetOutputType(0, &t, 0)?;
                    let size = t.GetUINT64(&MF_MT_FRAME_SIZE)?;
                    self.frame = Some(((size >> 32) as u32, size as u32));
                    (self.provides, self.out_size) = output_needs(&self.mft)?;
                    return Ok(());
                }
            }
            unreachable!()
        }

        /// Decodes a frame's data; with `picture`, returns what it shows:
        /// its size (`width` by `height`, or less if the decoder's is) and
        /// RGBA. Every frame must be given, in order; `picture` can be left
        /// off for ones nobody will see.
        pub fn decode(&mut self, data: &[u8], width: u32, height: u32, picture: bool) -> Result<Option<Picture>> {
            unsafe {
                let input = sample(data)?;
                let mut shown = None;
                loop {
                    match self.mft.ProcessInput(0, &input, 0) {
                        Ok(()) => break,
                        // Its output has to be taken first.
                        Err(e) if e.code() == MF_E_NOTACCEPTING => self.drain(width, height, picture, &mut shown)?,
                        Err(e) => return Err(e.into()),
                    }
                }
                self.drain(width, height, picture, &mut shown)?;
                Ok(shown)
            }
        }

        /// Takes all the output there is, keeping the newest picture in
        /// `shown`.
        unsafe fn drain(&mut self, width: u32, height: u32, picture: bool, shown: &mut Option<Picture>) -> Result<()> {
            loop {
                match process_output(&self.mft, self.provides, self.out_size)? {
                    Output::NeedMoreInput => return Ok(()),
                    Output::Changed => self.set_output()?,
                    Output::Sample(s) if picture => {
                        let Some((_, rows)) = self.frame else { continue };
                        let buffer = s.ConvertToContiguousBuffer()?;
                        let (mut ptr, mut pitch) = (std::ptr::null_mut(), 0i32);
                        let two_d = buffer.cast::<IMF2DBuffer>();
                        let (data, pitch) = match &two_d {
                            Ok(b) if b.Lock2D(&mut ptr, &mut pitch).is_ok() => {
                                let pitch = pitch.unsigned_abs() as usize;
                                (std::slice::from_raw_parts(ptr, pitch * rows as usize * 3 / 2), pitch)
                            }
                            _ => {
                                let mut len = 0;
                                buffer.Lock(&mut ptr, None, Some(&mut len))?;
                                let pitch = len as usize * 2 / 3 / rows as usize;
                                (std::slice::from_raw_parts(ptr, len as usize), pitch)
                            }
                        };
                        let (w, h) = ((width as usize).min(pitch), (height as usize).min(rows as usize));
                        *shown = Some(Picture { width: w as u32, height: h as u32, rgba: from_nv12(data, pitch, pitch * rows as usize, w, h) });
                        match &two_d {
                            Ok(b) => {
                                let _ = b.Unlock2D();
                            }
                            Err(_) => buffer.Unlock()?,
                        }
                    }
                    Output::Sample(_) => {}
                }
            }
        }
    }
}

/// Without Media Foundation there is no H.264: pictures stay JPEG.
#[cfg(not(windows))]
mod none {
    use anyhow::{bail, Result};

    pub struct Encoder {
        pub width: u32,
        pub height: u32,
        pub name: String,
    }

    impl Encoder {
        pub fn new(_width: u32, _height: u32) -> Result<Self> {
            bail!("H.264 needs Windows")
        }

        pub fn encode(&mut self, _nv12: &[u8], _key: bool) -> Result<Vec<(Vec<u8>, bool)>> {
            bail!("H.264 needs Windows")
        }
    }

    pub struct Decoder;

    impl Decoder {
        pub fn new() -> Result<Self> {
            bail!("H.264 needs Windows")
        }

        pub fn decode(&mut self, _data: &[u8], _width: u32, _height: u32, _picture: bool) -> Result<Option<super::Picture>> {
            bail!("H.264 needs Windows")
        }
    }
}

#[cfg(not(windows))]
pub use none::{Decoder, Encoder};

#[cfg(test)]
mod tests {
    use super::*;

    /// Colours survive the trip through NV12 (within rounding).
    #[test]
    fn nv12_round_trip() {
        let (w, h) = (8u32, 4u32);
        let mut rgba = Vec::new();
        for i in 0..w * h {
            let c = [(i * 37 % 256) as u8, (i * 91 % 256) as u8, (i * 53 % 256) as u8, 255];
            rgba.extend_from_slice(&c);
        }
        // Flat 2x2 blocks, so subsampling loses nothing.
        for y in 0..h as usize {
            for x in 0..w as usize {
                let src = ((y & !1) * w as usize + (x & !1)) * 4;
                let px: [u8; 4] = rgba[src..src + 4].try_into().unwrap();
                rgba[(y * w as usize + x) * 4..][..4].copy_from_slice(&px);
            }
        }
        let nv12 = to_nv12(&rgba, w, h, false);
        let back = from_nv12(&nv12, w as usize, (w * h) as usize, w as usize, h as usize);
        for (a, b) in rgba.iter().zip(&back) {
            assert!(a.abs_diff(*b) <= 6, "{a} vs {b}");
        }
    }

    /// The hardware (or software) encoder's output decodes.
    #[cfg(windows)]
    #[test]
    fn encode_and_decode() {
        let (w, h) = (320u32, 240u32);
        let mut encoder = match Encoder::new(w, h) {
            Ok(e) => e,
            Err(e) => return eprintln!("skipped: {e:#}"),
        };
        eprintln!("encoder: {}", encoder.name);
        let mut decoder = Decoder::new().unwrap();
        let mut shown = None;
        for f in 0..30u32 {
            let rgba: Vec<u8> = (0..w * h).flat_map(|i| [((i + f) % 256) as u8, 128, 64, 255]).collect();
            for (data, _) in encoder.encode(&to_nv12(&rgba, w, h, false), f == 0).unwrap() {
                if let Some(p) = decoder.decode(&data, w, h, true).unwrap() {
                    shown = Some(p);
                }
            }
        }
        let shown = shown.expect("a picture came out");
        assert_eq!((shown.width, shown.height, shown.rgba.len()), (w, h, (w * h * 4) as usize));
    }
}
