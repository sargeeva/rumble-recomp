//! Nintendo U8 archives (magic 0x55AA382D), as found in WiiWare contents.

use crate::be_u32;
use thiserror::Error;

pub const MAGIC: u32 = 0x55AA_382D;

#[derive(Debug, Error)]
pub enum U8Error {
    #[error("not a U8 archive")]
    BadMagic,
    #[error("U8 archive is malformed")]
    Malformed,
}

#[derive(Debug, Clone)]
pub struct U8File<'a> {
    /// Slash-separated path relative to the archive root.
    pub path: String,
    pub data: &'a [u8],
}

pub fn is_u8(data: &[u8]) -> bool {
    data.len() >= 4 && be_u32(data, 0) == MAGIC
}

fn cstr(table: &[u8], off: usize) -> Result<String, U8Error> {
    let s = table.get(off..).ok_or(U8Error::Malformed)?;
    let end = s.iter().position(|&b| b == 0).ok_or(U8Error::Malformed)?;
    Ok(String::from_utf8_lossy(&s[..end]).into_owned())
}

/// Lists every file in the archive with its full path.
pub fn files(data: &[u8]) -> Result<Vec<U8File<'_>>, U8Error> {
    if !is_u8(data) || data.len() < 0x20 {
        return Err(U8Error::BadMagic);
    }
    let root = be_u32(data, 4) as usize;
    let node = |i: usize| -> Result<(u8, usize, u32, u32), U8Error> {
        let o = root + i * 12;
        let n = data.get(o..o + 12).ok_or(U8Error::Malformed)?;
        Ok((n[0], be_u32(n, 0) as usize & 0xff_ffff, be_u32(n, 4), be_u32(n, 8)))
    };
    // Root node's size field is the total node count.
    let count = node(0)?.3 as usize;
    let strings = data.get(root + count * 12..).ok_or(U8Error::Malformed)?;

    let mut out = Vec::new();
    // Stack of (directory end index, path prefix).
    let mut dirs: Vec<(usize, String)> = vec![(count, String::new())];
    for i in 1..count {
        while dirs.last().is_some_and(|(end, _)| i >= *end) {
            dirs.pop();
        }
        let prefix = dirs.last().map(|d| d.1.clone()).unwrap_or_default();
        let (kind, name_off, off, size) = node(i)?;
        let path = format!("{prefix}{}", cstr(strings, name_off)?);
        if kind == 1 {
            dirs.push((size as usize, format!("{path}/")));
        } else {
            let (off, size) = (off as usize, size as usize);
            let bytes = data.get(off..off + size).ok_or(U8Error::Malformed)?;
            out.push(U8File { path, data: bytes });
        }
    }
    Ok(out)
}
