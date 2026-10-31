//! GameCube/Wii DOL executables.

use crate::be_u32;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DolError {
    #[error("DOL header is truncated")]
    Truncated,
    #[error("DOL section {0} lies outside the file")]
    BadSection(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionKind {
    Text,
    Data,
}

#[derive(Debug, Clone)]
pub struct Section<'a> {
    pub kind: SectionKind,
    /// Index within its kind (text0..text6, data0..data10).
    pub index: usize,
    pub addr: u32,
    pub data: &'a [u8],
}

impl Section<'_> {
    pub fn name(&self) -> String {
        match self.kind {
            SectionKind::Text => format!("text{}", self.index),
            SectionKind::Data => format!("data{}", self.index),
        }
    }

    pub fn end(&self) -> u32 {
        self.addr + self.data.len() as u32
    }

    pub fn contains(&self, addr: u32) -> bool {
        addr >= self.addr && addr < self.end()
    }
}

#[derive(Debug)]
pub struct Dol<'a> {
    pub sections: Vec<Section<'a>>,
    pub bss_addr: u32,
    pub bss_size: u32,
    pub entry: u32,
}

impl<'a> Dol<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, DolError> {
        if data.len() < 0x100 {
            return Err(DolError::Truncated);
        }
        let mut sections = Vec::new();
        for i in 0..18 {
            let (off, addr, size) = (
                be_u32(data, i * 4) as usize,
                be_u32(data, 0x48 + i * 4),
                be_u32(data, 0x90 + i * 4) as usize,
            );
            if size == 0 {
                continue;
            }
            let bytes = data.get(off..off + size).ok_or(DolError::BadSection(i))?;
            let (kind, index) = if i < 7 { (SectionKind::Text, i) } else { (SectionKind::Data, i - 7) };
            sections.push(Section { kind, index, addr, data: bytes });
        }
        Ok(Dol {
            sections,
            bss_addr: be_u32(data, 0xd8),
            bss_size: be_u32(data, 0xdc),
            entry: be_u32(data, 0xe0),
        })
    }

    pub fn text(&self) -> impl Iterator<Item = &Section<'a>> {
        self.sections.iter().filter(|s| s.kind == SectionKind::Text)
    }

    pub fn section_at(&self, addr: u32) -> Option<&Section<'a>> {
        self.sections.iter().find(|s| s.contains(addr))
    }

    pub fn is_code(&self, addr: u32) -> bool {
        addr & 3 == 0 && self.text().any(|s| s.contains(addr))
    }

    pub fn read_u32(&self, addr: u32) -> Option<u32> {
        let s = self.section_at(addr)?;
        let o = (addr - s.addr) as usize;
        s.data.get(o..o + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    }
}
