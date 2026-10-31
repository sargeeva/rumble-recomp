//! Wii save files as the System Menu copies them to an SD card
//! (`private/wii/title/XXXX/data.bin`), read and written the way Dolphin's
//! `WiiSave.cpp` and segher's tachtig/twintig do.
//!
//! Layout (all numbers big-endian):
//! - 0x0000 header, 0xF0C0 bytes, AES-128-CBC with the SD key and IV:
//!   title ID, banner size, permissions, an MD5 of the plain header taken
//!   with the MD5 field set to the "blanker", then the banner.
//! - 0xF0C0 Bk header, 0x80 bytes, plain: NG (console) ID, file count,
//!   size of the file area, title ID.
//! - The files: a 0x80-byte plain header each (size, permissions, type,
//!   path, IV), followed for files by the data, padded to 0x40 bytes and
//!   encrypted with the SD key and that IV.
//! - The signature: ECDSA (sect233r1) over SHA-1(SHA-1(Bk header and
//!   files)) by an "AP" key, 4 more bytes (`/Sii` from Dolphin), then the console's NG
//!   certificate and the AP certificate the NG key signed.

use crate::ec;
use crate::{align, be_u32, be_u64};
use aes::cipher::{block_padding::NoPadding, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use md5::{Digest as _, Md5};
use sha1::Sha1;
use thiserror::Error;

type Dec = cbc::Decryptor<aes::Aes128>;
type Enc = cbc::Encryptor<aes::Aes128>;

const HEADER_SIZE: usize = 0xf0c0;
const BK_SIZE: usize = 0x80;
const BK_LISTED_SIZE: u32 = 0x70;
const BK_MAGIC: u32 = 0x426b_0001;
const FILE_MAGIC: u32 = 0x03ad_f17e;
const BLOCK: usize = 0x40;
/// Banner without icons, one icon, and the most a banner may be.
const BANNER_BASE: usize = 0x60a0;
const ICON_SIZE: usize = 0x1200;
const BANNER_MAX: usize = 0xf0a0;
const SIG_END_MAGIC: u32 = 0x2f53_6969;
const CERT_SIZE: usize = 0x180;
/// Signature with its end magic, then the NG and AP certificates.
const TAIL_SIZE: usize = 0x40 + 2 * CERT_SIZE;
/// The System Menu, whose AP key signs exports.
const SYSTEM_MENU: u64 = 0x0000_0001_0000_0002;

#[derive(Debug, Error)]
pub enum SaveError {
    #[error("not a Wii save file: {0}")]
    Format(String),
    #[error("this save is for {found}, not {expected}")]
    WrongTitle { found: String, expected: String },
}

fn bad(msg: impl Into<String>) -> SaveError {
    SaveError::Format(msg.into())
}

/// The SD card keys (`keys/sd-key`, `keys/sd-iv`, `keys/md5-blanker`).
pub struct Keys {
    pub sd_key: [u8; 16],
    pub sd_iv: [u8; 16],
    pub md5_blanker: [u8; 16],
}

/// The console identity a save export is signed with.
pub struct Device {
    pub ng_id: u32,
    pub ms_id: u32,
    pub ca_id: u32,
    pub ng_key_id: u32,
    pub ng_private: [u8; 30],
    /// Nintendo's signature over the NG certificate.
    pub ng_signature: [u8; 60],
}

impl Default for Device {
    /// Dolphin's default console (`IOSC.cpp`, `DEFAULT_DEVICE_ID` and on),
    /// whose exports real Wiis accept.
    fn default() -> Self {
        Device {
            ng_id: 0x0403_ac68,
            ms_id: 2,
            ca_id: 1,
            ng_key_id: 0x6aab_8c59,
            ng_private: [
                0x00, 0xab, 0xee, 0xc1, 0xdd, 0xb4, 0xa6, 0x16, 0x6b, 0x70, 0xfd, 0x7e, 0x56, 0x67, 0x70, 0x57, 0x55,
                0x27, 0x38, 0xa3, 0x26, 0xc5, 0x46, 0x16, 0xf7, 0x62, 0xc9, 0xed, 0x73, 0xf2,
            ],
            ng_signature: [
                0x00, 0xd8, 0x81, 0x63, 0xb2, 0x00, 0x6b, 0x0b, 0x54, 0x82, 0x88, 0x63, 0x81, 0x1c, 0x00, 0x71, 0x12,
                0xed, 0xb7, 0xfd, 0x21, 0xab, 0x0e, 0x50, 0x0e, 0x1f, 0xbf, 0x78, 0xad, 0x37, 0x00, 0x71, 0x8d, 0x82,
                0x41, 0xee, 0x45, 0x11, 0xc7, 0x3b, 0xac, 0x08, 0xb6, 0x83, 0xdc, 0x05, 0xb8, 0xa8, 0x90, 0x1f, 0xa8,
                0x2a, 0x0e, 0x4e, 0x76, 0xef, 0x44, 0x72, 0x99, 0xf8,
            ],
        }
    }
}

impl Device {
    /// The console identity in a BootMii `keys.bin` (0x400 bytes, laid out
    /// as Dolphin's `BootMiiKeyDump`): NG ID at 0x124, NG private key at
    /// 0x128, the MS and CA IDs at 0x200 and 0x204, NG key ID at 0x208 and
    /// Nintendo's signature over the NG certificate at 0x20C.
    pub fn from_keys_bin(b: &[u8]) -> Result<Self, SaveError> {
        if b.len() != 0x400 {
            return Err(bad("keys.bin is not 1024 bytes"));
        }
        let mut ng_private = [0; 30];
        ng_private.copy_from_slice(&b[0x128..0x146]);
        let mut ng_signature = [0; 60];
        ng_signature.copy_from_slice(&b[0x20c..0x248]);
        Ok(Device {
            ng_id: be_u32(b, 0x124),
            ms_id: be_u32(b, 0x200),
            ca_id: be_u32(b, 0x204),
            ng_key_id: be_u32(b, 0x208),
            ng_private,
            ng_signature,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    File(Vec<u8>),
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveFile {
    /// Path inside the title's data folder, `/`-separated.
    pub path: String,
    pub permissions: u8,
    pub attributes: u8,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Save {
    pub title_id: u64,
    pub permissions: u8,
    /// `banner.bin`.
    pub banner: Vec<u8>,
    pub files: Vec<SaveFile>,
}

/// Game code of a title ID ("WPSE").
pub fn game_code(title_id: u64) -> String {
    (title_id as u32).to_be_bytes().iter().map(|&c| if c.is_ascii_graphic() { c as char } else { '?' }).collect()
}

fn decrypt(key: &[u8; 16], iv: &[u8; 16], data: &mut [u8]) {
    Dec::new(key.into(), iv.into()).decrypt_padded_mut::<NoPadding>(data).expect("whole blocks");
}

fn encrypt(key: &[u8; 16], iv: &[u8; 16], data: &mut [u8]) {
    let len = data.len();
    Enc::new(key.into(), iv.into()).encrypt_padded_mut::<NoPadding>(data, len).expect("whole blocks");
}

/// A path the save may name: relative, no `..`, no backslashes.
fn safe_path(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.contains(':')
        && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Reads a data.bin, checking the header's MD5 and that it is a save for
/// `expected_title`. The signature is not checked (see `verify_signature`).
pub fn parse(data: &[u8], keys: &Keys, expected_title: u64) -> Result<Save, SaveError> {
    if data.len() < HEADER_SIZE + BK_SIZE {
        return Err(bad("the file is too short"));
    }
    let mut header = data[..HEADER_SIZE].to_vec();
    decrypt(&keys.sd_key, &keys.sd_iv, &mut header);
    let title_id = be_u64(&header, 0);
    if title_id != expected_title {
        return Err(SaveError::WrongTitle { found: game_code(title_id), expected: game_code(expected_title) });
    }
    let banner_size = be_u32(&header, 8) as usize;
    if !(BANNER_BASE + ICON_SIZE..=BANNER_MAX).contains(&banner_size) || (banner_size - BANNER_BASE) % ICON_SIZE != 0
    {
        return Err(bad(format!("banner size {banner_size:#x}")));
    }
    let md5_file: [u8; 16] = header[0x0e..0x1e].try_into().unwrap();
    header[0x0e..0x1e].copy_from_slice(&keys.md5_blanker);
    if Md5::digest(&header)[..] != md5_file {
        return Err(bad("the header's checksum does not match (wrong SD key, or a damaged file)"));
    }

    let bk = &data[HEADER_SIZE..HEADER_SIZE + BK_SIZE];
    if be_u32(bk, 0) != BK_LISTED_SIZE || be_u32(bk, 4) != BK_MAGIC {
        return Err(bad("no Bk header"));
    }
    let count = be_u32(bk, 0x0c) as usize;
    let files_size = be_u32(bk, 0x10) as usize;
    let files_end = HEADER_SIZE + BK_SIZE + files_size;
    if files_end > data.len() {
        return Err(bad("the file list runs past the end"));
    }
    let mut files = Vec::new();
    let mut at = HEADER_SIZE + BK_SIZE;
    for _ in 0..count {
        if at + 0x80 > files_end {
            return Err(bad("a file header runs past the file list"));
        }
        let h = &data[at..at + 0x80];
        if be_u32(h, 0) != FILE_MAGIC {
            return Err(bad(format!("bad file header at {at:#x}")));
        }
        let size = be_u32(h, 4) as usize;
        let name = &h[0x0b..0x4b];
        let name = &name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())];
        let path = std::str::from_utf8(name).map_err(|_| bad("a file name is not text"))?.to_string();
        if !safe_path(&path) {
            return Err(bad(format!("unsafe file name {path:?}")));
        }
        at += 0x80;
        let kind = match h[0x0a] {
            1 => {
                let padded = align(size, BLOCK);
                if at + padded > files_end {
                    return Err(bad(format!("{path} runs past the file list")));
                }
                let iv: [u8; 16] = h[0x50..0x60].try_into().unwrap();
                let mut body = data[at..at + padded].to_vec();
                decrypt(&keys.sd_key, &iv, &mut body);
                body.truncate(size);
                at += padded;
                Kind::File(body)
            }
            2 => Kind::Dir,
            t => return Err(bad(format!("{path} has unknown type {t}"))),
        };
        files.push(SaveFile { path, permissions: h[8], attributes: h[9], kind });
    }
    Ok(Save { title_id, permissions: header[0x0c], banner: header[0x20..0x20 + banner_size].to_vec(), files })
}

/// An ECC certificate: signature (type ECC), issuer, name, key ID and
/// public key.
fn make_cert(issuer: &str, name: &str, key_id: u32, public: &[u8; 60], sig: &[u8; 60]) -> [u8; CERT_SIZE] {
    let mut c = [0u8; CERT_SIZE];
    c[0..4].copy_from_slice(&0x0001_0002u32.to_be_bytes());
    c[4..0x40].copy_from_slice(sig);
    c[0x80..0x80 + issuer.len()].copy_from_slice(issuer.as_bytes());
    c[0xc0..0xc4].copy_from_slice(&2u32.to_be_bytes());
    c[0xc4..0xc4 + name.len()].copy_from_slice(name.as_bytes());
    c[0x104..0x108].copy_from_slice(&key_id.to_be_bytes());
    c[0x108..0x144].copy_from_slice(public);
    c
}

fn sha1(data: &[u8]) -> [u8; 20] {
    Sha1::digest(data).into()
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0; N];
    getrandom::fill(&mut b).expect("system random numbers");
    b
}

/// Writes a data.bin for `save`, signed as `device`.
pub fn build(save: &Save, keys: &Keys, device: &Device) -> Result<Vec<u8>, SaveError> {
    let size = save.banner.len();
    if !(BANNER_BASE + ICON_SIZE..=BANNER_MAX).contains(&size) || (size - BANNER_BASE) % ICON_SIZE != 0 {
        return Err(bad(format!("banner.bin is {size:#x} bytes, which is no banner size")));
    }
    let mut header = vec![0u8; HEADER_SIZE];
    header[0..8].copy_from_slice(&save.title_id.to_be_bytes());
    header[8..12].copy_from_slice(&(size as u32).to_be_bytes());
    header[0x0c] = save.permissions;
    header[0x0e..0x1e].copy_from_slice(&keys.md5_blanker);
    header[0x20..0x20 + size].copy_from_slice(&save.banner);
    // As Dolphin does: clear the banner's "no copy" flag.
    header[0x20 + 7] &= !1;
    let md5 = Md5::digest(&header);
    header[0x0e..0x1e].copy_from_slice(&md5);
    encrypt(&keys.sd_key, &keys.sd_iv, &mut header);

    let mut files = Vec::new();
    for f in &save.files {
        if !safe_path(&f.path) || f.path.len() >= 0x40 {
            return Err(bad(format!("file name {:?} can't be stored", f.path)));
        }
        let mut h = [0u8; 0x80];
        h[0..4].copy_from_slice(&FILE_MAGIC.to_be_bytes());
        h[8] = f.permissions;
        h[9] = f.attributes;
        h[0x0b..0x0b + f.path.len()].copy_from_slice(f.path.as_bytes());
        match &f.kind {
            Kind::Dir => {
                h[0x0a] = 2;
                files.extend_from_slice(&h);
            }
            Kind::File(body) => {
                h[0x0a] = 1;
                h[4..8].copy_from_slice(&(body.len() as u32).to_be_bytes());
                let iv: [u8; 16] = random();
                h[0x50..0x60].copy_from_slice(&iv);
                files.extend_from_slice(&h);
                let mut padded = body.clone();
                padded.resize(align(body.len(), BLOCK), 0);
                encrypt(&keys.sd_key, &iv, &mut padded);
                files.extend_from_slice(&padded);
            }
        }
    }

    let mut bk = [0u8; BK_SIZE];
    bk[0..4].copy_from_slice(&BK_LISTED_SIZE.to_be_bytes());
    bk[4..8].copy_from_slice(&BK_MAGIC.to_be_bytes());
    bk[8..12].copy_from_slice(&device.ng_id.to_be_bytes());
    bk[0x0c..0x10].copy_from_slice(&(save.files.len() as u32).to_be_bytes());
    bk[0x10..0x14].copy_from_slice(&(files.len() as u32).to_be_bytes());
    bk[0x1c..0x20].copy_from_slice(&((files.len() + TAIL_SIZE) as u32).to_be_bytes());
    bk[0x60..0x68].copy_from_slice(&save.title_id.to_be_bytes());

    let mut out = header;
    out.extend_from_slice(&bk);
    out.extend_from_slice(&files);

    // The AP key, derived as Dolphin does, certified by the NG key; it
    // signs the Bk header and files.
    let mut ap_private = [0u8; 30];
    ap_private[0x1d] = 1;
    let ng_issuer = format!("Root-CA{:08x}-MS{:08x}", device.ca_id, device.ms_id);
    let ng_name = format!("NG{:08x}", device.ng_id);
    let ng_cert = make_cert(&ng_issuer, &ng_name, device.ng_key_id, &ec::priv_to_pub(&device.ng_private), &device.ng_signature);
    let ap_issuer = format!("{ng_issuer}-{ng_name}");
    let ap_name = format!("AP{SYSTEM_MENU:016x}");
    let ap_public = ec::priv_to_pub(&ap_private);
    let unsigned = make_cert(&ap_issuer, &ap_name, 0, &ap_public, &[0; 60]);
    let ap_sig = ec::sign(&device.ng_private, &sha1(&unsigned[0x80..]), &random());
    let ap_cert = make_cert(&ap_issuer, &ap_name, 0, &ap_public, &ap_sig);
    let hash = sha1(&sha1(&out[HEADER_SIZE..]));
    let sig = ec::sign(&ap_private, &hash, &random());
    out.extend_from_slice(&sig);
    out.extend_from_slice(&SIG_END_MAGIC.to_be_bytes());
    out.extend_from_slice(&ng_cert);
    out.extend_from_slice(&ap_cert);
    Ok(out)
}

/// Checks a data.bin's signatures: the AP certificate against the NG
/// certificate's key, and the Bk header and files against the AP key.
/// (The NG certificate's own signature is Nintendo's and can't be checked
/// here.)
pub fn verify_signature(data: &[u8]) -> Result<(), SaveError> {
    if data.len() < HEADER_SIZE + BK_SIZE + TAIL_SIZE {
        return Err(bad("the file is too short"));
    }
    let files_size = be_u32(&data[HEADER_SIZE..], 0x10) as usize;
    let signed_end = HEADER_SIZE + BK_SIZE + files_size;
    if signed_end + TAIL_SIZE > data.len() {
        return Err(bad("the signature runs past the end"));
    }
    let tail = &data[signed_end..signed_end + TAIL_SIZE];
    // The 4 bytes after the signature are Dolphin's `/Sii` end marker in
    // its exports, and something else in a real Wii's: not checked.
    let sig: [u8; 60] = tail[..60].try_into().unwrap();
    let ng_cert = &tail[0x40..0x40 + CERT_SIZE];
    let ap_cert = &tail[0x40 + CERT_SIZE..];
    let ng_public: [u8; 60] = ng_cert[0x108..0x144].try_into().unwrap();
    let ap_public: [u8; 60] = ap_cert[0x108..0x144].try_into().unwrap();
    let ap_sig: [u8; 60] = ap_cert[4..0x40].try_into().unwrap();
    if !ec::verify(&ng_public, &sha1(&ap_cert[0x80..]), &ap_sig) {
        return Err(bad("the AP certificate's signature is wrong"));
    }
    if !ec::verify(&ap_public, &sha1(&sha1(&data[HEADER_SIZE..signed_end])), &sig) {
        return Err(bad("the save's signature is wrong"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    const WPSE: u64 = 0x0001_0001_5750_5345;

    fn key(name: &str) -> Option<[u8; 16]> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../keys").join(name);
        std::fs::read(p).ok()?.try_into().ok()
    }

    fn keys() -> Option<Keys> {
        Some(Keys { sd_key: key("sd-key")?, sd_iv: key("sd-iv")?, md5_blanker: key("md5-blanker")? })
    }

    /// A save downloaded by the user, if it is there (`RUMBLE_SAVE_DIR`, or
    /// the Downloads folder).
    fn sample(name: &str) -> Option<Vec<u8>> {
        let dir = std::env::var_os("RUMBLE_SAVE_DIR")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Downloads")))?;
        std::fs::read(dir.join(name)).ok()
    }

    /// A save exported by a real Wii reads, and its signatures check out:
    /// this tests the curve code and the signed layout against the console.
    #[test]
    fn reads_and_verifies_a_real_save() {
        let (Some(keys), Some(data)) = (keys(), sample("data.bin")) else {
            eprintln!("no keys or sample save; skipped");
            return;
        };
        let save = parse(&data, &keys, WPSE).unwrap();
        let names: Vec<_> = save.files.iter().map(|f| f.path.as_str()).collect();
        println!("{names:?}, banner {:#x}, permissions {:#x}", save.banner.len(), save.permissions);
        assert!(names.contains(&"savedata.bin"));
        verify_signature(&data).unwrap();
    }

    #[test]
    fn rejects_another_region() {
        let (Some(keys), Some(data)) = (keys(), sample("pokemon-rumble.29268.bin")) else { return };
        match parse(&data, &keys, WPSE) {
            Err(SaveError::WrongTitle { found, .. }) => assert_eq!(found, "WPSP"),
            other => panic!("expected the wrong title, got {other:?}"),
        }
    }

    /// An export reads back as what was exported, and its signatures check
    /// out.
    #[test]
    fn export_round_trips() {
        let (Some(keys), Some(data)) = (keys(), sample("data.bin")) else { return };
        let save = parse(&data, &keys, WPSE).unwrap();
        let out = build(&save, &keys, &Device::default()).unwrap();
        assert_eq!(out.len(), data.len(), "same layout as the console's");
        verify_signature(&out).unwrap();
        let mut back = parse(&out, &keys, WPSE).unwrap();
        // The export clears the banner's no-copy flag.
        let mut expected = save.clone();
        expected.banner[7] &= !1;
        back.banner[7] &= !1;
        assert_eq!(back, expected);
    }
}
