//! Small maps the server keeps across restarts.
//!
//! A [`PersistedMap`] holds fingerprints and small values in memory, and
//! saves them to one file: written to a temporary file and renamed, so a
//! crash leaves the old file or the new, and checksummed, so a damaged file
//! is ignored. Everything kept this way can be recomputed, so a map that
//! can't be read starts empty, and a map holds at most a set number of
//! entries, forgetting those used longest ago.

use skycodec::Fingerprint;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// Identifies the file format, and its version.
const MAGIC: &[u8; 8] = b"bbmap\0\0\x01";

/// A map from fingerprints to small values, saved to a file.
pub struct PersistedMap {
    path: PathBuf,
    max_entries: usize,
    inner: Mutex<Inner>,
    /// Held while saving, so saves write the file one at a time.
    saving: Mutex<()>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<Fingerprint, Entry>,
    /// Counts saves, so entries record when they were last used.
    generation: u32,
    dirty: bool,
}

struct Entry {
    value: Box<[u8]>,
    used: u32,
}

impl PersistedMap {
    /// The map saved at `path`, or an empty one if there's none or it can't
    /// be read.
    pub fn open(path: impl Into<PathBuf>, max_entries: usize) -> PersistedMap {
        let path = path.into();
        let inner = match std::fs::read(&path) {
            Ok(bytes) => parse(&bytes).unwrap_or_else(|| {
                tracing::warn!("ignoring {}, which is damaged", path.display());
                Inner::default()
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Inner::default(),
            Err(error) => {
                tracing::warn!("ignoring {}: {error}", path.display());
                Inner::default()
            }
        };
        tracing::info!("loaded {} entries from {}", inner.entries.len(), path.display());
        PersistedMap {
            path,
            max_entries,
            inner: Mutex::new(inner),
            saving: Mutex::new(()),
        }
    }

    /// The value for `key`, marking it used.
    pub fn get(&self, key: &Fingerprint) -> Option<Box<[u8]>> {
        let mut inner = self.lock();
        let generation = inner.generation;
        let entry = inner.entries.get_mut(key)?;
        entry.used = generation;
        Some(entry.value.clone())
    }

    pub fn insert(&self, key: Fingerprint, value: &[u8]) {
        let mut inner = self.lock();
        let used = inner.generation;
        inner.entries.insert(key, Entry { value: value.into(), used });
        inner.dirty = true;
    }

    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Save the map if it changed, first forgetting the entries used longest
    /// ago beyond the limit. Blocks on the filesystem.
    pub fn save(&self) -> io::Result<()> {
        let _saving = self.saving.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let bytes = {
            let mut inner = self.lock();
            inner.generation = inner.generation.wrapping_add(1);
            if !inner.dirty {
                return Ok(());
            }
            evict(&mut inner, self.max_entries);
            inner.dirty = false;
            serialize(&inner)
        };
        write_atomically(&self.path, &bytes)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Keep the `max_entries` entries used most recently.
fn evict(inner: &mut Inner, max_entries: usize) {
    if inner.entries.len() <= max_entries {
        return;
    }
    let mut uses: Vec<u32> = inner.entries.values().map(|entry| entry.used).collect();
    uses.sort_unstable_by(|a, b| b.cmp(a));
    let oldest_kept = uses[max_entries - 1];
    inner.entries.retain(|_, entry| entry.used >= oldest_kept);
    while inner.entries.len() > max_entries {
        let Some(key) = inner.entries.iter().find(|(_, entry)| entry.used == oldest_kept).map(|(key, _)| *key) else {
            break;
        };
        inner.entries.remove(&key);
    }
}

/// The file: the magic, the generation and entry count, each entry's key,
/// last use, length and value, then a checksum of everything before it.
fn serialize(inner: &Inner) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(32 + inner.entries.len() * 64);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&inner.generation.to_le_bytes());
    bytes.extend_from_slice(&(inner.entries.len() as u64).to_le_bytes());
    for (key, entry) in &inner.entries {
        bytes.extend_from_slice(key.as_bytes());
        bytes.extend_from_slice(&entry.used.to_le_bytes());
        bytes.extend_from_slice(&(entry.value.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&entry.value);
    }
    let checksum = Fingerprint::of(&bytes);
    bytes.extend_from_slice(checksum.as_bytes());
    bytes
}

fn parse(bytes: &[u8]) -> Option<Inner> {
    let body_len = bytes.len().checked_sub(Fingerprint::LEN)?;
    let (body, checksum) = bytes.split_at(body_len);
    if Fingerprint::of(body).as_bytes() != checksum {
        return None;
    }
    let mut reader = Reader(body);
    if reader.take(MAGIC.len())? != MAGIC {
        return None;
    }
    let generation = reader.u32()?;
    let count = reader.u64()?;
    let mut entries = HashMap::new();
    for _ in 0..count {
        let key = Fingerprint::from_bytes(reader.take(Fingerprint::LEN)?.try_into().ok()?);
        let used = reader.u32()?;
        let len = reader.u32()? as usize;
        let value = reader.take(len)?.into();
        entries.insert(key, Entry { value, used });
    }
    Some(Inner {
        entries,
        generation,
        dirty: false,
    })
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.0.len() < len {
            return None;
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Some(taken)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

/// Replace the file at `path` with `bytes`, so readers see the old file or
/// the new, never part of one.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension(format!("tmp{}", std::process::id()));
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_data()?;
    drop(file);
    std::fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u32) -> Fingerprint {
        Fingerprint::of(&n.to_le_bytes())
    }

    #[test]
    fn saves_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("map");
        let map = PersistedMap::open(&path, 10);
        map.insert(key(1), b"one");
        map.save().unwrap();
        let loaded = PersistedMap::open(&path, 10);
        assert_eq!(loaded.get(&key(1)).as_deref(), Some(&b"one"[..]));
        assert_eq!(loaded.get(&key(2)), None);
    }

    #[test]
    fn forgets_the_least_recently_used() {
        let dir = tempfile::tempdir().unwrap();
        let map = PersistedMap::open(dir.path().join("map"), 2);
        map.insert(key(1), b"a");
        map.insert(key(2), b"b");
        map.save().unwrap();
        map.get(&key(1));
        map.insert(key(3), b"c");
        map.save().unwrap();
        assert!(map.get(&key(1)).is_some());
        assert!(map.get(&key(3)).is_some());
        assert!(map.get(&key(2)).is_none());
    }

    #[test]
    fn ignores_a_damaged_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("map");
        let map = PersistedMap::open(&path, 10);
        map.insert(key(1), b"one");
        map.save().unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[12] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        assert!(PersistedMap::open(&path, 10).is_empty());
    }
}
