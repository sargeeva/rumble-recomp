//! `/dev/fs` and NAND files, kept in a host folder. Paths are absolute NAND
//! paths ("/title/00010001/57505345/data/save.bin") mapped under
//! `Title::nand_dir`.

use super::{Buffers, Title};
use crate::mem::{Fault, Memory};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// FS error codes.
const FS_EINVAL: i32 = -101;
const FS_EACCESS: i32 = -102;
const FS_EEXIST: i32 = -105;
const FS_ENOENT: i32 = -106;
const FS_EIO: i32 = -114;

/// Open modes.
const MODE_READ: u32 = 1;
const MODE_WRITE: u32 = 2;

/// Attribute block used by create/get-attr: owner, group, path, permissions.
const ATTR_SIZE: usize = 0x4c;
const ATTR_PATH: u32 = 6;

/// Creates the folders a Wii's NAND always has: the title's data
/// directory (made when it is installed), `/tmp`, which the NAND library
/// writes saves into before moving them into place (IOS empties it at
/// boot), and `/shared2`.
pub fn prepare_nand(t: &Title) {
    let data = format!("/title/{:08x}/{:08x}/data", t.id >> 32, t.id as u32);
    for dir in [data.as_str(), "/shared2"] {
        if let Some(p) = host_path(Some(t), dir) {
            let _ = std::fs::create_dir_all(p);
        }
    }
    if let Some(p) = host_path(Some(t), "/tmp") {
        let _ = std::fs::remove_dir_all(&p);
        let _ = std::fs::create_dir_all(p);
    }
}

/// The NAND path(s) an ioctl on `/dev/fs` names, for the IOS log.
pub fn describe(m: &mut Memory, num: u32, b: &Buffers) -> Option<String> {
    let at = |m: &mut Memory, off: u32| b.in_str(m, 0, off).ok();
    match num {
        0x03 | 0x09 => at(m, ATTR_PATH),
        0x04 | 0x06 | 0x07 | 0x0c => at(m, 0),
        0x08 => Some(format!("{} -> {}", at(m, 0)?, at(m, 0x40)?)),
        _ => None,
    }
}

/// Maps a NAND path to the host, refusing anything that is not a plain
/// absolute path.
fn host_path(t: Option<&Title>, nand: &str) -> Option<PathBuf> {
    let rest = nand.strip_prefix('/')?;
    let mut p = t?.nand_dir.clone();
    for part in rest.split('/').filter(|s| !s.is_empty()) {
        if part == "." || part == ".." || part.contains(['\\', ':']) {
            return None;
        }
        p.push(part);
    }
    Some(p)
}

fn io_error(e: &std::io::Error) -> i32 {
    match e.kind() {
        std::io::ErrorKind::NotFound => FS_ENOENT,
        std::io::ErrorKind::AlreadyExists => FS_EEXIST,
        std::io::ErrorKind::PermissionDenied => FS_EACCESS,
        _ => FS_EIO,
    }
}

pub struct File {
    host: PathBuf,
    mode: u32,
    pos: u32,
}

impl File {
    pub fn open(t: Option<&Title>, path: &str, mode: u32) -> Result<Self, i32> {
        let host = host_path(t, path).ok_or(FS_EINVAL)?;
        match std::fs::metadata(&host) {
            Ok(md) if md.is_file() => Ok(Self { host, mode, pos: 0 }),
            Ok(_) => Err(FS_EACCESS),
            Err(e) => Err(io_error(&e)),
        }
    }

    fn size(&self) -> u32 {
        std::fs::metadata(&self.host).map_or(0, |md| md.len() as u32)
    }

    pub fn read(&mut self, m: &mut Memory, addr: u32, len: u32) -> Result<i32, Fault> {
        if self.mode & MODE_READ == 0 {
            return Ok(FS_EACCESS);
        }
        let mut buf = vec![0; len as usize];
        let n = match std::fs::File::open(&self.host).and_then(|mut f| {
            f.seek(SeekFrom::Start(self.pos as u64))?;
            let mut n = 0;
            while n < buf.len() {
                match f.read(&mut buf[n..])? {
                    0 => break,
                    k => n += k,
                }
            }
            Ok(n)
        }) {
            Ok(n) => n,
            Err(e) => return Ok(io_error(&e)),
        };
        m.load(addr, &buf[..n])?;
        self.pos += n as u32;
        Ok(n as i32)
    }

    pub fn write(&mut self, m: &mut Memory, addr: u32, len: u32) -> Result<i32, Fault> {
        if self.mode & MODE_WRITE == 0 {
            return Ok(FS_EACCESS);
        }
        let data = m.read_bytes(addr, len)?;
        let r = std::fs::OpenOptions::new().write(true).open(&self.host).and_then(|mut f| {
            f.seek(SeekFrom::Start(self.pos as u64))?;
            f.write_all(&data)
        });
        if let Err(e) = r {
            return Ok(io_error(&e));
        }
        self.pos += len;
        Ok(len as i32)
    }

    pub fn seek(&mut self, offset: i32, whence: u32) -> i32 {
        let base = match whence {
            0 => 0,
            1 => self.pos as i64,
            2 => self.size() as i64,
            _ => return FS_EINVAL,
        };
        let pos = base + offset as i64;
        if pos < 0 || pos > self.size() as i64 {
            return FS_EINVAL;
        }
        self.pos = pos as u32;
        pos as i32
    }

    pub fn ioctl(&mut self, m: &mut Memory, num: u32, b: &Buffers) -> Result<i32, Fault> {
        Ok(match num {
            // Get file stats: size, position.
            0x0b => {
                let mut out = self.size().to_be_bytes().to_vec();
                out.extend_from_slice(&self.pos.to_be_bytes());
                b.out(m, 0, &out)?;
                0
            }
            _ => FS_EINVAL,
        })
    }
}

/// Ioctls on `/dev/fs` itself.
pub fn ioctl(m: &mut Memory, t: Option<&Title>, num: u32, b: &Buffers) -> Result<i32, Fault> {
    let path_at = |m: &mut Memory, k: usize, off: u32| -> Result<Option<(String, PathBuf)>, Fault> {
        let nand = b.in_str(m, k, off)?;
        Ok(host_path(t, &nand).map(|h| (nand, h)))
    };
    Ok(match num {
        // Get stats: plenty of free space.
        0x02 => {
            let stats = [0x4000u32, 0x4000, 0, 0x100, 0, 0x4000, 0];
            let bytes: Vec<u8> = stats.iter().flat_map(|v| v.to_be_bytes()).collect();
            b.out(m, 0, &bytes)?;
            0
        }
        // Create directory / file.
        0x03 | 0x09 => {
            let Some((_, host)) = path_at(m, 0, ATTR_PATH)? else { return Ok(FS_EINVAL) };
            if host.exists() {
                return Ok(FS_EEXIST);
            }
            if !host.parent().is_some_and(Path::is_dir) {
                return Ok(FS_ENOENT);
            }
            let r = if num == 0x03 { std::fs::create_dir(&host) } else { std::fs::File::create(&host).map(drop) };
            r.map_or_else(|e| io_error(&e), |()| 0)
        }
        // Set attributes: permissions are not modelled.
        0x05 => 0,
        // Get attributes.
        0x06 => {
            let Some((nand, host)) = path_at(m, 0, 0)? else { return Ok(FS_EINVAL) };
            if !host.exists() {
                return Ok(FS_ENOENT);
            }
            let mut attr = [0u8; ATTR_SIZE];
            let name = nand.as_bytes();
            attr[6..6 + name.len().min(63)].copy_from_slice(&name[..name.len().min(63)]);
            attr[0x46..0x49].copy_from_slice(&[3, 3, 3]);
            b.out(m, 0, &attr)?;
            0
        }
        // Delete (directories recursively).
        0x07 => {
            let Some((_, host)) = path_at(m, 0, 0)? else { return Ok(FS_EINVAL) };
            let r = if host.is_dir() { std::fs::remove_dir_all(&host) } else { std::fs::remove_file(&host) };
            r.map_or_else(|e| io_error(&e), |()| 0)
        }
        // Rename.
        0x08 => {
            let (Some((_, from)), Some((_, to))) = (path_at(m, 0, 0)?, path_at(m, 0, 0x40)?) else {
                return Ok(FS_EINVAL);
            };
            if to.is_file() {
                let _ = std::fs::remove_file(&to);
            }
            std::fs::rename(&from, &to).map_or_else(|e| io_error(&e), |()| 0)
        }
        // Read directory: names (12 chars max, NUL-separated) and count;
        // with a single output, just the count.
        0x04 => {
            let Some((_, host)) = path_at(m, 0, 0)? else { return Ok(FS_EINVAL) };
            let mut names: Vec<String> = match std::fs::read_dir(&host) {
                Ok(rd) => rd.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect(),
                Err(e) => return Ok(io_error(&e)),
            };
            names.sort();
            if b.output.len() >= 2 {
                names.truncate(b.in_u32(m, 1, 0)? as usize);
                let mut list = Vec::new();
                for n in &names {
                    list.extend_from_slice(n.as_bytes());
                    list.push(0);
                }
                b.out(m, 0, &list)?;
                b.out_u32(m, 1, names.len() as u32)?;
            } else {
                b.out_u32(m, 0, names.len() as u32)?;
            }
            0
        }
        // Usage: 16 KiB clusters and file/dir count under a path.
        0x0c => {
            let Some((_, host)) = path_at(m, 0, 0)? else { return Ok(FS_EINVAL) };
            if !host.exists() {
                return Ok(FS_ENOENT);
            }
            let (bytes, inodes) = usage(&host);
            b.out_u32(m, 0, bytes.div_ceil(0x4000) as u32)?;
            b.out_u32(m, 1, inodes)?;
            0
        }
        // Shutdown.
        0x0d => 0,
        _ => FS_EINVAL,
    })
}

fn usage(p: &Path) -> (u64, u32) {
    let mut total = (0, 1);
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                let (b, n) = usage(&path);
                total = (total.0 + b, total.1 + n);
            } else {
                total = (total.0 + e.metadata().map_or(0, |m| m.len()), total.1 + 1);
            }
        }
    }
    total
}
