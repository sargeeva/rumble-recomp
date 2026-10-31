//! WiiWare installable WAD ("Is" type) parsing and content decryption.
//!
//! The common key is never bundled; callers must supply it from the user's
//! own console dump.

use crate::{align, be_u16, be_u32, be_u64};
use aes::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
use sha1::{Digest, Sha1};
use thiserror::Error;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

#[derive(Debug, Error)]
pub enum WadError {
    #[error("not an installable WAD")]
    BadHeader,
    #[error("WAD is truncated")]
    Truncated,
    #[error("content {index} failed its SHA-1 check (wrong common key?)")]
    HashMismatch { index: u16 },
}

#[derive(Debug, Clone)]
pub struct ContentRecord {
    pub id: u32,
    pub index: u16,
    pub kind: u16,
    pub size: u64,
    pub sha1: [u8; 20],
    /// Offset of the encrypted blob inside the WAD file.
    pub offset: usize,
}

#[derive(Debug)]
pub struct Wad<'a> {
    data: &'a [u8],
    pub title_id: u64,
    pub title_version: u16,
    pub common_key_index: u8,
    encrypted_title_key: [u8; 16],
    pub contents: Vec<ContentRecord>,
    tmd: &'a [u8],
    ticket: &'a [u8],
}

impl<'a> Wad<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, WadError> {
        if data.len() < 0x20 || be_u32(data, 0) != 0x20 {
            return Err(WadError::BadHeader);
        }
        let kind = be_u16(data, 4);
        if kind != u16::from_be_bytes(*b"Is") && kind != u16::from_be_bytes(*b"ib") {
            return Err(WadError::BadHeader);
        }
        let cert_size = be_u32(data, 0x08) as usize;
        let ticket_size = be_u32(data, 0x10) as usize;
        let tmd_size = be_u32(data, 0x14) as usize;

        let ticket_off = align(0x20, 0x40) + align(cert_size, 0x40);
        let tmd_off = ticket_off + align(ticket_size, 0x40);
        let data_off = tmd_off + align(tmd_size, 0x40);
        if data.len() < data_off || ticket_size < 0x2a4 || tmd_size < 0x1e4 {
            return Err(WadError::Truncated);
        }

        let ticket = &data[ticket_off..ticket_off + ticket_size];
        let tmd = &data[tmd_off..tmd_off + tmd_size];

        let num_contents = be_u16(tmd, 0x1de) as usize;
        let mut contents = Vec::with_capacity(num_contents);
        let mut off = data_off;
        for i in 0..num_contents {
            let r = 0x1e4 + i * 36;
            if r + 36 > tmd.len() {
                return Err(WadError::Truncated);
            }
            let size = be_u64(tmd, r + 8);
            contents.push(ContentRecord {
                id: be_u32(tmd, r),
                index: be_u16(tmd, r + 4),
                kind: be_u16(tmd, r + 6),
                size,
                sha1: tmd[r + 16..r + 36].try_into().unwrap(),
                offset: off,
            });
            off += align(size as usize, 0x40);
        }
        if off > align(data.len(), 0x40) {
            return Err(WadError::Truncated);
        }

        Ok(Wad {
            data,
            title_id: be_u64(tmd, 0x18c),
            title_version: be_u16(tmd, 0x1dc),
            common_key_index: ticket[0x1f1],
            encrypted_title_key: ticket[0x1bf..0x1cf].try_into().unwrap(),
            contents,
            tmd,
            ticket,
        })
    }

    /// The signed title metadata, as stored in the WAD.
    pub fn tmd(&self) -> &'a [u8] {
        self.tmd
    }

    /// The signed ticket, as stored in the WAD.
    pub fn ticket(&self) -> &'a [u8] {
        self.ticket
    }

    /// Four-character game code, e.g. "WPSE" for the USA release.
    pub fn game_code(&self) -> String {
        String::from_utf8_lossy(&(self.title_id as u32).to_be_bytes()).into_owned()
    }

    pub fn title_key(&self, common_key: &[u8; 16]) -> [u8; 16] {
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&self.title_id.to_be_bytes());
        let mut key = self.encrypted_title_key;
        Aes128CbcDec::new(common_key.into(), &iv.into())
            .decrypt_padded_mut::<NoPadding>(&mut key)
            .expect("16-byte block");
        key
    }

    /// Decrypts one content and verifies it against the TMD hash.
    pub fn decrypt_content(&self, rec: &ContentRecord, title_key: &[u8; 16]) -> Result<Vec<u8>, WadError> {
        let enc_len = align(rec.size as usize, 16);
        let mut buf = self
            .data
            .get(rec.offset..rec.offset + enc_len)
            .ok_or(WadError::Truncated)?
            .to_vec();
        let mut iv = [0u8; 16];
        iv[..2].copy_from_slice(&rec.index.to_be_bytes());
        Aes128CbcDec::new(title_key.into(), &iv.into())
            .decrypt_padded_mut::<NoPadding>(&mut buf)
            .expect("aligned to block size");
        buf.truncate(rec.size as usize);
        if Sha1::digest(&buf).as_slice() != rec.sha1 {
            return Err(WadError::HashMismatch { index: rec.index });
        }
        Ok(buf)
    }
}
