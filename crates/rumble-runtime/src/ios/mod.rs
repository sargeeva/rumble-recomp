//! High-level emulation of IOS, the Starlet-side OS the game talks to over
//! IPC. A request is a 0x20-byte block in guest memory (physical address):
//!
//! | off | field                                      |
//! |-----|--------------------------------------------|
//! | 0x0 | command (1 open … 7 ioctlv; IOS sets 8)    |
//! | 0x4 | result                                     |
//! | 0x8 | fd (IOS overwrites it with the command)    |
//! | 0xC | up to five arguments                       |
//!
//! Buffer pointers inside requests are physical addresses.

mod di;
mod es;
mod fs;
mod ncd;
mod stm;

use crate::mem::{Fault, Memory};
use std::path::PathBuf;

const OPEN: u32 = 1;
const CLOSE: u32 = 2;
const READ: u32 = 3;
const WRITE: u32 = 4;
const SEEK: u32 = 5;
const IOCTL: u32 = 6;
const IOCTLV: u32 = 7;
const REPLY: u32 = 8;

/// IOS error codes.
pub const EINVAL: i32 = -4;
pub const ENOENT: i32 = -6;

/// The title IOS is running, as ES and the NAND see it.
pub struct Title {
    pub id: u64,
    pub tmd: Vec<u8>,
    pub ticket: Vec<u8>,
    /// Decrypted contents, named `<content id>.app`.
    pub contents_dir: PathBuf,
    /// Host folder standing in for the NAND filesystem.
    pub nand_dir: PathBuf,
}

/// An ioctl's buffers: `ioctl` has one input and one output, `ioctlv` any
/// number of each. Entries are (physical address, length).
pub struct Buffers {
    pub input: Vec<(u32, u32)>,
    pub output: Vec<(u32, u32)>,
}

impl Buffers {
    pub fn in_u32(&self, m: &mut Memory, k: usize, off: u32) -> Result<u32, Fault> {
        m.r32(self.input.get(k).map_or(0, |b| b.0) + off)
    }

    pub fn in_u64(&self, m: &mut Memory, k: usize) -> Result<u64, Fault> {
        m.r64(self.input.get(k).map_or(0, |b| b.0))
    }

    pub fn in_str(&self, m: &mut Memory, k: usize, off: u32) -> Result<String, Fault> {
        read_cstr(m, self.input.get(k).map_or(0, |b| b.0) + off, 64)
    }

    /// Writes `data` to output buffer `k`, truncated to the buffer's size.
    pub fn out(&self, m: &mut Memory, k: usize, data: &[u8]) -> Result<(), Fault> {
        match self.output.get(k) {
            Some(&(addr, len)) => m.load(addr, &data[..data.len().min(len as usize)]),
            None => Ok(()),
        }
    }

    pub fn out_u32(&self, m: &mut Memory, k: usize, v: u32) -> Result<(), Fault> {
        self.out(m, k, &v.to_be_bytes())
    }
}

enum Handle {
    Ncd,
    Di,
    Es,
    Fs,
    File(fs::File),
    StmImmediate,
    StmEventHook,
}

#[derive(Default)]
pub struct Ios {
    title: Option<Title>,
    fds: Vec<Option<Handle>>,
    di: di::Di,
    es: es::Es,
    stm: stm::Stm,
    /// One line per request, for the `boot` report.
    pub log: Vec<String>,
}

impl Ios {
    pub fn new(title: Title) -> Self {
        fs::prepare_nand(&title);
        Self { title: Some(title), ..Default::default() }
    }

    /// Where the console's NAND is kept on the host, if a title is loaded.
    pub fn nand_dir(&self) -> Option<&std::path::Path> {
        self.title.as_ref().map(|t| t.nand_dir.as_path())
    }

    /// Executes the request at physical address `req`. Returns the requests
    /// that now have replies written into them, in order; usually just
    /// `req`, but some requests (an STM event hook) stay pending until a
    /// later one completes them.
    pub fn execute(&mut self, m: &mut Memory, req: u32) -> Result<Vec<u32>, Fault> {
        let cmd = m.r32(req)?;
        let fd = m.r32(req + 8)?;
        let args: Vec<u32> = (0..5).map(|k| m.r32(req + 0xc + 4 * k)).collect::<Result<_, _>>()?;
        let mut done = vec![req];
        let (what, result) = match cmd {
            OPEN => {
                let path = read_cstr(m, args[0], 64)?;
                let r = self.open(&path, args[1]);
                (format!("open {path:?} mode {}", args[1]), r)
            }
            CLOSE => {
                let r = match self.fds.get_mut(fd as usize).and_then(Option::take) {
                    Some(Handle::StmEventHook) => {
                        done.extend(self.stm.release(m)?);
                        0
                    }
                    Some(_) => 0,
                    None => EINVAL,
                };
                (format!("close fd {fd}"), r)
            }
            READ | WRITE | SEEK => {
                let name = ["read", "write", "seek"][(cmd - READ) as usize];
                let r = match self.fds.get_mut(fd as usize).and_then(Option::as_mut) {
                    Some(Handle::File(f)) => match cmd {
                        READ => f.read(m, args[0], args[1])?,
                        WRITE => f.write(m, args[0], args[1])?,
                        _ => f.seek(args[0] as i32, args[1]),
                    },
                    Some(_) => EINVAL,
                    None => EINVAL,
                };
                (format!("{name} fd {fd} args {:x?}", &args[..2]), r)
            }
            IOCTL | IOCTLV => {
                let num = args[0];
                let bufs = if cmd == IOCTL {
                    Buffers { input: vec![(args[1], args[2])], output: vec![(args[3], args[4])] }
                } else {
                    let (n_in, n_io, vec) = (args[1], args[2], args[3]);
                    let mut all = Vec::new();
                    for k in 0..n_in + n_io {
                        all.push((m.r32(vec + 8 * k)?, m.r32(vec + 8 * k + 4)?));
                    }
                    let output = all.split_off(n_in as usize);
                    Buffers { input: all, output }
                };
                let (dev, r) = self.ioctl(m, fd, num, &bufs, req, &mut done)?;
                let kind = if cmd == IOCTL { "ioctl" } else { "ioctlv" };
                let path = if dev == "/dev/fs" { fs::describe(m, num, &bufs) } else { None };
                let path = path.map_or_else(String::new, |p| format!(" {p:?}"));
                (format!("{kind} 0x{num:x} on {dev}{path}"), r)
            }
            _ => (format!("unknown command {cmd}"), EINVAL),
        };
        if done.first() == Some(&req) {
            m.w32(req + 4, result as u32)?;
            m.w32(req + 8, cmd)?;
            m.w32(req, REPLY)?;
            self.log.push(format!("{what} -> {result}"));
        } else {
            self.log.push(format!("{what} -> (pending)"));
        }
        Ok(done)
    }

    fn open(&mut self, path: &str, mode: u32) -> i32 {
        let handle = match path {
            "/dev/di" => Handle::Di,
            "/dev/es" => Handle::Es,
            "/dev/fs" => Handle::Fs,
            "/dev/net/ncd/manage" => Handle::Ncd,
            "/dev/stm/immediate" => Handle::StmImmediate,
            "/dev/stm/eventhook" => Handle::StmEventHook,
            _ if path.starts_with("/dev/") => return ENOENT,
            _ => match fs::File::open(self.title.as_ref(), path, mode) {
                Ok(f) => Handle::File(f),
                Err(e) => return e,
            },
        };
        let fd = match self.fds.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.fds.push(None);
                self.fds.len() - 1
            }
        };
        self.fds[fd] = Some(handle);
        fd as i32
    }

    fn ioctl(
        &mut self,
        m: &mut Memory,
        fd: u32,
        num: u32,
        bufs: &Buffers,
        req: u32,
        done: &mut Vec<u32>,
    ) -> Result<(&'static str, i32), Fault> {
        let title = self.title.as_ref();
        Ok(match self.fds.get_mut(fd as usize).and_then(Option::as_mut) {
            Some(Handle::Di) => ("/dev/di", self.di.ioctl(m, num, bufs)?),
            Some(Handle::Ncd) => ("/dev/net/ncd/manage", ncd::ioctlv(m, num, bufs)?),
            Some(Handle::Es) => ("/dev/es", self.es.ioctl(m, title, num, bufs)?),
            Some(Handle::Fs) => ("/dev/fs", fs::ioctl(m, title, num, bufs)?),
            Some(Handle::File(f)) => ("file", f.ioctl(m, num, bufs)?),
            Some(Handle::StmImmediate) => {
                let (r, released) = self.stm.immediate(m, num, bufs)?;
                done.extend(released);
                ("/dev/stm/immediate", r)
            }
            Some(Handle::StmEventHook) => {
                if self.stm.hook(num, req) {
                    done.clear();
                }
                ("/dev/stm/eventhook", 0)
            }
            None => ("bad fd", EINVAL),
        })
    }
}

fn read_cstr(m: &mut Memory, addr: u32, max: u32) -> Result<String, Fault> {
    let mut s = Vec::new();
    for a in addr..addr + max {
        match m.r8(a)? {
            0 => break,
            b => s.push(b),
        }
    }
    Ok(String::from_utf8_lossy(&s).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQ: u32 = 0x1000;
    const VEC: u32 = 0x1100;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rumble-ios-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A title with two contents (IDs 0x10 and 0x11, indices 0 and 1).
    fn title(dir: &std::path::Path) -> Title {
        let mut tmd = vec![0u8; 0x1e4 + 2 * 36];
        tmd[0x18c..0x194].copy_from_slice(&0x0001_0001_5750_5345u64.to_be_bytes());
        tmd[0x1de..0x1e0].copy_from_slice(&2u16.to_be_bytes());
        for (i, id) in [0x10u32, 0x11].into_iter().enumerate() {
            let r = 0x1e4 + i * 36;
            tmd[r..r + 4].copy_from_slice(&id.to_be_bytes());
            tmd[r + 4..r + 6].copy_from_slice(&(i as u16).to_be_bytes());
            tmd[r + 8..r + 16].copy_from_slice(&11u64.to_be_bytes());
        }
        let contents = dir.join("contents");
        std::fs::create_dir_all(&contents).unwrap();
        std::fs::write(contents.join("00000011.app"), b"hello world").unwrap();
        Title { id: 0x0001_0001_5750_5345, tmd, ticket: vec![0; 0x2a4], contents_dir: contents, nand_dir: dir.join("nand") }
    }

    fn send(m: &mut Memory, cmd: u32, fd: u32, args: &[u32]) -> i32 {
        m.w32(REQ, cmd).unwrap();
        m.w32(REQ + 8, fd).unwrap();
        for (k, a) in args.iter().enumerate() {
            m.w32(REQ + 0xc + 4 * k as u32, *a).unwrap();
        }
        let mut ios = std::mem::take(&mut m.ios);
        let done = ios.execute(m, REQ).unwrap();
        m.ios = ios;
        assert_eq!(done, [REQ]);
        m.r32(REQ + 4).unwrap() as i32
    }

    fn open(m: &mut Memory, path: &str, mode: u32) -> i32 {
        m.load(0x1200, format!("{path}\0").as_bytes()).unwrap();
        send(m, OPEN, 0, &[0x1200, mode])
    }

    /// ioctlv with the given (address, length) vectors.
    fn ioctlv(m: &mut Memory, fd: i32, num: u32, input: &[(u32, u32)], output: &[(u32, u32)]) -> i32 {
        for (k, (a, l)) in input.iter().chain(output).enumerate() {
            m.w32(VEC + 8 * k as u32, *a).unwrap();
            m.w32(VEC + 8 * k as u32 + 4, *l).unwrap();
        }
        send(m, IOCTLV, fd as u32, &[num, input.len() as u32, output.len() as u32, VEC])
    }

    #[test]
    fn es_views_and_content() {
        let dir = temp_dir("es");
        let mut m = Memory::default();
        m.ios = Ios::new(title(&dir));
        let es = open(&mut m, "/dev/es", 0);
        assert!(es >= 0);

        m.w64(0x3000, 0x0001_0001_5750_5345).unwrap();
        assert_eq!(ioctlv(&mut m, es, 0x14, &[(0x3000, 8)], &[(0x3010, 4)]), 0);
        assert_eq!(m.r32(0x3010).unwrap(), 0x5e + 2 * 16);
        assert_eq!(ioctlv(&mut m, es, 0x15, &[(0x3000, 8), (0x3010, 4)], &[(0x4000, 0x7e)]), 0);
        assert_eq!(m.r16(0x4000 + 0x5a).unwrap(), 2); // content count
        assert_eq!(m.r32(0x4000 + 0x5e + 16).unwrap(), 0x11); // second content ID

        m.w32(0x3020, 1).unwrap();
        let cfd = ioctlv(&mut m, es, 0x09, &[(0x3020, 4)], &[]);
        assert!(cfd >= 0);
        m.w32(0x3030, cfd as u32).unwrap();
        m.w32(0x3034, 6).unwrap();
        m.w32(0x3038, 0).unwrap();
        assert_eq!(ioctlv(&mut m, es, 0x23, &[(0x3030, 4), (0x3034, 4), (0x3038, 4)], &[]), 6);
        assert_eq!(ioctlv(&mut m, es, 0x0a, &[(0x3030, 4)], &[(0x5000, 16)]), 5);
        assert_eq!(m.read_bytes(0x5000, 5).unwrap(), b"world");
        assert_eq!(ioctlv(&mut m, es, 0x0b, &[(0x3030, 4)], &[]), 0);

        m.w32(0x3020, 0).unwrap();
        assert!(ioctlv(&mut m, es, 0x09, &[(0x3020, 4)], &[]) < 0, "content 0 has no file");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nand_files() {
        let dir = temp_dir("fs");
        let mut m = Memory::default();
        m.ios = Ios::new(title(&dir));
        let data = "/title/00010001/57505345/data";
        let save = format!("{data}/save.bin");
        let fs = open(&mut m, "/dev/fs", 0);

        assert_eq!(open(&mut m, &save, 3), -106);
        m.load(0x3000 + 6, format!("{save}\0").as_bytes()).unwrap();
        assert_eq!(send(&mut m, IOCTL, fs as u32, &[0x09, 0x3000, 0x4c, 0, 0]), 0);
        assert_eq!(send(&mut m, IOCTL, fs as u32, &[0x09, 0x3000, 0x4c, 0, 0]), -105);

        let f = open(&mut m, &save, 3);
        assert!(f >= 0);
        m.load(0x4000, b"abc").unwrap();
        assert_eq!(send(&mut m, WRITE, f as u32, &[0x4000, 3]), 3);
        assert_eq!(send(&mut m, SEEK, f as u32, &[0, 0]), 0);
        assert_eq!(send(&mut m, READ, f as u32, &[0x5000, 16]), 3);
        assert_eq!(m.read_bytes(0x5000, 3).unwrap(), b"abc");
        assert_eq!(send(&mut m, IOCTL, f as u32, &[0x0b, 0, 0, 0x5100, 8]), 0);
        assert_eq!(m.r32(0x5100).unwrap(), 3);
        assert_eq!(send(&mut m, CLOSE, f as u32, &[]), 0);

        m.load(0x3100, format!("{data}\0").as_bytes()).unwrap();
        assert_eq!(ioctlv(&mut m, fs, 0x04, &[(0x3100, 64)], &[(0x3200, 4)]), 0);
        assert_eq!(m.r32(0x3200).unwrap(), 1);

        m.load(0x3300, format!("{save}\0").as_bytes()).unwrap();
        assert_eq!(send(&mut m, IOCTL, fs as u32, &[0x07, 0x3300, 64, 0, 0]), 0);
        assert_eq!(open(&mut m, &save, 1), -106);
        assert_eq!(open(&mut m, "/title/../../escape", 1), -101);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stm_event_hook_waits_until_released() {
        let mut m = Memory::default();
        let hook = open(&mut m, "/dev/stm/eventhook", 0);
        let imm = open(&mut m, "/dev/stm/immediate", 0);

        m.w32(0x2000, IOCTL).unwrap();
        m.w32(0x2008, hook as u32).unwrap();
        m.w32(0x200c, 0x1000).unwrap();
        m.w32(0x2018, 0x2100).unwrap();
        let mut ios = std::mem::take(&mut m.ios);
        assert!(ios.execute(&mut m, 0x2000).unwrap().is_empty());

        m.w32(REQ, IOCTL).unwrap();
        m.w32(REQ + 8, imm as u32).unwrap();
        m.w32(REQ + 0xc, 0x3002).unwrap();
        assert_eq!(ios.execute(&mut m, REQ).unwrap(), [REQ, 0x2000]);
        assert_eq!(m.r32(0x2000).unwrap(), REPLY);
        assert_eq!(m.r32(0x2004).unwrap(), 0);
    }

    #[test]
    fn ncd_reports_a_mac_address() {
        let mut m = Memory::default();
        let fd = open(&mut m, "/dev/net/ncd/manage", 0);
        assert!(fd >= 0);
        assert_eq!(ioctlv(&mut m, fd, 0x8, &[], &[(0x3000, 4), (0x3010, 6)]), 0);
        assert_eq!(m.r32(0x3000).unwrap(), 0);
        assert_eq!(m.read_bytes(0x3010, 6).unwrap(), ncd::MAC_ADDRESS);
    }
}