//! `/dev/es`: title identity, ticket/TMD views and content access for the
//! running title. Contents come from the decrypted `.app` files the build
//! step writes, so mods that patch the WAD are already applied.

use super::{Buffers, Title, EINVAL};
use crate::mem::{Fault, Memory};

/// ES error codes.
const ES_EINVAL: i32 = -1017;
const ES_NO_CONTENT: i32 = -1022;
const ES_NO_TITLE: i32 = -1027;

/// TMD layout: header fields and 36-byte content records.
const TMD_VERSION: usize = 0x180;
const TMD_ACCESS_RIGHTS: usize = 0x1d8;
const TMD_TITLE_VERSION: usize = 0x1dc;
const TMD_NUM_CONTENTS: usize = 0x1de;
const TMD_CONTENTS: usize = 0x1e4;
const TMD_CONTENT_SIZE: usize = 36;

/// A ticket view is a 4-byte view number followed by these ticket bytes.
const TICKET_VIEW_START: usize = 0x1d0;
const TICKET_VIEW_LEN: usize = 0xd4;

struct OpenContent {
    data: Vec<u8>,
    pos: u32,
}

#[derive(Default)]
pub struct Es {
    contents: Vec<Option<OpenContent>>,
}

impl Es {
    pub fn ioctl(&mut self, m: &mut Memory, title: Option<&Title>, num: u32, b: &Buffers) -> Result<i32, Fault> {
        let Some(t) = title else { return Ok(ES_NO_TITLE) };
        let is_title = |id: u64| id == t.id;
        Ok(match num {
            // Get title ID.
            0x20 => {
                b.out(m, 0, &t.id.to_be_bytes())?;
                0
            }
            // Get title directory.
            0x1d => {
                let dir = format!("/title/{:08x}/{:08x}/data\0", t.id >> 32, t.id as u32);
                b.out(m, 0, dir.as_bytes())?;
                0
            }
            // Ticket view count / views.
            0x12 => {
                let n = is_title(b.in_u64(m, 0)?) as u32;
                b.out_u32(m, 0, n)?;
                0
            }
            0x13 => {
                if !is_title(b.in_u64(m, 0)?) {
                    return Ok(ES_NO_TITLE);
                }
                b.out(m, 0, &ticket_view(t))?;
                0
            }
            // Ticket view of the "disc" title; for a NAND title with no
            // ticket passed in, the running title's.
            0x1b => {
                b.out(m, 0, &ticket_view(t))?;
                0
            }
            // Consumption (play limits) of the title's ticket: none. The
            // count goes in the last output.
            0x16 => {
                b.out_u32(m, b.output.len().saturating_sub(1), 0)?;
                0
            }
            // TMD view size / view.
            0x14 => {
                if !is_title(b.in_u64(m, 0)?) {
                    return Ok(ES_NO_TITLE);
                }
                b.out_u32(m, 0, tmd_view(t).len() as u32)?;
                0
            }
            0x15 => {
                if !is_title(b.in_u64(m, 0)?) {
                    return Ok(ES_NO_TITLE);
                }
                b.out(m, 0, &tmd_view(t))?;
                0
            }
            // Title content count / content IDs.
            0x10 => {
                b.out_u32(m, 0, num_contents(t) as u32)?;
                0
            }
            0x11 => {
                let ids: Vec<u8> = (0..num_contents(t)).flat_map(|i| content(t, i).0.to_be_bytes()).collect();
                b.out(m, 0, &ids)?;
                0
            }
            // Open content of the running title, by index; or of a title by
            // ID, ticket view and index.
            0x09 | 0x24 => {
                let index = if num == 0x09 { b.in_u32(m, 0, 0)? } else { b.in_u32(m, 2, 0)? };
                if num == 0x24 && !is_title(b.in_u64(m, 0)?) {
                    return Ok(ES_NO_TITLE);
                }
                self.open(t, index)
            }
            // Read / seek / close content.
            0x0a => {
                let Some(c) = self.content(b.in_u32(m, 0, 0)?) else { return Ok(ES_EINVAL) };
                let Some(&(addr, len)) = b.output.first() else { return Ok(ES_EINVAL) };
                let start = (c.pos as usize).min(c.data.len());
                let end = (start + len as usize).min(c.data.len());
                m.load(addr, &c.data[start..end])?;
                c.pos = end as u32;
                (end - start) as i32
            }
            0x23 => {
                let (offset, whence) = (b.in_u32(m, 1, 0)? as i32, b.in_u32(m, 2, 0)?);
                let Some(c) = self.content(b.in_u32(m, 0, 0)?) else { return Ok(ES_EINVAL) };
                let base = match whence {
                    0 => 0,
                    1 => c.pos as i64,
                    2 => c.data.len() as i64,
                    _ => return Ok(ES_EINVAL),
                };
                let pos = base + offset as i64;
                if pos < 0 || pos > c.data.len() as i64 {
                    return Ok(ES_EINVAL);
                }
                c.pos = pos as u32;
                pos as i32
            }
            0x0b => match self.contents.get_mut(b.in_u32(m, 0, 0)? as usize) {
                Some(slot @ Some(_)) => {
                    *slot = None;
                    0
                }
                _ => ES_EINVAL,
            },
            _ => EINVAL,
        })
    }

    fn open(&mut self, t: &Title, index: u32) -> i32 {
        let Some(i) = (0..num_contents(t)).find(|&i| content(t, i).1 == index as u16) else {
            return ES_NO_CONTENT;
        };
        let path = t.contents_dir.join(format!("{:08x}.app", content(t, i).0));
        let Ok(data) = std::fs::read(path) else { return ES_NO_CONTENT };
        let cfd = match self.contents.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.contents.push(None);
                self.contents.len() - 1
            }
        };
        self.contents[cfd] = Some(OpenContent { data, pos: 0 });
        cfd as i32
    }

    fn content(&mut self, cfd: u32) -> Option<&mut OpenContent> {
        self.contents.get_mut(cfd as usize)?.as_mut()
    }
}

fn num_contents(t: &Title) -> usize {
    u16::from_be_bytes([t.tmd[TMD_NUM_CONTENTS], t.tmd[TMD_NUM_CONTENTS + 1]]) as usize
}

/// (content ID, index) of TMD content record `i`.
fn content(t: &Title, i: usize) -> (u32, u16) {
    let r = &t.tmd[TMD_CONTENTS + i * TMD_CONTENT_SIZE..];
    (u32::from_be_bytes(r[0..4].try_into().unwrap()), u16::from_be_bytes([r[4], r[5]]))
}

fn ticket_view(t: &Title) -> Vec<u8> {
    let mut v = vec![0; 4];
    v.extend_from_slice(&t.ticket[TICKET_VIEW_START..TICKET_VIEW_START + TICKET_VIEW_LEN]);
    v
}

/// The TMD as IOS hands it out: header fields up to the access rights, the
/// title version and content count, padding, then ID/index/type/size of
/// each content (no hashes).
fn tmd_view(t: &Title) -> Vec<u8> {
    let tmd = &t.tmd;
    let mut v = tmd[TMD_VERSION..TMD_ACCESS_RIGHTS].to_vec();
    v.extend_from_slice(&tmd[TMD_TITLE_VERSION..TMD_TITLE_VERSION + 2]);
    v.extend_from_slice(&tmd[TMD_NUM_CONTENTS..TMD_NUM_CONTENTS + 2]);
    v.extend_from_slice(&[0, 0]);
    for i in 0..num_contents(t) {
        let r = &tmd[TMD_CONTENTS + i * TMD_CONTENT_SIZE..];
        v.extend_from_slice(&r[0..8]);
        v.extend_from_slice(&r[8..16]);
    }
    v
}
