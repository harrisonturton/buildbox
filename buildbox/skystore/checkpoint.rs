//! Saving the store to segment files, and restoring it.
//!
//! A checkpoint writes the values it's asked to save that aren't on disk
//! yet into a new segment, and from then on reads them from it, so their
//! bytes leave the store's memory. It describes what it saved as a
//! [`StoreImage`]: each value's entry, at the shard and index it has, so
//! that references to values stay valid in the restored store. Live
//! objects can't be saved; their entries are saved without them, as
//! absent, so that when the value is computed again it gets its old
//! reference back.
//!
//! Segments that have become mostly unused are rewritten into the new one
//! and dropped.

use crate::compress::Codec;
use crate::pages::Location;
use crate::segments::{self, SegmentInfo};
use crate::{Entry, Kind, Shard, Slot, ValueRef, ValueStore, shard_of};
use skycodec::{Decode, DecodeError, Decoder, Encode, Encoder, Fingerprint};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;

/// What a checkpoint saved: enough to restore the store.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreImage {
    pub entries: Vec<SavedEntry>,
    /// Each kind's compression dictionary.
    pub dictionaries: Vec<(u16, Vec<u8>)>,
    pub segments: Vec<SegmentInfo>,
}

/// A saved value's entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedEntry {
    pub value: ValueRef,
    pub fingerprint: Fingerprint,
    pub kind: u16,
    /// The references the restored store starts with.
    pub refs: u32,
    pub slot: SavedSlot,
}

/// Where a saved value's bytes are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SavedSlot {
    Inline { codec: u8, bytes: Vec<u8> },
    Disk { codec: u8, segment: u32, offset: u32, len: u32 },
    /// A live object, not saved.
    Absent,
}

impl StoreImage {
    /// The bytes the image's segments take on disk.
    pub fn disk_bytes(&self) -> u64 {
        self.segments.iter().map(|segment| segment.len).sum()
    }
}

/// A value whose bytes the checkpoint writes to its new segment.
struct Pending {
    value: ValueRef,
    fingerprint: Fingerprint,
    codec: Codec,
    stored: Vec<u8>,
    /// Where the bytes were in memory, to free once they're on disk.
    from: Slot,
}

impl ValueStore {
    /// Save the values `refs` lists, with the references each should start
    /// with when restored, to segments in `dir`. Blocks on the filesystem.
    pub fn checkpoint(&self, dir: &Path, refs: &[(ValueRef, u32)]) -> io::Result<StoreImage> {
        let rewrite = self.segments_to_rewrite();
        let saving: HashMap<ValueRef, u32> = refs.iter().copied().collect();
        let mut entries = Vec::with_capacity(refs.len());
        let mut pending = Vec::new();
        for (index, shard) in self.shards.iter().enumerate() {
            let shard = self.lock_shard(shard);
            collect(index, &shard, &saving, &rewrite, &self.segments, &mut entries, &mut pending);
        }

        let mut written = HashMap::new();
        if !pending.is_empty() {
            let id = self.segments.next_id();
            let records: Vec<&[u8]> = pending.iter().map(|pending| pending.stored.as_slice()).collect();
            let (info, locations, map) = segments::write(dir, id, &records)?;
            self.segments.add(info, map);
            for (pending, location) in pending.iter().zip(locations) {
                self.move_to_disk(pending, location);
                written.insert(pending.value, (pending.codec, location));
            }
        }
        for entry in &mut entries {
            if let (SavedSlot::Absent, Some((codec, location))) = (&entry.slot, written.get(&entry.value)) {
                entry.slot = disk_slot(*codec, *location);
            }
        }

        let used = self.segments_in_use();
        self.segments.retain(&used);
        Ok(StoreImage {
            entries,
            dictionaries: self.dictionaries.export(),
            segments: used,
        })
    }

    /// The store an image describes, reading values from its segments in
    /// `dir`.
    pub fn restore(dir: &Path, image: &StoreImage) -> io::Result<ValueStore> {
        let store = ValueStore::new();
        for (kind, content) in &image.dictionaries {
            store.dictionaries.import(Kind(*kind), content.clone());
        }
        for info in &image.segments {
            store.segments.add(*info, segments::open(dir, info)?);
        }
        for saved in &image.entries {
            store.restore_entry(saved)?;
        }
        for shard in store.shards.iter() {
            store.lock_shard(shard).collect_free();
        }
        Ok(store)
    }

    /// The segments whose values in use take less than half of them.
    fn segments_to_rewrite(&self) -> HashSet<u32> {
        let mut used: HashMap<u32, u64> = HashMap::new();
        for shard in self.shards.iter() {
            for entry in &self.lock_shard(shard).entries {
                if let Slot::Disk { location, .. } = entry.slot {
                    *used.entry(location.page).or_default() += location.len as u64;
                }
            }
        }
        let infos = self.segments.infos();
        let sparse = infos.iter().filter(|info| used.get(&info.id).copied().unwrap_or(0) * 2 < info.len);
        sparse.map(|info| info.id).collect()
    }

    /// The segments some entry is in.
    fn segments_in_use(&self) -> Vec<SegmentInfo> {
        let mut ids = HashSet::new();
        for shard in self.shards.iter() {
            for entry in &self.lock_shard(shard).entries {
                if let Slot::Disk { location, .. } = entry.slot {
                    ids.insert(location.page);
                }
            }
        }
        self.segments.infos().into_iter().filter(|info| ids.contains(&info.id)).collect()
    }

    /// Point a value's entry at its bytes' new place on disk, freeing them
    /// from memory, unless the entry has changed since it was collected.
    fn move_to_disk(&self, pending: &Pending, location: Location) {
        let mut shard = self.lock(pending.value.shard());
        let Some(entry) = shard.entries.get(pending.value.index()).copied() else {
            return;
        };
        if entry.fingerprint != pending.fingerprint || !same_place(&entry.slot, &pending.from) {
            return;
        }
        if let Slot::Paged { location: paged, .. } = entry.slot {
            shard.pages.free(paged);
        }
        shard.entries[pending.value.index()].slot = Slot::Disk {
            codec: pending.codec,
            location,
        };
    }

    fn restore_entry(&self, saved: &SavedEntry) -> io::Result<()> {
        let corrupt = |what: &str| io::Error::other(format!("store image: {what}"));
        if shard_of(&saved.fingerprint) != saved.value.shard() {
            return Err(corrupt("an entry is in the wrong shard"));
        }
        let slot = match &saved.slot {
            SavedSlot::Absent => Slot::Absent,
            SavedSlot::Inline { codec, bytes } => {
                let codec = Codec::from_u8(*codec).ok_or_else(|| corrupt("unknown codec"))?;
                let mut inline = [0u8; crate::INLINE_BYTES];
                inline.get_mut(..bytes.len()).ok_or_else(|| corrupt("inline value too long"))?.copy_from_slice(bytes);
                Slot::Inline {
                    codec,
                    len: bytes.len() as u8,
                    bytes: inline,
                }
            }
            SavedSlot::Disk { codec, segment, offset, len } => {
                let codec = Codec::from_u8(*codec).ok_or_else(|| corrupt("unknown codec"))?;
                let location = Location {
                    page: *segment,
                    offset: *offset,
                    len: *len,
                };
                let map = self.segments.get(*segment).ok_or_else(|| corrupt("a value's segment is missing"))?;
                let stored = segments::read(&map, location).ok_or_else(|| corrupt("a value is beyond its segment"))?;
                self.lock(saved.value.shard()).count(Kind(saved.kind), codec, stored, 1);
                Slot::Disk { codec, location }
            }
        };
        if let Slot::Inline { codec, len, bytes } = slot {
            self.lock(saved.value.shard()).count(Kind(saved.kind), codec, &bytes[..len as usize], 1);
        }
        let mut shard = self.lock(saved.value.shard());
        shard.place_restored(saved.value.index(), Entry {
            fingerprint: saved.fingerprint,
            refs: saved.refs.max(1),
            kind: saved.kind,
            slot,
        })
    }
}

/// Add the entries of one shard that are being saved to `entries`, and
/// the bytes to write to the new segment to `pending`: saved values still
/// in memory, and any value in a segment being rewritten.
fn collect(
    index: usize,
    shard: &Shard,
    saving: &HashMap<ValueRef, u32>,
    rewrite: &HashSet<u32>,
    segments: &crate::segments::Segments,
    entries: &mut Vec<SavedEntry>,
    pending: &mut Vec<Pending>,
) {
    for (slot_index, entry) in shard.entries.iter().enumerate() {
        let value = ValueRef::new(index, slot_index as u32);
        let refs = saving.get(&value).copied();
        let moving = match entry.slot {
            Slot::Paged { codec, location } if refs.is_some() => Some((codec, shard.pages.read(location).to_vec())),
            Slot::Disk { codec, location } if rewrite.contains(&location.page) => {
                segments.get(location.page).and_then(|map| Some((codec, segments::read(&map, location)?.to_vec())))
            }
            _ => None,
        };
        if let Some((codec, stored)) = moving {
            pending.push(Pending {
                value,
                fingerprint: entry.fingerprint,
                codec,
                stored,
                from: entry.slot,
            });
        }
        let Some(refs) = refs else {
            continue;
        };
        let slot = match entry.slot {
            Slot::Free => continue,
            Slot::Inline { codec, len, bytes } => SavedSlot::Inline {
                codec: codec as u8,
                bytes: bytes[..len as usize].to_vec(),
            },
            Slot::Disk { codec, location } if !rewrite.contains(&location.page) => disk_slot(codec, location),
            Slot::Paged { .. } | Slot::Disk { .. } | Slot::Live { .. } | Slot::Absent => SavedSlot::Absent,
        };
        entries.push(SavedEntry {
            value,
            fingerprint: entry.fingerprint,
            kind: entry.kind,
            refs,
            slot,
        });
    }
}

fn disk_slot(codec: Codec, location: Location) -> SavedSlot {
    SavedSlot::Disk {
        codec: codec as u8,
        segment: location.page,
        offset: location.offset,
        len: location.len,
    }
}

fn same_place(a: &Slot, b: &Slot) -> bool {
    match (a, b) {
        (Slot::Paged { location: a, .. }, Slot::Paged { location: b, .. }) | (Slot::Disk { location: a, .. }, Slot::Disk { location: b, .. }) => a == b,
        _ => false,
    }
}

impl Shard {
    /// Put a restored entry at its old index, leaving free entries before it.
    fn place_restored(&mut self, index: usize, entry: Entry) -> io::Result<()> {
        if self.entries.len() <= index {
            self.entries.resize(index + 1, Entry {
                fingerprint: Fingerprint::from_bytes([0; Fingerprint::LEN]),
                refs: 0,
                kind: 0,
                slot: Slot::Free,
            });
        }
        if !matches!(self.entries[index].slot, Slot::Free) {
            return Err(io::Error::other("store image: two entries at one index"));
        }
        self.entries[index] = entry;
        let entries = &self.entries;
        self.index.insert_unique(entry.fingerprint.short(), index as u32, |&i| entries[i as usize].fingerprint.short());
        Ok(())
    }

    /// Make the restored store's unused entries free for reuse.
    fn collect_free(&mut self) {
        self.free = (0..self.entries.len() as u32).filter(|&index| matches!(self.entries[index as usize].slot, Slot::Free)).collect();
    }
}

impl Encode for StoreImage {
    fn encode(&self, e: &mut Encoder) {
        e.usize(self.dictionaries.len());
        for (kind, content) in &self.dictionaries {
            e.u32(*kind as u32);
            e.bytes(content);
        }
        e.usize(self.segments.len());
        for segment in &self.segments {
            e.u32(segment.id);
            e.u64(segment.len);
            e.raw(segment.checksum.as_bytes());
        }
        e.usize(self.entries.len());
        for entry in &self.entries {
            e.u32(entry.value.id());
            e.raw(entry.fingerprint.as_bytes());
            e.u32(entry.kind as u32);
            e.u32(entry.refs);
            match &entry.slot {
                SavedSlot::Absent => e.u8(0),
                SavedSlot::Inline { codec, bytes } => {
                    e.u8(1);
                    e.u8(*codec);
                    e.bytes(bytes);
                }
                SavedSlot::Disk { codec, segment, offset, len } => {
                    e.u8(2);
                    e.u8(*codec);
                    e.u32(*segment);
                    e.u32(*offset);
                    e.u32(*len);
                }
            }
        }
    }
}

impl Decode for StoreImage {
    fn decode(d: &mut Decoder<'_>) -> Result<StoreImage, DecodeError> {
        let mut image = StoreImage::default();
        for _ in 0..d.usize()? {
            let kind = d.u32()? as u16;
            image.dictionaries.push((kind, d.bytes()?.to_vec()));
        }
        for _ in 0..d.usize()? {
            let id = d.u32()?;
            let len = d.u64()?;
            let checksum = fingerprint(d)?;
            image.segments.push(SegmentInfo { id, len, checksum });
        }
        for _ in 0..d.usize()? {
            let value = ValueRef::from_id(d.u32()?);
            let fingerprint = fingerprint(d)?;
            let kind = d.u32()? as u16;
            let refs = d.u32()?;
            let slot = match d.u8()? {
                0 => SavedSlot::Absent,
                1 => SavedSlot::Inline {
                    codec: d.u8()?,
                    bytes: d.bytes()?.to_vec(),
                },
                _ => SavedSlot::Disk {
                    codec: d.u8()?,
                    segment: d.u32()?,
                    offset: d.u32()?,
                    len: d.u32()?,
                },
            };
            image.entries.push(SavedEntry {
                value,
                fingerprint,
                kind,
                refs,
                slot,
            });
        }
        Ok(image)
    }
}

fn fingerprint(d: &mut Decoder<'_>) -> Result<Fingerprint, DecodeError> {
    let bytes = d.raw(Fingerprint::LEN)?;
    let bytes: [u8; Fingerprint::LEN] = bytes.try_into().map_err(|_| DecodeError::Invalid {
        what: "fingerprint",
        value: 0,
        position: d.position(),
    })?;
    Ok(Fingerprint::from_bytes(bytes))
}
