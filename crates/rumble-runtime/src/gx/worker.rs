//! Building draw commands on a thread of their own. Turning the game's
//! primitives into draws (reading, transforming and lighting every vertex,
//! looking up textures) costs about as much as running the game in busy
//! scenes; done by the emulator thread, it slows the game, and the sound
//! the game makes as it runs.
//!
//! With a worker, the emulator's `Gx` still decodes every command, keeping
//! the register state, the interrupts and what the game-side hooks read
//! (`Gx::note_view`), but builds no draws: it hands the FIFO bytes it
//! executes to a second `Gx` on the worker, which decodes them again and
//! builds the frames. What the hooks set for drawing goes along with the
//! bytes (`Msg::Change`, `Msg::State`).
//!
//! The worker reads RAM while the game runs, as the GPU does on the Wii: the
//! game keeps what a draw uses in place until the GPU is done, which it
//! learns from a draw-done (BP 0x45). So the emulator waits there for the
//! worker to catch up before raising it (`Worker::wait`). Nothing the
//! game sees depends on the worker, so sessions replay the same either way.

use super::draw::{GpuCommand, MAX_QUEUED_FRAMES};
use super::Gx;
use crate::mem::Ram;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// FIFO bytes collected before they are sent to the worker.
const BATCH: usize = 4096;

/// What the emulator sends the worker, in order.
enum Msg {
    /// Executed FIFO bytes.
    Bytes(Vec<u8>),
    /// A change taking effect at a FIFO position (counted from the start).
    Change(u64, Change),
    /// The widescreen settings the game-side hooks keep.
    State { widescreen: f32, wide_cameras: Vec<(u32, f32)> },
}

/// Changes the hooks make at a point in the command stream.
#[derive(Debug, Clone, Copy)]
pub(super) enum Change {
    /// Draws from here belong to the HUD on this side (`Gx::set_hud_anchor`).
    Anchor(i8),
    /// The next shadow map is drawn from a widened light (`hle::mtx`).
    LightWidened,
}

/// The emulator's end of a worker.
pub(super) struct Worker {
    tx: Option<Sender<Msg>>,
    /// Finished frames not yet taken: the oldest are dropped past
    /// `MAX_QUEUED_FRAMES`, as when drawing in place.
    frames: Frames,
    /// FIFO bytes the worker has decoded.
    decoded: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
    /// Bytes not yet sent, and the settings last sent.
    batch: Vec<u8>,
    sent: (f32, Vec<(u32, f32)>),
}

impl Worker {
    /// Starts a worker drawing with `gx` from `ram`.
    pub(super) fn start(gx: Gx, ram: Ram<'static>) -> Worker {
        let (tx, rx) = channel();
        let frames: Frames = Arc::default();
        let (done, decoded) = (frames.clone(), Arc::new(AtomicU64::new(0)));
        let progress = decoded.clone();
        let sent = (gx.widescreen, gx.wide_cameras.clone());
        let thread = std::thread::Builder::new()
            .name("gx".into())
            .spawn(move || run(gx, ram, rx, done, progress))
            .expect("starting the graphics thread");
        Worker { tx: Some(tx), frames, decoded, thread: Some(thread), batch: Vec::with_capacity(BATCH), sent }
    }

    fn send(&self, msg: Msg) {
        if let Some(tx) = &self.tx {
            // A worker that has stopped (it panicked) draws nothing more.
            let _ = tx.send(msg);
        }
    }

    /// Queues executed FIFO bytes.
    pub(super) fn bytes(&mut self, data: &[u8], widescreen: f32, wide_cameras: &[(u32, f32)]) {
        if self.batch.is_empty() && (self.sent.0 != widescreen || self.sent.1 != wide_cameras) {
            self.sent = (widescreen, wide_cameras.to_vec());
            self.send(Msg::State { widescreen, wide_cameras: wide_cameras.to_vec() });
        }
        self.batch.extend_from_slice(data);
        if self.batch.len() >= BATCH {
            self.flush();
        }
    }

    /// Sends the bytes queued so far.
    pub(super) fn flush(&mut self) {
        if !self.batch.is_empty() {
            let batch = std::mem::replace(&mut self.batch, Vec::with_capacity(BATCH));
            self.send(Msg::Bytes(batch));
        }
    }

    /// A change at FIFO position `at`, which no byte sent so far reaches.
    pub(super) fn change(&self, at: u64, change: Change) {
        self.send(Msg::Change(at, change));
    }

    /// Waits until the worker has decoded the first `at` FIFO bytes.
    pub(super) fn wait(&mut self, at: u64) {
        self.flush();
        while self.decoded.load(Ordering::Acquire) < at {
            if self.thread.as_ref().is_none_or(|t| t.is_finished()) {
                return;
            }
            std::thread::yield_now();
        }
    }

    /// Frames the worker has finished, oldest first.
    pub(super) fn frames(&self) -> Vec<Vec<GpuCommand>> {
        self.frames.lock().map(|mut f| f.drain(..).collect()).unwrap_or_default()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Closing the channel ends the worker.
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

type Frames = Arc<Mutex<VecDeque<Vec<GpuCommand>>>>;

fn run(mut gx: Gx, ram: Ram<'static>, rx: Receiver<Msg>, frames: Frames, decoded: Arc<AtomicU64>) {
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Bytes(bytes) => {
                gx.pending.extend_from_slice(&bytes);
                gx.fifo_queued += bytes.len() as u64;
                gx.run_fifo(&ram);
                // Frames first: once the emulator sees the bytes decoded,
                // what they finished is there to take.
                let finished = gx.take_frames();
                if !finished.is_empty() {
                    let Ok(mut queue) = frames.lock() else { return };
                    for frame in finished {
                        if queue.len() >= MAX_QUEUED_FRAMES {
                            queue.pop_front();
                        }
                        queue.push_back(frame);
                    }
                }
                decoded.store(gx.fifo_decoded, Ordering::Release);
            }
            Msg::Change(at, change) => gx.changes.push_back((at, change)),
            Msg::State { widescreen, wide_cameras } => {
                gx.widescreen = widescreen;
                gx.wide_cameras = wide_cameras;
            }
        }
    }
}
