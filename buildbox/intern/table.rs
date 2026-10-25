//! The deduplicating table that symbols and paths are both interned in.

use crate::segvec::SegVec;
use hashbrown::HashTable;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// How many independently locked shards the lookup index is split into, so
/// that threads adding different values rarely wait for each other. Finding
/// a value already interned only shares its shard's lock.
const SHARDS: usize = 64;

/// Assigns each distinct value a dense `u32` id.
///
/// A value is anything that fits in 64 bits, such as a pointer to interned
/// text. Ids are handed out in order from zero, and an id's value can be read
/// without locking. The index from value to id stores only ids, hashing and
/// comparing them through their values, so it costs about five bytes an
/// entry.
pub(crate) struct Table {
    values: SegVec,
    next: AtomicU32,
    shards: Box<[RwLock<HashTable<u32>>]>,
}

/// How a [`Table`] hashes and compares values. Equal values must hash equally.
pub(crate) trait Hashing {
    fn hash(&self, value: u64) -> u64;
}

impl Table {
    pub(crate) fn new() -> Table {
        Table {
            values: SegVec::new(),
            next: AtomicU32::new(0),
            shards: (0..SHARDS).map(|_| RwLock::new(HashTable::new())).collect(),
        }
    }

    /// The value with the given id.
    pub(crate) fn value(&self, id: u32) -> u64 {
        self.values.get(id)
    }

    /// The id of the value that hashes to `hash` and matches `is_match`, if
    /// one has been interned.
    pub(crate) fn find(&self, hash: u64, is_match: impl Fn(u64) -> bool) -> Option<u32> {
        let shard = read(self.shard(hash));
        shard.find(hash, |&id| is_match(self.values.get(id))).copied()
    }

    /// The id of the value that hashes to `hash` and matches `is_match`,
    /// interning the value `make` returns if there is none.
    ///
    /// Panics if more than `u32::MAX` values are interned.
    pub(crate) fn intern(
        &self,
        hashing: &impl Hashing,
        hash: u64,
        is_match: impl Fn(u64) -> bool,
        make: impl FnOnce() -> u64,
    ) -> u32 {
        if let Some(id) = self.find(hash, &is_match) {
            return id;
        }
        let mut shard = write(self.shard(hash));
        if let Some(&id) = shard.find(hash, |&id| is_match(self.values.get(id))) {
            return id;
        }

        let id = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(id != u32::MAX, "interned more than u32::MAX values");
        self.values.set(id, make());
        shard.insert_unique(hash, id, |&id| hashing.hash(self.values.get(id)));
        id
    }

    /// How many values have been interned.
    pub(crate) fn len(&self) -> u32 {
        self.next.load(Ordering::Relaxed)
    }

    /// The bytes taken by the id-to-value table and the lookup index.
    pub(crate) fn allocated_bytes(&self) -> usize {
        let mut bytes = self.values.allocated_bytes();
        for shard in self.shards.iter() {
            let shard = read(shard);
            bytes += shard.capacity() * (std::mem::size_of::<u32>() + 1);
        }
        bytes
    }

    /// The shard for `hash`, chosen by bits that the shard's own table
    /// doesn't use: hashbrown places an entry by the hash's low bits and tags
    /// it with the top seven, which tell entries apart only if they vary.
    fn shard(&self, hash: u64) -> &RwLock<HashTable<u32>> {
        let index = (hash >> 32) as usize % SHARDS;
        &self.shards[index]
    }
}

/// A shard is consistent between calls into `hashbrown`, so a panic while it
/// was held doesn't make it unusable.
fn read(shard: &RwLock<HashTable<u32>>) -> RwLockReadGuard<'_, HashTable<u32>> {
    shard.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write(shard: &RwLock<HashTable<u32>>) -> RwLockWriteGuard<'_, HashTable<u32>> {
    shard.write().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::hash::BuildHasher;

    struct Strings(Vec<String>);

    impl Hashing for &Strings {
        fn hash(&self, value: u64) -> u64 {
            rustc_hash::FxBuildHasher.hash_one(self.0[value as usize].as_str())
        }
    }

    #[test]
    fn lookups_rarely_compare_other_values() {
        let strings = Strings((0..100_000).map(|i| format!("target_{i}")).collect());
        let hash = |s: &str| rustc_hash::FxBuildHasher.hash_one(s);
        let table = Table::new();
        for (i, s) in strings.0.iter().enumerate() {
            table.intern(&&strings, hash(s), |v| strings.0[v as usize] == *s, || i as u64);
        }
        let compares = Cell::new(0);
        for s in strings.0.iter() {
            let is_match = |v| {
                compares.set(compares.get() + 1);
                strings.0[v as usize] == *s
            };
            assert!(table.find(hash(s), is_match).is_some());
        }
        let per_lookup = compares.get() as f64 / strings.0.len() as f64;
        assert!(per_lookup < 1.1, "{per_lookup} compares per lookup");
    }
}
