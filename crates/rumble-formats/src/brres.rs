//! NW4R resource files (`.brres`): reading their name dictionaries and
//! putting together a new file from parts of others.
//!
//! A file is a header, a `root` section holding a dictionary of groups
//! (`3DModels(NW4R)`, `Textures(NW4R)`, ...) each with a dictionary of its
//! subfiles, the subfiles, and a pool of the names they all use. A name is
//! an offset to its string, from the dictionary or subfile that names it;
//! a subfile also holds the (negative) offset back to the file's start.

use crate::{align, be_u16, be_u32};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BrresError {
    #[error("not a brres file")]
    BadMagic,
    #[error("brres file is malformed")]
    Malformed,
    #[error("no {0} in the file")]
    Missing(String),
}

fn get(data: &[u8], off: usize, len: usize) -> Result<&[u8], BrresError> {
    data.get(off..off + len).ok_or(BrresError::Malformed)
}

fn u32_at(data: &[u8], off: usize) -> Result<u32, BrresError> {
    get(data, off, 4).map(|b| be_u32(b, 0))
}

fn cstr(data: &[u8], off: usize) -> Result<&str, BrresError> {
    let s = data.get(off..).ok_or(BrresError::Malformed)?;
    let end = s.iter().position(|&b| b == 0).ok_or(BrresError::Malformed)?;
    std::str::from_utf8(&s[..end]).map_err(|_| BrresError::Malformed)
}

/// The entries of the dictionary at `off`: each name and where its data is.
pub fn dict(data: &[u8], off: usize) -> Result<Vec<(String, usize)>, BrresError> {
    let count = u32_at(data, off + 4)? as usize;
    (1..=count)
        .map(|i| {
            let e = off + 8 + 16 * i;
            let name = u32_at(data, e + 8)? as i32 as isize;
            let at = u32_at(data, e + 12)? as i32 as isize;
            let name = cstr(data, off.wrapping_add_signed(name))?.to_string();
            Ok((name, off.wrapping_add_signed(at)))
        })
        .collect()
}

/// The groups of a brres file, each with its subfiles' names and offsets.
pub fn groups(data: &[u8]) -> Result<Vec<(String, Vec<(String, usize)>)>, BrresError> {
    if data.get(0..4) != Some(b"bres") {
        return Err(BrresError::BadMagic);
    }
    let root = be_u16(get(data, 0xc, 2)?, 0) as usize;
    dict(data, root + 8)?.into_iter().map(|(name, at)| Ok((name, dict(data, at)?))).collect()
}

/// The subfile `name` in `group`, from its header to the end of its size.
pub fn subfile<'a>(data: &'a [u8], group: &str, name: &str) -> Result<&'a [u8], BrresError> {
    let missing = || BrresError::Missing(format!("{group}/{name}"));
    let (_, files) = groups(data)?.into_iter().find(|(g, _)| g == group).ok_or_else(missing)?;
    let &(_, at) = files.iter().find(|(n, _)| n == name).ok_or_else(missing)?;
    let size = u32_at(data, at + 4)? as usize;
    get(data, at, size)
}

/// Whether the dictionary search for `name` goes right at node `id`: bit
/// `id & 7` of character `id >> 3`, 0 past the end of the name.
fn dict_bit(name: &[u8], id: u16) -> bool {
    name.get(id as usize >> 3).is_some_and(|c| c >> (id & 7) & 1 != 0)
}

/// The highest bit (as a node id) at which `a` and `b` differ.
fn dict_crit(a: &[u8], b: &[u8]) -> u16 {
    (0..a.len().max(b.len()))
        .rev()
        .find_map(|i| {
            let x = a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
            (x != 0).then(|| (i * 8 + 7 - x.leading_zeros() as usize) as u16)
        })
        .unwrap_or(0)
}

/// The search tree of a dictionary over `names`, inserted in order: each
/// entry's (id, left, right), with entry 0 the root. This is the tree
/// Nintendo's tools build; it matches every dictionary in the game's files.
pub fn dict_tree(names: &[&str]) -> Vec<(u16, u16, u16)> {
    let mut t: Vec<(u16, u16, u16, &[u8])> = vec![(0xffff, 0, 0, b"")];
    for name in names {
        let name = name.as_bytes();
        let step = |t: &[(u16, u16, u16, &[u8])], x: usize| {
            if dict_bit(name, t[x].0) { t[x].2 as usize } else { t[x].1 as usize }
        };
        let (mut p, mut x) = (0, t[0].1 as usize);
        while t[p].0 > t[x].0 {
            (p, x) = (x, step(&t, x));
        }
        let id = dict_crit(name, t[x].3);
        let (mut p, mut x) = (0, t[0].1 as usize);
        while t[p].0 > t[x].0 && t[x].0 > id {
            (p, x) = (x, step(&t, x));
        }
        let k = t.len() as u16;
        let (left, right) = if dict_bit(name, id) { (x as u16, k) } else { (k, x as u16) };
        t.push((id, left, right, name));
        if p == 0 {
            t[0].1 = k;
        } else if dict_bit(name, t[p].0) {
            t[p].2 = k;
        } else {
            t[p].1 = k;
        }
    }
    t.into_iter().map(|(id, l, r, _)| (id, l, r)).collect()
}


/// Where a subfile keeps its name, and the field holding its dictionary of
/// named nodes if it has one (an animation's bones; each node's data
/// starts with the offset to its name). For the kinds `Builder::add` moves.
fn name_fields(sub: &[u8]) -> Option<(usize, Option<usize>)> {
    match sub.get(0..4)? {
        b"TEX0" | b"PLT0" => Some((0x14, None)),
        b"CHR0" | b"SRT0" if be_u32(sub, 8) == 5 => Some((0x18, Some(0x10))),
        _ => None,
    }
}

struct Part {
    group: String,
    name: String,
    bytes: Vec<u8>,
    /// Name offsets to rewrite: where in `bytes` each is kept, the position
    /// in `bytes` it counts from, and the name.
    names: Vec<(usize, usize, String)>,
}

/// A brres file put together from parts of others.
#[derive(Default)]
pub struct Builder {
    parts: Vec<Part>,
}

impl Builder {
    /// Adds the only model of the brres `file` as `name`. A model has names
    /// all through it, so it keeps them where they are: everything from its
    /// start on (its file's name pool included) comes along unchanged.
    pub fn add_model_file(&mut self, file: &[u8], name: &str) -> Result<(), BrresError> {
        let gs = groups(file)?;
        let [(group, models)] = &gs[..] else { return Err(BrresError::Malformed) };
        let [(_, at)] = models[..] else { return Err(BrresError::Malformed) };
        if group != "3DModels(NW4R)" {
            return Err(BrresError::Missing("3DModels(NW4R)".into()));
        }
        let bytes = file[at..].to_vec();
        self.parts.push(Part { group: group.clone(), name: name.to_string(), bytes, names: Vec::new() });
        Ok(())
    }

    /// Adds subfile `name` of `group` from the brres `file`, as `rename` if
    /// given. Its names move to the new file. Textures, palettes and
    /// (version 5) bone and texture-matrix animations can be moved.
    pub fn add(&mut self, file: &[u8], group: &str, name: &str, rename: Option<&str>) -> Result<(), BrresError> {
        let sub = subfile(file, group, name)?;
        let start = sub.as_ptr() as usize - file.as_ptr() as usize;
        let (header_name, nodes) =
            name_fields(sub).ok_or_else(|| BrresError::Missing(format!("a movable {group}/{name}")))?;
        let new_name = rename.unwrap_or(name).to_string();
        let mut names = vec![(header_name, 0, new_name.clone())];
        if let Some(field) = nodes {
            let d = be_u32(sub, field) as usize;
            let count = u32_at(sub, d + 4)? as usize;
            for i in 1..=count {
                let e = d + 8 + 16 * i;
                let name_rel = u32_at(sub, e + 8)? as i32 as isize;
                let node = d.wrapping_add_signed(u32_at(sub, e + 12)? as i32 as isize);
                let bone = cstr(file, (start + d).wrapping_add_signed(name_rel))?.to_string();
                get(sub, node, 4)?;
                names.push((e + 8, d, bone.clone()));
                names.push((node, node, bone));
            }
        }
        self.parts.push(Part { group: group.to_string(), name: new_name, bytes: sub.to_vec(), names });
        Ok(())
    }

    /// Lays the file out: header, root section and dictionaries, the parts
    /// (each 32-byte aligned, as texture data must be), then the names.
    pub fn build(&self) -> Vec<u8> {
        let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
        for (i, p) in self.parts.iter().enumerate() {
            match groups.iter_mut().find(|(g, _)| *g == p.group) {
                Some((_, ps)) => ps.push(i),
                None => groups.push((&p.group, vec![i])),
            }
        }
        let dict_size = |n: usize| 8 + 16 * (n + 1);
        let root_dict = 0x18;
        let mut group_dicts = Vec::new();
        let mut at = root_dict + dict_size(groups.len());
        for (_, ps) in &groups {
            group_dicts.push(at);
            at += dict_size(ps.len());
        }
        let root_end = at;
        let mut part_at = Vec::new();
        for p in &self.parts {
            at = align(at, 0x20);
            part_at.push(at);
            at += p.bytes.len();
        }
        // The name pool: each name after its length, padded to 4 bytes.
        let pool_start = align(at, 4);
        let mut pool: Vec<(&str, usize)> = Vec::new();
        let mut pool_bytes = Vec::new();
        let all_names = groups
            .iter()
            .map(|(g, _)| *g)
            .chain(self.parts.iter().flat_map(|p| std::iter::once(p.name.as_str()).chain(p.names.iter().map(|n| n.2.as_str()))));
        for name in all_names {
            if pool.iter().any(|(n, _)| *n == name) {
                continue;
            }
            pool_bytes.extend_from_slice(&(name.len() as u32).to_be_bytes());
            pool.push((name, pool_start + pool_bytes.len()));
            pool_bytes.extend_from_slice(name.as_bytes());
            pool_bytes.push(0);
            pool_bytes.resize(align(pool_bytes.len(), 4), 0);
        }
        let name_at = |n: &str| pool.iter().find(|(p, _)| *p == n).unwrap().1;
        let total = pool_start + pool_bytes.len();

        let mut out = vec![0u8; total];
        let put = |out: &mut [u8], at: usize, v: u32| out[at..at + 4].copy_from_slice(&v.to_be_bytes());
        out[0..4].copy_from_slice(b"bres");
        out[4..6].copy_from_slice(&0xfeffu16.to_be_bytes());
        put(&mut out, 8, total as u32);
        out[0xc..0xe].copy_from_slice(&0x10u16.to_be_bytes());
        out[0xe..0x10].copy_from_slice(&(1 + self.parts.len() as u16).to_be_bytes());
        out[0x10..0x14].copy_from_slice(b"root");
        put(&mut out, 0x14, (root_end - 0x10) as u32);
        let write_dict = |out: &mut [u8], d: usize, entries: &[(&str, usize)]| {
            let names: Vec<&str> = entries.iter().map(|e| e.0).collect();
            put(out, d, dict_size(entries.len()) as u32);
            put(out, d + 4, entries.len() as u32);
            for (i, (id, left, right)) in dict_tree(&names).into_iter().enumerate() {
                let e = d + 8 + 16 * i;
                out[e..e + 2].copy_from_slice(&id.to_be_bytes());
                out[e + 4..e + 6].copy_from_slice(&left.to_be_bytes());
                out[e + 6..e + 8].copy_from_slice(&right.to_be_bytes());
                if i > 0 {
                    let (name, data) = entries[i - 1];
                    put(out, e + 8, (name_at(name) - d) as u32);
                    put(out, e + 12, (data - d) as u32);
                }
            }
        };
        let root_entries: Vec<(&str, usize)> = groups.iter().zip(&group_dicts).map(|((g, _), &d)| (*g, d)).collect();
        write_dict(&mut out, root_dict, &root_entries);
        for ((_, ps), &d) in groups.iter().zip(&group_dicts) {
            let entries: Vec<(&str, usize)> = ps.iter().map(|&i| (self.parts[i].name.as_str(), part_at[i])).collect();
            write_dict(&mut out, d, &entries);
        }
        for (p, &at) in self.parts.iter().zip(&part_at) {
            out[at..at + p.bytes.len()].copy_from_slice(&p.bytes);
            put(&mut out, at + 0xc, (at as u32).wrapping_neg());
            for (field, base, name) in &p.names {
                put(&mut out, at + field, (name_at(name) - (at + base)) as u32);
            }
        }
        out[pool_start..].copy_from_slice(&pool_bytes);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One name splits at its last character's highest set bit; two split
    /// at the highest bit where they differ.
    #[test]
    fn dict_tree_shape() {
        assert_eq!(dict_tree(&["3DModels(NW4R)"]), vec![(0xffff, 1, 0), (13 * 8 + 5, 0, 1)]);
        assert_eq!(dict_tree(&["ab", "ac"])[2].0, 8);
    }

    /// A built file reads back: groups, names and the moved names.
    #[test]
    fn built_file_reads_back() {
        // A source file with one 0x60-byte palette named "eye".
        let mut src = Builder::default();
        let mut plt = vec![0u8; 0x60];
        plt[0..4].copy_from_slice(b"PLT0");
        plt[4..8].copy_from_slice(&0x60u32.to_be_bytes());
        plt[8..12].copy_from_slice(&3u32.to_be_bytes());
        src.parts.push(Part { group: "Palettes(NW4R)".into(), name: "eye".into(), bytes: plt, names: vec![(0x14, 0, "eye".into())] });
        let src = src.build();
        assert_eq!(cstr(&src, subfile_at(&src, "Palettes(NW4R)", "eye") + be_u32(subfile(&src, "Palettes(NW4R)", "eye").unwrap(), 0x14) as usize).unwrap(), "eye");

        let mut b = Builder::default();
        b.add(&src, "Palettes(NW4R)", "eye", Some("eye_2")).unwrap();
        let out = b.build();
        let at = subfile_at(&out, "Palettes(NW4R)", "eye_2");
        assert_eq!(at % 0x20, 0);
        assert_eq!(be_u32(&out, at + 0xc) as i32, -(at as i32));
        assert_eq!(cstr(&out, at + be_u32(&out, at + 0x14) as usize).unwrap(), "eye_2");
    }

    fn subfile_at(file: &[u8], group: &str, name: &str) -> usize {
        subfile(file, group, name).unwrap().as_ptr() as usize - file.as_ptr() as usize
    }
}
