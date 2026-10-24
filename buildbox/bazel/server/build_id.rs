//! Which build of bb this process is: the GNU build ID the linker computed
//! from the executable's contents, so that a client can tell whether a
//! running server is the same build as itself.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::OnceLock;

/// A program header for notes.
const PT_NOTE: u32 = 4;

/// A note holding the build ID.
const NT_GNU_BUILD_ID: u32 = 3;

/// The largest note segment read: build IDs are a few dozen bytes.
const MAX_NOTES_BYTES: u64 = 64 * 1024;

/// This process's build ID in hex, or `None` if its executable has none
/// or can't be read.
pub fn build_id() -> Option<&'static str> {
    static ID: OnceLock<Option<String>> = OnceLock::new();
    ID.get_or_init(|| read_build_id("/proc/self/exe")).as_deref()
}

/// The build ID of the 64-bit little-endian ELF file at `path`.
fn read_build_id(path: &str) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let mut header = [0u8; 64];
    file.read_exact(&mut header).ok()?;
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return None;
    }
    let program_headers = u64_at(&header, 0x20)?;
    let entry_size = u64::from(u16_at(&header, 0x36)?);
    let entries = u64::from(u16_at(&header, 0x38)?);
    for index in 0..entries {
        let mut entry = [0u8; 56];
        file.seek(SeekFrom::Start(program_headers + index * entry_size)).ok()?;
        file.read_exact(&mut entry).ok()?;
        if u32_at(&entry, 0)? != PT_NOTE {
            continue;
        }
        let notes = read_segment(&mut file, u64_at(&entry, 8)?, u64_at(&entry, 32)?)?;
        if let Some(id) = gnu_build_id(&notes) {
            return Some(id);
        }
    }
    None
}

fn read_segment(file: &mut File, offset: u64, size: u64) -> Option<Vec<u8>> {
    let mut bytes = vec![0u8; size.min(MAX_NOTES_BYTES) as usize];
    file.seek(SeekFrom::Start(offset)).ok()?;
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

/// The GNU build ID among a segment's notes, in hex.
fn gnu_build_id(notes: &[u8]) -> Option<String> {
    let mut offset = 0;
    while offset + 12 <= notes.len() {
        let name_size = u32_at(notes, offset)? as usize;
        let desc_size = u32_at(notes, offset + 4)? as usize;
        let kind = u32_at(notes, offset + 8)?;
        let name_start = offset + 12;
        let desc_start = name_start + name_size.next_multiple_of(4);
        let name = notes.get(name_start..name_start + name_size)?;
        let desc = notes.get(desc_start..desc_start + desc_size)?;
        if kind == NT_GNU_BUILD_ID && name == b"GNU\0" {
            return Some(desc.iter().map(|byte| format!("{byte:02x}")).collect());
        }
        offset = desc_start + desc_size.next_multiple_of(4);
    }
    None
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset + 8)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_are_searched_for_the_gnu_build_id() {
        let mut notes = Vec::new();
        for (name, kind, desc) in [(&b"GNU\0"[..], 1u32, &[0u8; 16][..]), (&b"GNU\0"[..], NT_GNU_BUILD_ID, &[0xab, 0xcd, 0x01][..])] {
            notes.extend((name.len() as u32).to_le_bytes());
            notes.extend((desc.len() as u32).to_le_bytes());
            notes.extend(kind.to_le_bytes());
            notes.extend(name);
            notes.extend(desc);
            notes.resize(notes.len().next_multiple_of(4), 0);
        }
        assert_eq!(gnu_build_id(&notes).as_deref(), Some("abcd01"));
        assert_eq!(gnu_build_id(&notes[..20]), None);
    }

    #[test]
    fn this_test_has_a_build_id() {
        assert!(build_id().is_some_and(|id| !id.is_empty()));
    }
}
