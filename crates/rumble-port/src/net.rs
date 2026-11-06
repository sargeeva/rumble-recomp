//! Online multiplayer: the host's server and the protocol clients speak to
//! it (see `docs/online.md`).
//!
//! A client connects over TCP and says hello; the host answers with its
//! name, so the player can see whose session it is and choose to connect.
//! Then the password both sides were given becomes a shared key through
//! SPAKE2, so it never crosses the network and a wrong one simply fails;
//! everything after that is sealed with ChaCha20-Poly1305. The client joins
//! with a name and the picture size it wants, and is given a player (2-4).
//! It sends its controller as Wii Remote buttons, which the host hands to
//! that player's remote; the host sends it the game's picture and sound,
//! and toasts.

use anyhow::{bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::ChaCha20Poly1305;
use rumble_runtime::input::CHANNELS;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 7777;
/// The longest player name, in characters.
pub const MAX_NAME: usize = 16;
/// Picture heights a joiner can ask for (16:9 widths follow).
pub const QUALITIES: [u16; 4] = [720, 1080, 1440, 2160];

const MAGIC: &[u8; 4] = b"RMBL";
const VERSION: u16 = 4;
/// How long the host waits for a client's hello.
const HELLO_TIME: Duration = Duration::from_secs(10);
/// How long the host waits while the player decides and types the password.
const PASSWORD_TIME: Duration = Duration::from_secs(180);
/// A peer that sends nothing for this long has lost its connection
/// (clients send their input every frame).
const SILENCE: Duration = Duration::from_secs(5);
/// The longest frame either side accepts (a 4K picture fits).
const MAX_FRAME: usize = 16 << 20;
/// Sound and toasts waiting for a joiner's connection (pictures don't
/// queue: only the newest waits). Past this, they are dropped rather than
/// slowing the game or the other joiners.
const OUTBOX_DEPTH: usize = 64;

/// What a frame holds, its first byte.
mod kind {
    /// Sealed with the session key.
    pub const SEALED: u8 = 0;
    /// A refusal in the clear (a wrong password, another version).
    pub const REFUSED: u8 = 1;
    /// Client: magic and version.
    pub const HELLO: u8 = 2;
    /// Host: its name and how many players are in.
    pub const INFO: u8 = 3;
    /// Either side's SPAKE2 message.
    pub const KEY: u8 = 4;
}

fn write_frame(s: &mut TcpStream, kind: u8, body: &[u8]) -> Result<()> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.extend_from_slice(&((body.len() + 1) as u32).to_be_bytes());
    out.push(kind);
    out.extend_from_slice(body);
    s.write_all(&out)?;
    Ok(())
}

/// The next frame. A refusal is an error carrying its reason.
fn read_frame(s: &mut TcpStream) -> Result<(u8, Vec<u8>)> {
    let mut len = [0; 4];
    s.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        bail!("bad frame");
    }
    let mut body = vec![0; len];
    s.read_exact(&mut body)?;
    let kind = body.remove(0);
    if kind == kind::REFUSED {
        bail!("{}", String::from_utf8_lossy(&body));
    }
    Ok((kind, body))
}

fn expect(s: &mut TcpStream, want: u8) -> Result<Vec<u8>> {
    let (kind, body) = read_frame(s)?;
    if kind != want {
        bail!("unexpected message");
    }
    Ok(body)
}

fn identities() -> (Identity, Identity) {
    (Identity::new(b"rumble-recomp client"), Identity::new(b"rumble-recomp host"))
}

/// The nonce for message `n` sent by `side` (0 client, 1 host): the
/// direction is part of it, so the two sides never seal with the same one,
/// and the count means none can be replayed or reordered.
fn nonce(side: u8, n: u64) -> [u8; 12] {
    let mut nonce = [0; 12];
    nonce[0] = side;
    nonce[4..].copy_from_slice(&n.to_be_bytes());
    nonce
}

/// The sending half of a connection after the handshake.
struct Sending {
    stream: TcpStream,
    cipher: ChaCha20Poly1305,
    side: u8,
    sent: u64,
}

impl Sending {
    fn send(&mut self, msg: &[u8]) -> Result<()> {
        let sealed = self.cipher.encrypt(&nonce(self.side, self.sent).into(), msg).map_err(|_| anyhow::anyhow!("sealing failed"))?;
        self.sent += 1;
        write_frame(&mut self.stream, kind::SEALED, &sealed)
    }

    fn refuse(&mut self, reason: &str) {
        let _ = write_frame(&mut self.stream, kind::REFUSED, reason.as_bytes());
    }
}

/// The receiving half.
struct Receiving {
    stream: TcpStream,
    cipher: ChaCha20Poly1305,
    side: u8,
    received: u64,
}

impl Receiving {
    fn receive(&mut self) -> Result<Vec<u8>> {
        let body = expect(&mut self.stream, kind::SEALED)?;
        let msg = self
            .cipher
            .decrypt(&nonce(1 - self.side, self.received).into(), body.as_slice())
            .map_err(|_| anyhow::anyhow!("a message didn't open"))?;
        self.received += 1;
        Ok(msg)
    }
}

/// Both halves of a connection whose shared key is `key`, for `side`.
fn link(stream: &TcpStream, key: &[u8], side: u8) -> Result<(Sending, Receiving)> {
    let cipher = ChaCha20Poly1305::new_from_slice(key).map_err(|_| anyhow::anyhow!("bad key"))?;
    Ok((
        Sending { stream: stream.try_clone()?, cipher: cipher.clone(), side, sent: 0 },
        Receiving { stream: stream.try_clone()?, cipher, side, received: 0 },
    ))
}

/// What a client sends, sealed.
#[derive(Debug, PartialEq)]
enum ToHost {
    /// `height` is the picture size it wants (one of `QUALITIES`); `h264`,
    /// whether it can decode H.264 (else pictures are JPEG).
    Join { name: String, height: u16, h264: bool },
    /// The Wii Remote buttons it holds, and its left stick (-1 to 1, y up)
    /// for true stick controls.
    Input(u32, [f32; 2]),
    Bye,
}

/// What the host sends, sealed.
#[derive(Debug, PartialEq)]
pub enum ToClient {
    /// Joined as `player` (1-4).
    Welcome { player: u8 },
    /// The game's picture, a JPEG.
    Picture(Vec<u8>),
    /// A frame of the game's picture as H.264 (Annex B), `width` by
    /// `height`: every one is needed, in order, from a key frame on.
    Video { width: u16, height: u16, key: bool, data: Vec<u8> },
    /// The game's sound: interleaved stereo samples played at `rate`.
    Sound { rate: u32, samples: Vec<i16> },
    /// Something to show, such as another player joining.
    Toast(String),
}

impl ToHost {
    fn encode(&self) -> Vec<u8> {
        match self {
            ToHost::Join { name, height, h264 } => [&[1][..], &height.to_be_bytes(), &[*h264 as u8], name.as_bytes()].concat(),
            ToHost::Input(bits, stick) => {
                let axis = |v: f32| ((v.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_be_bytes();
                [&[2][..], &bits.to_be_bytes(), &axis(stick[0]), &axis(stick[1])].concat()
            }
            ToHost::Bye => vec![3],
        }
    }

    fn decode(b: &[u8]) -> Option<Self> {
        Some(match b.split_first()? {
            (1, rest) if rest.len() >= 3 => ToHost::Join {
                height: u16::from_be_bytes([rest[0], rest[1]]),
                h264: rest[2] & 1 != 0,
                name: String::from_utf8(rest[3..].to_vec()).ok()?,
            },
            (2, [b0, b1, b2, b3, x0, x1, y0, y1]) => {
                let axis = |hi: u8, lo: u8| i16::from_be_bytes([hi, lo]) as f32 / 32767.0;
                ToHost::Input(u32::from_be_bytes([*b0, *b1, *b2, *b3]), [axis(*x0, *x1), axis(*y0, *y1)])
            }
            (3, []) => ToHost::Bye,
            _ => return None,
        })
    }
}

impl ToClient {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            ToClient::Welcome { player } => vec![10, *player],
            ToClient::Picture(jpeg) => [&[11], jpeg.as_slice()].concat(),
            ToClient::Sound { rate, samples } => {
                let mut out = Vec::with_capacity(5 + samples.len() * 2);
                out.push(12);
                out.extend_from_slice(&rate.to_be_bytes());
                for s in samples {
                    out.extend_from_slice(&s.to_le_bytes());
                }
                out
            }
            ToClient::Toast(text) => [&[13], text.as_bytes()].concat(),
            ToClient::Video { width, height, key, data } => {
                [&[14][..], &width.to_be_bytes(), &height.to_be_bytes(), &[*key as u8], data].concat()
            }
        }
    }

    fn decode(b: &[u8]) -> Option<Self> {
        Some(match b.split_first()? {
            (10, [player]) => ToClient::Welcome { player: *player },
            (11, jpeg) => ToClient::Picture(jpeg.to_vec()),
            (12, rest) if rest.len() >= 4 => ToClient::Sound {
                rate: u32::from_be_bytes(rest[..4].try_into().unwrap()),
                samples: rest[4..].chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]])).collect(),
            },
            (13, text) => ToClient::Toast(String::from_utf8_lossy(text).into_owned()),
            (14, rest) if rest.len() >= 5 => ToClient::Video {
                width: u16::from_be_bytes([rest[0], rest[1]]),
                height: u16::from_be_bytes([rest[2], rest[3]]),
                key: rest[4] & 1 != 0,
                data: rest[5..].to_vec(),
            },
            _ => return None,
        })
    }
}

/// A name as players see it: without control characters, trimmed, at most
/// `MAX_NAME` characters. Empty if nothing is left.
pub fn clean_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    name.trim().chars().take(MAX_NAME).collect::<String>().trim_end().to_string()
}

/// A connected online player, as the host sees it.
pub struct Peer {
    pub name: String,
    /// The Wii Remote buttons it holds.
    pub buttons: u32,
    /// Its left stick (-1 to 1, y up).
    pub stick: [f32; 2],
    stream: TcpStream,
    /// The host ended multiplayer and is disconnecting it.
    removed: bool,
}

/// Who plays where. Channel 0 is the host.
#[derive(Default)]
pub struct Lobby {
    pub players: [Option<Peer>; CHANNELS],
    /// Channels this PC's own gamepads play on; online players take others.
    pub local: [bool; CHANNELS],
}

impl Lobby {
    fn free_channel(&self) -> Option<usize> {
        (1..CHANNELS).find(|&c| !self.local[c] && self.players[c].is_none())
    }

    fn count(&self) -> usize {
        1 + self.players.iter().flatten().count()
    }
}

/// What a joiner's sending thread is told to do next.
enum Job {
    /// Send this (sound or a toast).
    Send(Arc<Vec<u8>>),
    /// Send the newest picture, if it hasn't been sent yet.
    Picture,
}

/// A joiner's queue of things to send, its newest unsent picture (a newer
/// one replaces it, so pictures never pile up behind a slow connection),
/// the picture size it wants and whether it takes H.264. H.264 frames go
/// through the queue, as each builds on the one before: one that doesn't
/// fit leaves the joiner waiting (`need_key`) for the next key frame.
struct Subscriber {
    chan: usize,
    height: u16,
    queue: SyncSender<Job>,
    picture: Arc<Mutex<Option<Arc<Vec<u8>>>>>,
    h264: bool,
    need_key: bool,
}

/// What the host sends to every joiner: pictures, sound and toasts. The
/// game's threads hand them over here; each joiner's connection sends them
/// from a thread of its own. `want_key` asks the encoder for a key frame,
/// for a joiner just in or one that fell behind.
#[derive(Default)]
pub struct Outbox {
    subscribers: Mutex<Vec<Subscriber>>,
    want_key: AtomicBool,
}

impl Outbox {
    /// Whether anyone is listening (so pictures and sound are worth making).
    pub fn listening(&self) -> bool {
        !self.subscribers.lock().unwrap().is_empty()
    }

    /// Whether every joiner takes H.264 (otherwise pictures go as JPEG).
    pub fn h264(&self) -> bool {
        let subscribers = self.subscribers.lock().unwrap();
        !subscribers.is_empty() && subscribers.iter().all(|s| s.h264)
    }

    /// Whether a key frame is wanted (asking clears it).
    pub fn take_want_key(&self) -> bool {
        self.want_key.swap(false, Ordering::Relaxed)
    }

    /// The tallest picture any joiner asked for.
    pub fn height(&self) -> Option<u16> {
        self.subscribers.lock().unwrap().iter().map(|s| s.height).max()
    }

    /// Sends `msg` to every joiner, dropping it for any whose queue is full.
    pub fn send(&self, msg: &ToClient) {
        let mut subscribers = self.subscribers.lock().unwrap();
        if subscribers.is_empty() {
            return;
        }
        let bytes = Arc::new(msg.encode());
        if let ToClient::Video { key, .. } = msg {
            for s in subscribers.iter_mut().filter(|s| s.h264 && (*key || !s.need_key)) {
                match s.queue.try_send(Job::Send(bytes.clone())) {
                    Ok(()) => s.need_key = false,
                    Err(_) => {
                        s.need_key = true;
                        self.want_key.store(true, Ordering::Relaxed);
                    }
                }
            }
            return;
        }
        for s in subscribers.iter() {
            let job = if matches!(msg, ToClient::Picture(_)) {
                *s.picture.lock().unwrap() = Some(bytes.clone());
                Job::Picture
            } else {
                Job::Send(bytes.clone())
            };
            match s.queue.try_send(job) {
                Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }
}

struct Shared {
    lobby: Mutex<Lobby>,
    outbox: Arc<Outbox>,
    password: String,
    name: String,
    stop: AtomicBool,
    port_opening: Mutex<PortOpening>,
}

/// Opening the host's port on the router with UPnP, so players outside its
/// network can reach it.
#[derive(Clone, PartialEq, Debug)]
pub enum PortOpening {
    Trying,
    /// Open, with the router's public address if it said.
    Open(Option<IpAddr>),
    /// No UPnP router answered, or it refused.
    Failed,
}

/// How long a port mapping lasts, and how often it is renewed while
/// hosting (a router may forget one without a lease, or drop leases).
const LEASE: Duration = Duration::from_secs(60 * 60);
const RENEW: Duration = Duration::from_secs(30 * 60);

/// Opens `port` on the router to `lan` while `shared` hosts, renewing it,
/// and closes it after.
fn open_port(port: u16, lan: Option<IpAddr>, shared: Arc<Shared>) {
    use igd_next::{search_gateway, PortMappingProtocol, SearchOptions};
    let set = |state| *shared.port_opening.lock().unwrap() = state;
    // Tests host on whatever port is free: none of that on the router.
    if cfg!(test) {
        set(PortOpening::Failed);
        return;
    }
    let mut options = SearchOptions::default();
    options.timeout = Some(Duration::from_secs(3));
    let (Some(lan), Ok(gateway)) = (lan, search_gateway(options)) else {
        set(PortOpening::Failed);
        return;
    };
    let local = SocketAddr::new(lan, port);
    let add = || gateway.add_port(PortMappingProtocol::TCP, port, local, LEASE.as_secs() as u32, "Pokemon Rumble");
    if add().is_err() {
        set(PortOpening::Failed);
        return;
    }
    set(PortOpening::Open(gateway.get_external_ip().ok()));
    let mut since = std::time::Instant::now();
    while !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
        if since.elapsed() >= RENEW {
            let _ = add();
            since = std::time::Instant::now();
        }
    }
    let _ = gateway.remove_port(PortMappingProtocol::TCP, port);
}

/// The host's server, running while it exists.
pub struct Host {
    /// The port it listens on.
    pub port: u16,
    pub name: String,
    /// This PC's address on its local network, for players on the same one.
    pub lan: Option<IpAddr>,
    shared: Arc<Shared>,
    /// Joins, leaves and refusals, to show as toasts.
    pub events: Receiver<String>,
}

impl Host {
    /// Starts listening on `port` (0 for any) for players who know
    /// `password`, in `name`'s session; what `outbox` is given goes to
    /// every joiner.
    pub fn start(port: u16, password: &str, name: &str, outbox: Arc<Outbox>) -> Result<Self> {
        let name = clean_name(name);
        if name.is_empty() {
            bail!("set your name first");
        }
        if password.is_empty() {
            bail!("set a password first");
        }
        let listener = TcpListener::bind(("0.0.0.0", port)).with_context(|| format!("can't listen on port {port}"))?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let shared = Arc::new(Shared {
            lobby: Mutex::new(Lobby::default()),
            outbox,
            password: password.to_string(),
            name: name.clone(),
            stop: AtomicBool::new(false),
            port_opening: Mutex::new(PortOpening::Trying),
        });
        let (tx, events) = channel();
        let s = shared.clone();
        std::thread::Builder::new().name("host".into()).spawn(move || accept(listener, s, tx))?;
        let lan = lan_address();
        let s = shared.clone();
        std::thread::Builder::new().name("upnp".into()).spawn(move || open_port(port, lan, s))?;
        Ok(Self { port, name, lan, shared, events })
    }

    /// How opening the port on the router is going.
    pub fn port_opening(&self) -> PortOpening {
        self.shared.port_opening.lock().unwrap().clone()
    }

    /// The lobby, to read players' input from and say which channels this
    /// PC's own gamepads play on.
    pub fn lobby(&self) -> MutexGuard<'_, Lobby> {
        self.shared.lobby.lock().unwrap()
    }

    /// Disconnects every online player, telling them the host ended
    /// multiplayer. The session stays open for new players.
    pub fn remove_all(&self) {
        let msg = format!("{} ended multiplayer", self.name);
        self.shared.outbox.send(&ToClient::Toast(msg));
        // Their toasts go out before the connections close.
        std::thread::sleep(Duration::from_millis(100));
        for p in self.lobby().players.iter_mut().flatten() {
            p.removed = true;
            let _ = p.stream.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for p in self.lobby().players.iter().flatten() {
            let _ = p.stream.shutdown(Shutdown::Both);
        }
    }
}

fn accept(listener: TcpListener, shared: Arc<Shared>, events: Sender<String>) {
    while !shared.stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, addr)) => {
                let (s, e) = (shared.clone(), events.clone());
                let _ = std::thread::Builder::new().name("player".into()).spawn(move || {
                    let _ = serve(stream, addr, &s, &e);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Tells everyone, the host and the joiners, that something happened.
fn announce(shared: &Shared, events: &Sender<String>, msg: String) {
    shared.outbox.send(&ToClient::Toast(msg.clone()));
    let _ = events.send(msg);
}

/// One client, from its hello until it leaves.
fn serve(mut s: TcpStream, addr: SocketAddr, shared: &Shared, events: &Sender<String>) -> Result<()> {
    s.set_nonblocking(false)?;
    s.set_nodelay(true)?;
    s.set_read_timeout(Some(HELLO_TIME))?;
    let hello = expect(&mut s, kind::HELLO)?;
    if hello.len() != 6 || &hello[..4] != MAGIC {
        bail!("not a client");
    }
    if u16::from_be_bytes([hello[4], hello[5]]) != VERSION {
        write_frame(&mut s, kind::REFUSED, b"the host runs a different version of the game")?;
        bail!("another version");
    }
    let count = shared.lobby.lock().unwrap().count() as u8;
    write_frame(&mut s, kind::INFO, &[&[count][..], shared.name.as_bytes()].concat())?;

    // The player decides and types the password.
    s.set_read_timeout(Some(PASSWORD_TIME))?;
    let theirs = expect(&mut s, kind::KEY)?;
    let (a, b) = identities();
    let (spake, ours) = Spake2::<Ed25519Group>::start_b(&Password::new(&shared.password), &a, &b);
    write_frame(&mut s, kind::KEY, &ours)?;
    let key = spake.finish(&theirs).map_err(|_| anyhow::anyhow!("bad handshake"))?;
    s.set_read_timeout(Some(HELLO_TIME))?;
    let (mut sending, mut receiving) = link(&s, &key, 1)?;

    // The first sealed message only opens with the right password.
    let (name, height, h264) = match receiving.receive().ok().and_then(|m| ToHost::decode(&m)) {
        Some(ToHost::Join { name, height, h264 }) => (clean_name(&name), height, h264),
        _ => {
            // Slowly, so guessing takes time.
            std::thread::sleep(Duration::from_secs(1));
            sending.refuse("wrong password");
            let _ = events.send(format!("Someone at {} tried the wrong password", addr.ip()));
            return Ok(());
        }
    };
    let height = QUALITIES.into_iter().min_by_key(|q| q.abs_diff(height)).unwrap();
    let chan = {
        let mut lobby = shared.lobby.lock().unwrap();
        let taken = name.eq_ignore_ascii_case(&shared.name) || lobby.players.iter().flatten().any(|p| p.name.eq_ignore_ascii_case(&name));
        let refusal = if name.is_empty() {
            Some("choose a name")
        } else if taken {
            Some("that name is taken")
        } else if lobby.free_channel().is_none() {
            Some("the game is full")
        } else {
            None
        };
        if let Some(reason) = refusal {
            drop(lobby);
            sending.refuse(reason);
            return Ok(());
        }
        let chan = lobby.free_channel().unwrap();
        lobby.players[chan] = Some(Peer { name: name.clone(), buttons: 0, stick: [0.0; 2], stream: s.try_clone()?, removed: false });
        chan
    };
    let player = chan as u8 + 1;
    sending.send(&ToClient::Welcome { player }.encode())?;
    announce(shared, events, format!("{name} joined as Player {player}"));

    // From here this joiner is sent what the outbox gets, from a thread of
    // its own.
    let (queue, outgoing) = sync_channel::<Job>(OUTBOX_DEPTH);
    let picture = Arc::new(Mutex::new(None));
    shared.outbox.subscribers.lock().unwrap().push(Subscriber { chan, height, queue, picture: picture.clone(), h264, need_key: true });
    shared.outbox.want_key.store(true, Ordering::Relaxed);
    std::thread::Builder::new().name("player send".into()).spawn(move || {
        for job in outgoing {
            let msg = match job {
                Job::Send(msg) => msg,
                Job::Picture => match picture.lock().unwrap().take() {
                    Some(msg) => msg,
                    None => continue,
                },
            };
            if sending.send(&msg).is_err() {
                break;
            }
        }
    })?;
    s.set_read_timeout(Some(SILENCE))?;

    let left = loop {
        if shared.stop.load(Ordering::Relaxed) {
            break None;
        }
        match receiving.receive().ok().and_then(|m| ToHost::decode(&m)) {
            Some(ToHost::Input(bits, stick)) => {
                if let Some(p) = shared.lobby.lock().unwrap().players[chan].as_mut() {
                    p.buttons = bits;
                    p.stick = stick;
                }
            }
            Some(ToHost::Bye) => break Some(format!("{name} left")),
            Some(ToHost::Join { .. }) | None => {
                let removed = shared.lobby.lock().unwrap().players[chan].as_ref().is_some_and(|p| p.removed);
                break (!removed).then(|| format!("{name} lost connection"));
            }
        }
    };
    // Dropping its queue ends its sending thread.
    shared.outbox.subscribers.lock().unwrap().retain(|sub| sub.chan != chan);
    shared.lobby.lock().unwrap().players[chan] = None;
    let _ = s.shutdown(Shutdown::Both);
    if let Some(msg) = left {
        announce(shared, events, msg);
    }
    Ok(())
}

/// A session found at an address, before the player decides to join it.
pub struct Session {
    stream: TcpStream,
    /// The host's name.
    pub host: String,
    /// Players in it, the host included.
    pub players: u8,
}

impl Session {
    /// Asks `addr` (`ip:port`, or `ip` for the default port) whose session
    /// it is.
    pub fn find(addr: &str) -> Result<Self> {
        let addr = addr.trim();
        let addr = if addr.contains(':') { addr.to_string() } else { format!("{addr}:{DEFAULT_PORT}") };
        let mut s = TcpStream::connect(&addr).with_context(|| format!("no session found at {addr}"))?;
        s.set_nodelay(true)?;
        s.set_read_timeout(Some(HELLO_TIME))?;
        write_frame(&mut s, kind::HELLO, &[&MAGIC[..], &VERSION.to_be_bytes()].concat())?;
        let info = expect(&mut s, kind::INFO).context("the host didn't answer")?;
        let (&players, host) = info.split_first().context("the host didn't answer")?;
        Ok(Self { stream: s, host: String::from_utf8_lossy(host).into_owned(), players })
    }

    /// Joins as `name` with `password`, asking for pictures `height` tall,
    /// as H.264 if `h264` (it can decode it).
    pub fn join(mut self, password: &str, name: &str, height: u16, h264: bool) -> Result<Client> {
        let name = clean_name(name);
        if name.is_empty() {
            bail!("choose a name");
        }
        let (a, b) = identities();
        let (spake, ours) = Spake2::<Ed25519Group>::start_a(&Password::new(password), &a, &b);
        write_frame(&mut self.stream, kind::KEY, &ours)?;
        let theirs = expect(&mut self.stream, kind::KEY)?;
        let key = spake.finish(&theirs).map_err(|_| anyhow::anyhow!("bad handshake"))?;
        let (mut sending, mut receiving) = link(&self.stream, &key, 0)?;
        sending.send(&ToHost::Join { name, height, h264 }.encode())?;
        match receiving.receive().map(|m| ToClient::decode(&m)) {
            Ok(Some(ToClient::Welcome { player })) => {
                self.stream.set_read_timeout(None)?;
                Ok(Client { sending, receiving: Some(receiving), player, host: self.host })
            }
            Ok(_) => bail!("the host answered something unexpected"),
            Err(e) => Err(e),
        }
    }
}

/// A client in a host's session.
pub struct Client {
    sending: Sending,
    receiving: Option<Receiving>,
    /// The player it plays as (2-4).
    pub player: u8,
    pub host: String,
}

impl Client {
    pub fn send_input(&mut self, bits: u32, stick: [f32; 2]) -> Result<()> {
        self.sending.send(&ToHost::Input(bits, stick).encode())
    }

    /// What the host sends from now on, read on a thread of its own: each
    /// message, then `None` when the connection closes. Only the first call
    /// gets them.
    pub fn messages(&mut self) -> Receiver<Option<ToClient>> {
        let (tx, rx) = channel();
        if let Some(mut receiving) = self.receiving.take() {
            let _ = std::thread::Builder::new().name("from host".into()).spawn(move || loop {
                match receiving.receive() {
                    Ok(m) => {
                        if let Some(msg) = ToClient::decode(&m) {
                            if tx.send(Some(msg)).is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => {
                        let _ = tx.send(None);
                        break;
                    }
                }
            });
        }
        rx
    }

    pub fn leave(mut self) {
        let _ = self.sending.send(&ToHost::Bye.encode());
        let _ = self.sending.stream.shutdown(Shutdown::Both);
    }
}

/// This PC's address on its local network.
fn lan_address() -> Option<IpAddr> {
    // Connecting a UDP socket sends nothing; it only picks the route.
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    Some(s.local_addr().ok()?.ip())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next(events: &Receiver<String>) -> String {
        events.recv_timeout(Duration::from_secs(5)).unwrap()
    }

    #[test]
    fn messages_round_trip() {
        for m in [ToHost::Join { name: "Ash".into(), height: 1080, h264: true }, ToHost::Input(0x0900, [1.0, -1.0]), ToHost::Bye] {
            assert_eq!(ToHost::decode(&m.encode()), Some(m));
        }
        for m in [
            ToClient::Welcome { player: 3 },
            ToClient::Picture(vec![0xff, 0xd8, 1, 2]),
            ToClient::Sound { rate: 32000, samples: vec![-1, 2, i16::MIN, i16::MAX] },
            ToClient::Toast("Ash joined".into()),
            ToClient::Video { width: 1920, height: 1080, key: true, data: vec![0, 0, 1, 0x65, 9] },
        ] {
            assert_eq!(ToClient::decode(&m.encode()), Some(m));
        }
        assert_eq!(clean_name("  Ash\n Ketchum the very long name "), "Ash Ketchum the");
        assert_eq!(clean_name(" \t "), "");
    }

    /// Clients find the session, join, play and leave; a wrong password, a
    /// taken name and a full game are refused; what the outbox gets reaches
    /// the joiners.
    /// A joiner gets H.264 from a key frame on; one whose queue is full
    /// waits for the next, which the encoder is asked for.
    #[test]
    fn video_from_key_frames() {
        let outbox = Outbox::default();
        let (queue, sent) = sync_channel(2);
        let picture = Arc::new(Mutex::new(None));
        outbox.subscribers.lock().unwrap().push(Subscriber { chan: 1, height: 720, queue, picture, h264: true, need_key: true });
        let frame = |key: bool, n: u8| ToClient::Video { width: 2, height: 2, key, data: vec![n] };
        let got = || sent.try_iter().map(|j| match j { Job::Send(b) => ToClient::decode(&b).unwrap(), Job::Picture => unreachable!() }).collect::<Vec<_>>();
        outbox.send(&frame(false, 1));
        assert!(got().is_empty(), "nothing before a key frame");
        outbox.send(&frame(true, 2));
        outbox.send(&frame(false, 3));
        assert_eq!(got(), [frame(true, 2), frame(false, 3)]);
        assert!(!outbox.take_want_key());
        for n in 4..8 {
            outbox.send(&frame(false, n));
        }
        assert!(outbox.take_want_key(), "fell behind");
        assert_eq!(got(), [frame(false, 4), frame(false, 5)]);
        outbox.send(&frame(false, 8));
        assert!(got().is_empty(), "waiting for a key frame");
        outbox.send(&frame(true, 9));
        assert_eq!(got(), [frame(true, 9)]);
    }

    #[test]
    fn host_and_clients() {
        let outbox = Arc::new(Outbox::default());
        let host = Host::start(0, "hunter2", "Red", outbox.clone()).unwrap();
        let addr = format!("127.0.0.1:{}", host.port);

        let session = Session::find(&addr).unwrap();
        assert_eq!((session.host.as_str(), session.players), ("Red", 1));
        let err = session.join("hunter3", "Ash", 1080, false).err().unwrap();
        assert!(err.to_string().contains("wrong password"), "{err:#}");
        assert!(next(&host.events).contains("wrong password"));

        let mut ash = Session::find(&addr).unwrap().join("hunter2", "Ash", 1000, false).unwrap();
        assert_eq!(ash.player, 2);
        assert_eq!(next(&host.events), "Ash joined as Player 2");
        let from_host = ash.messages();
        ash.send_input(0x0100, [0.0; 2]).unwrap();
        for _ in 0..50 {
            if host.lobby().players[1].as_ref().is_some_and(|p| p.buttons == 0x0100) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(host.lobby().players[1].as_ref().unwrap().buttons, 0x0100);
        // 1000 is closest to 1080.
        assert_eq!(outbox.height(), Some(1080));
        outbox.send(&ToClient::Picture(vec![1, 2, 3]));
        let got = from_host.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(got, Some(ToClient::Picture(vec![1, 2, 3])));

        let err = Session::find(&addr).unwrap().join("hunter2", "ash", 720, false).err().unwrap();
        assert!(err.to_string().contains("taken"), "{err:#}");
        let err = Session::find(&addr).unwrap().join("hunter2", "red", 720, false).err().unwrap();
        assert!(err.to_string().contains("taken"), "{err:#}");

        // This PC's own pad plays as player 3; online players get 4, then
        // the game is full. Ash hears that Misty joined.
        host.lobby().local[2] = true;
        let misty = Session::find(&addr).unwrap().join("hunter2", "Misty", 2160, false).unwrap();
        assert_eq!(misty.player, 4);
        assert_eq!(next(&host.events), "Misty joined as Player 4");
        assert_eq!(from_host.recv_timeout(Duration::from_secs(5)).unwrap(), Some(ToClient::Toast("Misty joined as Player 4".into())));
        assert_eq!(outbox.height(), Some(2160));
        let err = Session::find(&addr).unwrap().join("hunter2", "Brock", 720, false).err().unwrap();
        assert!(err.to_string().contains("full"), "{err:#}");

        ash.leave();
        assert_eq!(next(&host.events), "Ash left");
        assert!(host.lobby().players[1].is_none());
        drop(misty);
        assert_eq!(next(&host.events), "Misty lost connection");
        assert!(!outbox.listening());
    }
}
