//! Segment files: the store's values on disk.
//!
//! A segment is written once, by a checkpoint, with the stored bytes of the
//! values it saves, back to back, and never changed after. Values are read
//! from it through a read-only mapping. A segment's checksum is kept in the
//! store's image, so a damaged one is found when the store is restored.

use crate::mmap::Mmap;
use crate::pages::Location;
use skycodec::Fingerprint;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// A segment as an image describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentInfo {
    pub id: u32,
    pub len: u64,
    pub checksum: Fingerprint,
}

/// The segments a store reads values from.
#[derive(Default)]
pub(crate) struct Segments {
    mapped: RwLock<HashMap<u32, (SegmentInfo, Arc<Mmap>)>>,
}

impl Segments {
    pub(crate) fn get(&self, id: u32) -> Option<Arc<Mmap>> {
        self.lock_read().get(&id).map(|(_, map)| map.clone())
    }

    pub(crate) fn add(&self, info: SegmentInfo, map: Mmap) {
        let mut mapped = self.mapped.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        mapped.insert(info.id, (info, Arc::new(map)));
    }

    /// Every segment read from.
    pub(crate) fn infos(&self) -> Vec<SegmentInfo> {
        let mut infos: Vec<SegmentInfo> = self.lock_read().values().map(|(info, _)| *info).collect();
        infos.sort_by_key(|info| info.id);
        infos
    }

    /// Stop reading from the segments not in `keep`. Readers holding one
    /// keep it mapped until they finish.
    pub(crate) fn retain(&self, keep: &[SegmentInfo]) {
        let mut mapped = self.mapped.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        mapped.retain(|id, _| keep.iter().any(|segment| segment.id == *id));
    }

    pub(crate) fn next_id(&self) -> u32 {
        self.lock_read().keys().max().map_or(0, |id| id + 1)
    }

    fn lock_read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<u32, (SegmentInfo, Arc<Mmap>)>> {
        self.mapped.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The bytes at `location` in `segment`, if it holds them.
pub(crate) fn read(segment: &Mmap, location: Location) -> Option<&[u8]> {
    let start = location.offset as usize;
    segment.bytes().get(start..start + location.len as usize)
}

pub(crate) fn path(dir: &Path, id: u32) -> PathBuf {
    dir.join(format!("{id:08}.seg"))
}

/// Write a new segment with `records`, returning where each landed.
pub(crate) fn write(dir: &Path, id: u32, records: &[&[u8]]) -> io::Result<(SegmentInfo, Vec<Location>, Mmap)> {
    let mut contents = Vec::with_capacity(records.iter().map(|record| record.len()).sum());
    let mut locations = Vec::with_capacity(records.len());
    for record in records {
        let offset = u32::try_from(contents.len()).map_err(|_| io::Error::other("segment larger than 4 GiB"))?;
        contents.extend_from_slice(record);
        locations.push(Location {
            page: id,
            offset,
            len: record.len() as u32,
        });
    }
    std::fs::create_dir_all(dir)?;
    let path = path(dir, id);
    let mut file = File::create(&path)?;
    file.write_all(&contents)?;
    file.sync_data()?;
    let map = Mmap::map(&File::open(&path)?)?;
    let info = SegmentInfo {
        id,
        len: contents.len() as u64,
        checksum: Fingerprint::of(&contents),
    };
    Ok((info, locations, map))
}

/// Map a segment an image describes, checking it's what was written.
pub(crate) fn open(dir: &Path, info: &SegmentInfo) -> io::Result<Mmap> {
    let map = Mmap::map(&File::open(path(dir, info.id))?)?;
    if map.bytes().len() as u64 != info.len || Fingerprint::of(map.bytes()) != info.checksum {
        return Err(io::Error::other(format!("segment {} is damaged", info.id)));
    }
    Ok(map)
}

/// Delete the segment files in `dir` not in `keep`.
pub fn remove_unused(dir: &Path, keep: &[SegmentInfo]) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let path = entry?.path();
        let id = path.extension().filter(|extension| *extension == "seg").and_then(|_| path.file_stem()?.to_str()?.parse::<u32>().ok());
        let Some(id) = id else {
            continue;
        };
        if !keep.iter().any(|segment| segment.id == id) {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}
