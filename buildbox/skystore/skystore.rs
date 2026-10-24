//! The store that holds every value in the build graph, as compactly as
//! possible.
//!
//! Values are byte strings, as encoded by `skycodec`, identified by their
//! [`Fingerprint`]. Storing a value that is already stored returns the same
//! [`ValueRef`], so values that many workspaces compute, such as those of a
//! shared `.bzl` file or toolchain, are held once.
//!
//! Each value is compressed with zstd. Values are mostly small and similar to
//! others of their kind, which compress poorly on their own, so the store
//! trains a dictionary for each [`Kind`] from the first values it sees and
//! compresses later ones with it. Compressed bytes are packed into large
//! pages, and the store compacts pages once enough of them is unused.
//!
//! Some values, such as evaluated Starlark modules, can't be encoded. The
//! store holds those as live objects, still deduplicated by fingerprint.
//!
//! Values are reference counted: [`ValueStore::insert`] and
//! [`ValueStore::retain`] add a reference, and [`ValueStore::release`]
//! removes one, freeing the value when none are left.

mod checkpoint;
mod compress;
mod dictionaries;
mod mmap;
mod pages;
mod segments;

pub use checkpoint::{SavedEntry, SavedSlot, StoreImage};
pub use segments::{SegmentInfo, remove_unused as remove_unused_segments};

use compress::Codec;
use dictionaries::Dictionaries;
use hashbrown::HashTable;
use pages::{Location, Pages};
use segments::Segments;
use skycodec::Fingerprint;
use std::any::Any;
use std::sync::{Arc, Mutex, MutexGuard};

/// How many independently locked shards values are spread over.
const SHARD_BITS: u32 = 4;
const SHARDS: usize = 1 << SHARD_BITS;

/// Values whose stored bytes fit in this many bytes are kept inside their
/// entry rather than in a page.
const INLINE_BYTES: usize = 12;

/// Refers to a stored value. Each distinct value has one `ValueRef`, so
/// comparing refs compares values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ValueRef(u32);

impl ValueRef {
    pub fn id(self) -> u32 {
        self.0
    }

    pub fn from_id(id: u32) -> ValueRef {
        ValueRef(id)
    }

    fn new(shard: usize, index: u32) -> ValueRef {
        ValueRef((index << SHARD_BITS) | shard as u32)
    }

    fn shard(self) -> usize {
        (self.0 as usize) & (SHARDS - 1)
    }

    fn index(self) -> usize {
        (self.0 >> SHARD_BITS) as usize
    }
}

/// A kind of value, such as a package or a configured target. Values of one
/// kind share a compression dictionary and are counted together in
/// [`StoreStats`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Kind(pub u16);

/// The result of [`ValueStore::insert_new`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inserted {
    pub value: ValueRef,
    /// Whether the value wasn't already stored.
    pub is_new: bool,
}

/// A value [`Shard::release`] freed, still encoded.
enum Freed {
    Bytes { kind: Kind, codec: Codec, stored: Vec<u8> },
    Live(LiveValue),
}

/// A live object held by the store.
pub type LiveValue = Arc<dyn Any + Send + Sync>;

/// A value read from the store.
#[derive(Clone)]
pub enum Stored {
    Bytes(Vec<u8>),
    Live(LiveValue),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    #[error("no value is stored for {0:?}")]
    Missing(ValueRef),

    #[error("stored value is corrupt: {0}")]
    Corrupt(&'static str),

    #[error("failed to decompress a stored value: {0}")]
    Decompress(&'static str),
}

/// How much the store holds, in total and for each kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// Values stored as bytes.
    pub values: u64,
    /// Values stored as live objects.
    pub live_values: u64,
    /// The bytes values would take uncompressed.
    pub raw_bytes: u64,
    /// The bytes they take compressed.
    pub stored_bytes: u64,
    /// The bytes taken by pages, including space not yet compacted away.
    pub page_bytes: u64,
    /// The bytes of pages holding live data.
    pub page_live_bytes: u64,
    /// The bytes taken by the entries describing each value, and the index.
    pub entry_bytes: u64,
    /// The bytes taken by compression dictionaries.
    pub dictionary_bytes: u64,
    /// The size live objects report for themselves.
    pub live_bytes: u64,
    /// The memory zstd's pooled working state holds.
    pub context_bytes: u64,
    /// The same, for each kind, indexed by kind.
    pub kinds: Vec<KindStats>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KindStats {
    pub values: u64,
    pub raw_bytes: u64,
    pub stored_bytes: u64,
    pub dictionary_bytes: u64,
}

/// Holds values in memory, compressed and deduplicated. See the
/// [module docs](self).
pub struct ValueStore {
    shards: Box<[Mutex<Shard>]>,
    dictionaries: Dictionaries,
    segments: Segments,
}

#[derive(Default)]
struct Shard {
    entries: Vec<Entry>,
    free: Vec<u32>,
    index: HashTable<u32>,
    pages: Pages,
    live: Vec<Option<LiveValue>>,
    free_live: Vec<u32>,
    kinds: Vec<KindStats>,
    live_bytes: u64,
}

/// What the store knows about one value, in 40 bytes.
#[derive(Clone, Copy)]
struct Entry {
    fingerprint: Fingerprint,
    refs: u32,
    kind: u16,
    slot: Slot,
}

/// Where an entry's data is. Freed entries are [`Slot::Free`] until reused.
#[derive(Clone, Copy)]
enum Slot {
    Free,
    Inline { codec: Codec, len: u8, bytes: [u8; INLINE_BYTES] },
    Paged { codec: Codec, location: Location },
    /// In a segment file; the location's page is the segment's id.
    Disk { codec: Codec, location: Location },
    Live { index: u32, size: u32 },
    /// A live object a restored store knows the fingerprint of but doesn't
    /// have, until it's computed again and stored.
    Absent,
}

impl ValueStore {
    pub fn new() -> ValueStore {
        ValueStore {
            shards: (0..SHARDS).map(|_| Mutex::new(Shard::default())).collect(),
            dictionaries: Dictionaries::default(),
            segments: Segments::default(),
        }
    }

    /// Store an encoded value, or add a reference to it if it is already
    /// stored. `fingerprint` must identify `bytes`, including their kind.
    pub fn insert(&self, kind: Kind, fingerprint: Fingerprint, bytes: &[u8]) -> ValueRef {
        self.insert_new(kind, fingerprint, bytes).value
    }

    /// Like [`ValueStore::insert`], also saying whether the value is new.
    /// Values that refer to other values use this to take references to
    /// them only once.
    pub fn insert_new(&self, kind: Kind, fingerprint: Fingerprint, bytes: &[u8]) -> Inserted {
        let existing = self.lock(shard_of(&fingerprint)).retain_existing(&fingerprint);
        if let Some(value) = existing {
            if self.is_absent(value) {
                let (codec, stored) = compress::compress(bytes, self.dictionaries.get(kind).as_ref());
                self.lock(value.shard()).fill_absent_bytes(value, kind, codec, &stored);
            }
            return Inserted { value, is_new: false };
        }

        let dictionary = self.dictionaries.get(kind);
        let (codec, stored) = compress::compress(bytes, dictionary.as_ref());
        let inserted = self.lock(shard_of(&fingerprint)).insert(kind, fingerprint, codec, &stored);

        if inserted.is_new && dictionary.is_none() && bytes.len() >= compress::MIN_COMPRESS_LEN {
            self.sample(kind, bytes);
        }
        inserted
    }

    /// Store a live object, or add a reference to the one already stored with
    /// the same fingerprint. `size` is roughly how many bytes it takes, for
    /// accounting.
    pub fn insert_live(&self, kind: Kind, fingerprint: Fingerprint, value: LiveValue, size: usize) -> ValueRef {
        let mut shard = self.lock(shard_of(&fingerprint));
        if let Some(existing) = shard.retain_existing(&fingerprint) {
            shard.fill_absent(existing, value, size);
            return existing;
        }
        shard.insert_live(kind, fingerprint, value, size)
    }

    /// Every live object of `kind` the store holds.
    pub fn live_of_kind(&self, kind: Kind) -> Vec<LiveValue> {
        let mut values = Vec::new();
        for shard in self.shards.iter() {
            let shard = self.lock_shard(shard);
            for entry in shard.entries.iter().filter(|entry| entry.kind == kind.0) {
                if let Slot::Live { index, .. } = entry.slot {
                    values.extend(shard.live[index as usize].clone());
                }
            }
        }
        values
    }

    /// Whether the value is a live object the store doesn't have: one a
    /// restored store knows only the fingerprint of.
    pub fn is_absent(&self, value: ValueRef) -> bool {
        self.lock(value.shard()).is_absent_at(value)
    }

    /// The stored value with this fingerprint, if any, with a reference added.
    pub fn find(&self, fingerprint: &Fingerprint) -> Option<ValueRef> {
        self.lock(shard_of(fingerprint)).retain_existing(fingerprint)
    }

    /// Add a reference to a stored value.
    pub fn retain(&self, value: ValueRef) {
        let mut shard = self.lock(value.shard());
        if let Some(entry) = shard.entry_mut(value) {
            entry.refs += 1;
        }
    }

    /// Remove a reference, freeing the value if it was the last. Returns
    /// whether it was freed.
    pub fn release(&self, value: ValueRef) -> bool {
        self.lock(value.shard()).release(value, &self.segments).is_some()
    }

    /// Remove a reference and, if it was the last, return the value that was
    /// freed, so that references it held can be released in turn.
    pub fn release_and_take(&self, value: ValueRef) -> Result<Option<Stored>, StoreError> {
        let Some(freed) = self.lock(value.shard()).release(value, &self.segments) else {
            return Ok(None);
        };
        let stored = match freed {
            Freed::Live(live) => Stored::Live(live),
            Freed::Bytes { kind, codec, stored } => {
                let dictionary = self.dictionaries.get(kind);
                Stored::Bytes(compress::decompress(codec, &stored, dictionary.as_deref())?)
            }
        };
        Ok(Some(stored))
    }

    /// Read a value.
    pub fn read(&self, value: ValueRef) -> Result<Stored, StoreError> {
        let (kind, codec, stored) = {
            let shard = self.lock(value.shard());
            let entry = *shard.entry(value).ok_or(StoreError::Missing(value))?;
            match entry.slot {
                Slot::Free | Slot::Absent => return Err(StoreError::Missing(value)),
                Slot::Live { index, .. } => {
                    let live = shard.live[index as usize].clone().ok_or(StoreError::Missing(value))?;
                    return Ok(Stored::Live(live));
                }
                Slot::Inline { codec, len, bytes } => (entry.kind, codec, bytes[..len as usize].to_vec()),
                Slot::Paged { codec, location } => (entry.kind, codec, shard.pages.read(location).to_vec()),
                Slot::Disk { codec, location } => {
                    drop(shard);
                    return self.read_disk(value, Kind(entry.kind), codec, location);
                }
            }
        };
        let dictionary = self.dictionaries.get(Kind(kind));
        let bytes = compress::decompress(codec, &stored, dictionary.as_deref())?;
        Ok(Stored::Bytes(bytes))
    }

    /// Read a value from its segment, decompressing straight from the
    /// mapping.
    fn read_disk(&self, value: ValueRef, kind: Kind, codec: Codec, location: Location) -> Result<Stored, StoreError> {
        let segment = self.segments.get(location.page).ok_or(StoreError::Missing(value))?;
        let stored = segments::read(&segment, location).ok_or(StoreError::Corrupt("value beyond the end of its segment"))?;
        let dictionary = self.dictionaries.get(kind);
        let bytes = compress::decompress(codec, stored, dictionary.as_deref())?;
        Ok(Stored::Bytes(bytes))
    }

    /// The fingerprint of a stored value.
    pub fn fingerprint(&self, value: ValueRef) -> Option<Fingerprint> {
        self.lock(value.shard()).entry(value).map(|entry| entry.fingerprint)
    }

    /// Report how much the store holds.
    pub fn stats(&self) -> StoreStats {
        let mut stats = StoreStats::default();
        for shard in self.shards.iter() {
            self.lock_shard(shard).add_stats(&mut stats);
        }
        stats.context_bytes = compress::context_bytes() as u64;
        for (index, kind) in stats.kinds.iter_mut().enumerate() {
            kind.dictionary_bytes = self.dictionaries.bytes(Kind(index as u16)) as u64;
            stats.dictionary_bytes += kind.dictionary_bytes;
        }
        stats
    }

    /// Collect a sample for the kind's dictionary, training it and
    /// recompressing the kind's values with it once there are enough.
    fn sample(&self, kind: Kind, bytes: &[u8]) {
        let Some(samples) = self.dictionaries.add_sample(kind, bytes) else {
            return;
        };
        let _span = tracing::info_span!("skystore.train_dictionary", kind = kind.0, samples = samples.len()).entered();
        let Some(dictionary) = self.dictionaries.train(kind, &samples) else {
            return;
        };
        for shard in self.shards.iter() {
            self.lock_shard(shard).recompress(kind, &dictionary);
        }
    }

    fn lock(&self, shard: usize) -> MutexGuard<'_, Shard> {
        self.lock_shard(&self.shards[shard])
    }

    /// Shards are consistent between method calls, so one whose lock was
    /// poisoned is still usable.
    fn lock_shard<'a>(&self, shard: &'a Mutex<Shard>) -> MutexGuard<'a, Shard> {
        shard.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for ValueStore {
    fn default() -> ValueStore {
        ValueStore::new()
    }
}

fn shard_of(fingerprint: &Fingerprint) -> usize {
    (fingerprint.short() as usize) & (SHARDS - 1)
}

impl Shard {
    fn entry(&self, value: ValueRef) -> Option<&Entry> {
        self.entries.get(value.index()).filter(|entry| !matches!(entry.slot, Slot::Free))
    }

    fn entry_mut(&mut self, value: ValueRef) -> Option<&mut Entry> {
        self.entries.get_mut(value.index()).filter(|entry| !matches!(entry.slot, Slot::Free))
    }

    fn find(&self, fingerprint: &Fingerprint) -> Option<u32> {
        let entries = &self.entries;
        self.index
            .find(fingerprint.short(), |&index| entries[index as usize].fingerprint == *fingerprint)
            .copied()
    }

    fn retain_existing(&mut self, fingerprint: &Fingerprint) -> Option<ValueRef> {
        let index = self.find(fingerprint)?;
        self.entries[index as usize].refs += 1;
        Some(ValueRef::new(shard_of(fingerprint), index))
    }

    fn insert(&mut self, kind: Kind, fingerprint: Fingerprint, codec: Codec, stored: &[u8]) -> Inserted {
        if let Some(value) = self.retain_existing(&fingerprint) {
            return Inserted { value, is_new: false };
        }
        let slot = self.place(codec, stored);
        self.count(kind, codec, stored, 1);
        let value = self.add_entry(kind, fingerprint, slot);
        Inserted { value, is_new: true }
    }

    fn insert_live(&mut self, kind: Kind, fingerprint: Fingerprint, value: LiveValue, size: usize) -> ValueRef {
        let slot = self.push_live(value, size);
        self.add_entry(kind, fingerprint, slot)
    }

    fn is_absent_at(&self, value: ValueRef) -> bool {
        self.entry(value).is_some_and(|entry| matches!(entry.slot, Slot::Absent))
    }

    /// Give an absent entry the live object it stood for.
    fn fill_absent(&mut self, value: ValueRef, live: LiveValue, size: usize) {
        if self.is_absent_at(value) {
            self.entries[value.index()].slot = self.push_live(live, size);
        }
    }

    /// Give an absent entry the bytes it stood for.
    fn fill_absent_bytes(&mut self, value: ValueRef, kind: Kind, codec: Codec, stored: &[u8]) {
        if self.is_absent_at(value) {
            self.count(kind, codec, stored, 1);
            self.entries[value.index()].slot = self.place(codec, stored);
        }
    }

    /// Hold a live object, returning its slot.
    fn push_live(&mut self, live: LiveValue, size: usize) -> Slot {
        let index = match self.free_live.pop() {
            Some(index) => {
                self.live[index as usize] = Some(live);
                index
            }
            None => {
                self.live.push(Some(live));
                (self.live.len() - 1) as u32
            }
        };
        let size = u32::try_from(size).unwrap_or(u32::MAX);
        self.live_bytes += size as u64;
        Slot::Live { index, size }
    }

    fn place(&mut self, codec: Codec, stored: &[u8]) -> Slot {
        if stored.len() <= INLINE_BYTES {
            let mut bytes = [0u8; INLINE_BYTES];
            bytes[..stored.len()].copy_from_slice(stored);
            return Slot::Inline {
                codec,
                len: stored.len() as u8,
                bytes,
            };
        }
        Slot::Paged {
            codec,
            location: self.pages.alloc(stored),
        }
    }

    fn add_entry(&mut self, kind: Kind, fingerprint: Fingerprint, slot: Slot) -> ValueRef {
        let entry = Entry {
            fingerprint,
            refs: 1,
            kind: kind.0,
            slot,
        };
        let index = match self.free.pop() {
            Some(index) => {
                self.entries[index as usize] = entry;
                index
            }
            None => {
                self.entries.push(entry);
                (self.entries.len() - 1) as u32
            }
        };
        let entries = &self.entries;
        self.index.insert_unique(fingerprint.short(), index, |&i| entries[i as usize].fingerprint.short());
        ValueRef::new(shard_of(&fingerprint), index)
    }

    fn release(&mut self, value: ValueRef, segments: &Segments) -> Option<Freed> {
        let entry = self.entry_mut(value)?;
        entry.refs -= 1;
        if entry.refs > 0 {
            return None;
        }
        let entry = *entry;
        let freed = self.contents(&entry, segments);
        self.free_entry(value.index() as u32, entry, segments);
        if self.pages.should_compact() {
            self.compact();
        }
        freed
    }

    fn contents(&self, entry: &Entry, segments: &Segments) -> Option<Freed> {
        let kind = Kind(entry.kind);
        match entry.slot {
            Slot::Free | Slot::Absent => None,
            Slot::Disk { codec, location } => {
                let segment = segments.get(location.page)?;
                Some(Freed::Bytes {
                    kind,
                    codec,
                    stored: segments::read(&segment, location)?.to_vec(),
                })
            }
            Slot::Live { index, .. } => self.live[index as usize].clone().map(Freed::Live),
            Slot::Inline { codec, len, bytes } => Some(Freed::Bytes {
                kind,
                codec,
                stored: bytes[..len as usize].to_vec(),
            }),
            Slot::Paged { codec, location } => Some(Freed::Bytes {
                kind,
                codec,
                stored: self.pages.read(location).to_vec(),
            }),
        }
    }

    fn free_entry(&mut self, index: u32, entry: Entry, segments: &Segments) {
        let removed = self
            .index
            .find_entry(entry.fingerprint.short(), |&i| i == index)
            .map(|found| found.remove());
        debug_assert!(removed.is_ok(), "every stored entry is indexed");

        match entry.slot {
            Slot::Free | Slot::Absent => {}
            Slot::Disk { codec, location } => {
                if let Some(stored) = segments.get(location.page).and_then(|segment| segments::read(&segment, location).map(<[u8]>::to_vec)) {
                    self.count(Kind(entry.kind), codec, &stored, -1);
                }
            }
            Slot::Inline { codec, len, bytes } => self.count(Kind(entry.kind), codec, &bytes[..len as usize], -1),
            Slot::Paged { codec, location } => {
                let stored = self.pages.read(location).to_vec();
                self.count(Kind(entry.kind), codec, &stored, -1);
                self.pages.free(location);
            }
            Slot::Live { index, size } => {
                self.live[index as usize] = None;
                self.free_live.push(index);
                self.live_bytes -= size as u64;
            }
        }
        self.entries[index as usize].slot = Slot::Free;
        self.free.push(index);
    }

    fn compact(&mut self) {
        let _span = tracing::debug_span!("skystore.compact").entered();
        let live: Vec<(usize, Location)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| match entry.slot {
                Slot::Paged { location, .. } => Some((index, location)),
                _ => None,
            })
            .collect();
        let mut moved = Vec::with_capacity(live.len());
        self.pages.compact(live.iter().map(|(_, location)| *location), |_, new| moved.push(new));
        for ((index, _), new) in live.into_iter().zip(moved) {
            if let Slot::Paged { location, .. } = &mut self.entries[index].slot {
                *location = new;
            }
        }
    }

    /// Recompress the values of `kind` that were stored before it had a
    /// dictionary, keeping the new encoding where it is smaller.
    fn recompress(&mut self, kind: Kind, dictionary: &Arc<compress::Dictionary>) {
        for index in 0..self.entries.len() {
            let entry = self.entries[index];
            if entry.kind != kind.0 {
                continue;
            }
            let (codec, stored) = match entry.slot {
                Slot::Paged { codec, location } => (codec, self.pages.read(location).to_vec()),
                Slot::Inline { codec, len, bytes } => (codec, bytes[..len as usize].to_vec()),
                Slot::Free | Slot::Live { .. } | Slot::Disk { .. } | Slot::Absent => continue,
            };
            if codec == Codec::ZstdDict {
                continue;
            }
            let Ok(raw) = compress::decompress(codec, &stored, None) else {
                continue;
            };
            let (new_codec, new_stored) = compress::compress(&raw, Some(dictionary));
            if new_stored.len() >= stored.len() {
                continue;
            }
            if let Slot::Paged { location, .. } = entry.slot {
                self.pages.free(location);
            }
            self.count(kind, codec, &stored, -1);
            self.count(kind, new_codec, &new_stored, 1);
            self.entries[index].slot = self.place(new_codec, &new_stored);
        }
    }

    fn count(&mut self, kind: Kind, codec: Codec, stored: &[u8], sign: i64) {
        let index = kind.0 as usize;
        if self.kinds.len() <= index {
            self.kinds.resize(index + 1, KindStats::default());
        }
        let stats = &mut self.kinds[index];
        let raw = compress::raw_len(codec, stored) as u64;
        let stored = stored.len() as u64;
        if sign > 0 {
            stats.values += 1;
            stats.raw_bytes += raw;
            stats.stored_bytes += stored;
        } else {
            stats.values -= 1;
            stats.raw_bytes -= raw;
            stats.stored_bytes -= stored;
        }
    }

    fn add_stats(&self, stats: &mut StoreStats) {
        if stats.kinds.len() < self.kinds.len() {
            stats.kinds.resize(self.kinds.len(), KindStats::default());
        }
        for (total, kind) in stats.kinds.iter_mut().zip(&self.kinds) {
            total.values += kind.values;
            total.raw_bytes += kind.raw_bytes;
            total.stored_bytes += kind.stored_bytes;
            stats.values += kind.values;
            stats.raw_bytes += kind.raw_bytes;
            stats.stored_bytes += kind.stored_bytes;
        }
        stats.live_values += (self.live.len() - self.free_live.len()) as u64;
        stats.live_bytes += self.live_bytes;
        stats.page_bytes += self.pages.allocated_bytes() as u64;
        stats.page_live_bytes += self.pages.live_bytes() as u64;
        let entry_bytes = self.entries.capacity() * std::mem::size_of::<Entry>()
            + self.index.capacity() * (std::mem::size_of::<u32>() + 1)
            + self.live.capacity() * std::mem::size_of::<Option<LiveValue>>();
        stats.entry_bytes += entry_bytes as u64;
    }
}

#[cfg(test)]
mod tests;
