//! BPS (beat patch) format: https://www.romhacking.net/documents/746/

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BpsError {
    #[error("not a BPS1 patch")]
    BadMagic,
    #[error("patch is truncated or malformed")]
    Malformed,
    #[error("patch file checksum mismatch (corrupt download?)")]
    PatchCrc,
    #[error("source mismatch: patch expects size {expected_size} crc {expected_crc:08x}, got size {actual_size} crc {actual_crc:08x}")]
    SourceMismatch {
        expected_size: u64,
        expected_crc: u32,
        actual_size: u64,
        actual_crc: u32,
    },
    #[error("output checksum mismatch after patching")]
    TargetCrc,
}

/// Header information of a BPS patch, readable without applying it.
#[derive(Debug, Clone)]
pub struct BpsInfo {
    pub source_size: u64,
    pub target_size: u64,
    pub metadata: String,
    pub source_crc: u32,
    pub target_crc: u32,
    pub patch_crc: u32,
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, BpsError> {
        let b = *self.data.get(self.pos).ok_or(BpsError::Malformed)?;
        self.pos += 1;
        Ok(b)
    }

    fn varint(&mut self) -> Result<u64, BpsError> {
        let mut data = 0u64;
        let mut shift = 1u64;
        loop {
            let x = self.byte()?;
            data = data
                .checked_add((x & 0x7f) as u64 * shift)
                .ok_or(BpsError::Malformed)?;
            if x & 0x80 != 0 {
                return Ok(data);
            }
            shift = shift.checked_shl(7).ok_or(BpsError::Malformed)?;
            data = data.checked_add(shift).ok_or(BpsError::Malformed)?;
        }
    }

    fn signed(&mut self) -> Result<i64, BpsError> {
        let v = self.varint()?;
        let mag = (v >> 1) as i64;
        Ok(if v & 1 != 0 { -mag } else { mag })
    }
}

fn footer_u32(patch: &[u8], from_end: usize) -> u32 {
    let o = patch.len() - from_end;
    u32::from_le_bytes(patch[o..o + 4].try_into().unwrap())
}

fn parse_header(patch: &[u8]) -> Result<(BpsInfo, usize), BpsError> {
    if patch.len() < 4 + 12 || &patch[..4] != b"BPS1" {
        return Err(BpsError::BadMagic);
    }
    let mut r = Reader { data: &patch[..patch.len() - 12], pos: 4 };
    let source_size = r.varint()?;
    let target_size = r.varint()?;
    let meta_len = r.varint()? as usize;
    let meta = r.data.get(r.pos..r.pos + meta_len).ok_or(BpsError::Malformed)?;
    r.pos += meta_len;
    let info = BpsInfo {
        source_size,
        target_size,
        metadata: String::from_utf8_lossy(meta).into_owned(),
        source_crc: footer_u32(patch, 12),
        target_crc: footer_u32(patch, 8),
        patch_crc: footer_u32(patch, 4),
    };
    Ok((info, r.pos))
}

/// Reads a patch's header and footer without applying it.
pub fn info(patch: &[u8]) -> Result<BpsInfo, BpsError> {
    parse_header(patch).map(|(i, _)| i)
}

/// Returns true if `source` is the exact input this patch was made against.
pub fn matches_source(info: &BpsInfo, source: &[u8]) -> bool {
    source.len() as u64 == info.source_size && crc32fast::hash(source) == info.source_crc
}

/// Applies a BPS patch, verifying the patch, source and output checksums.
pub fn apply(patch: &[u8], source: &[u8]) -> Result<Vec<u8>, BpsError> {
    let (info, start) = parse_header(patch)?;
    if crc32fast::hash(&patch[..patch.len() - 4]) != info.patch_crc {
        return Err(BpsError::PatchCrc);
    }
    let source_crc = crc32fast::hash(source);
    if source.len() as u64 != info.source_size || source_crc != info.source_crc {
        return Err(BpsError::SourceMismatch {
            expected_size: info.source_size,
            expected_crc: info.source_crc,
            actual_size: source.len() as u64,
            actual_crc: source_crc,
        });
    }

    let target_size = info.target_size as usize;
    let mut target: Vec<u8> = Vec::with_capacity(target_size);
    let mut r = Reader { data: &patch[..patch.len() - 12], pos: start };
    let mut source_rel: i64 = 0;
    let mut target_rel: i64 = 0;

    while r.pos < r.data.len() {
        let n = r.varint()?;
        let len = ((n >> 2) + 1) as usize;
        if target.len() + len > target_size {
            return Err(BpsError::Malformed);
        }
        match n & 3 {
            // SourceRead: copy from source at the current output offset.
            0 => {
                let o = target.len();
                target.extend_from_slice(source.get(o..o + len).ok_or(BpsError::Malformed)?);
            }
            // TargetRead: literal bytes from the patch.
            1 => {
                let bytes = r.data.get(r.pos..r.pos + len).ok_or(BpsError::Malformed)?;
                target.extend_from_slice(bytes);
                r.pos += len;
            }
            // SourceCopy: copy from a relative source offset.
            2 => {
                source_rel += r.signed()?;
                let o = usize::try_from(source_rel).map_err(|_| BpsError::Malformed)?;
                target.extend_from_slice(source.get(o..o + len).ok_or(BpsError::Malformed)?);
                source_rel += len as i64;
            }
            // TargetCopy: copy from already-written output; may overlap itself.
            _ => {
                target_rel += r.signed()?;
                let o = usize::try_from(target_rel).map_err(|_| BpsError::Malformed)?;
                if o >= target.len() {
                    return Err(BpsError::Malformed);
                }
                for i in 0..len {
                    let b = target[o + i];
                    target.push(b);
                }
                target_rel += len as i64;
            }
        }
    }

    if target.len() != target_size || crc32fast::hash(&target) != info.target_crc {
        return Err(BpsError::TargetCrc);
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let x = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(x | 0x80);
                return;
            }
            out.push(x);
            v -= 1;
        }
    }

    fn build(source: &[u8], target: &[u8], actions: &[u8]) -> Vec<u8> {
        let mut p = b"BPS1".to_vec();
        varint(source.len() as u64, &mut p);
        varint(target.len() as u64, &mut p);
        varint(0, &mut p);
        p.extend_from_slice(actions);
        p.extend_from_slice(&crc32fast::hash(source).to_le_bytes());
        p.extend_from_slice(&crc32fast::hash(target).to_le_bytes());
        let crc = crc32fast::hash(&p);
        p.extend_from_slice(&crc.to_le_bytes());
        p
    }

    #[test]
    fn applies_all_action_kinds() {
        let source = b"HELLO";
        let target = b"HELxyxyxyLO";
        let mut a = Vec::new();
        varint(((3 - 1) << 2) | 0, &mut a); // SourceRead "HEL"
        varint(((2 - 1) << 2) | 1, &mut a); // TargetRead "xy"
        a.extend_from_slice(b"xy");
        varint(((4 - 1) << 2) | 3, &mut a); // TargetCopy from 3, overlapping
        varint(3 << 1, &mut a);
        varint(((2 - 1) << 2) | 2, &mut a); // SourceCopy "LO" from 3
        varint(3 << 1, &mut a);
        let patch = build(source, target, &a);
        assert_eq!(apply(&patch, source).unwrap(), target);
    }

    #[test]
    fn rejects_wrong_source() {
        let mut a = Vec::new();
        varint(0, &mut a);
        let patch = build(b"A", b"A", &a);
        assert!(matches!(apply(&patch, b"B"), Err(BpsError::SourceMismatch { .. })));
    }
}
