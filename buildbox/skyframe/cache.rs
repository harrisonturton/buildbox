//! A cache of decoded values, so values in use aren't decompressed and
//! decoded on every read.

use rustc_hash::FxHashMap;
use std::any::Any;
use std::sync::{Arc, Mutex, MutexGuard};

const SHARDS: usize = 16;

/// A decoded value.
pub(crate) type Decoded = Arc<dyn Any + Send + Sync>;

/// Keeps recently used decoded values, up to a budget of bytes.
///
/// Each shard has two generations. Values are added to the young one; when it
/// fills half the budget, it becomes the old one and the previous old
/// generation is dropped. A value read from the old generation moves back to
/// the young one. This approximates least-recently-used eviction at a
/// constant cost per operation.
pub(crate) struct ValueCache {
    shards: Box<[Mutex<Generations>]>,
    generation_bytes: usize,
}

#[derive(Default)]
struct Generations {
    young: FxHashMap<u32, (Decoded, usize)>,
    old: FxHashMap<u32, (Decoded, usize)>,
    young_bytes: usize,
    old_bytes: usize,
}

/// How much the cache holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub entries: u64,
    pub bytes: u64,
}

impl ValueCache {
    pub(crate) fn new(budget_bytes: usize) -> ValueCache {
        ValueCache {
            shards: (0..SHARDS).map(|_| Mutex::new(Generations::default())).collect(),
            generation_bytes: (budget_bytes / SHARDS / 2).max(1),
        }
    }

    pub(crate) fn get(&self, id: u32) -> Option<Decoded> {
        let mut shard = self.shard(id);
        if let Some((value, _)) = shard.young.get(&id) {
            return Some(value.clone());
        }
        let (value, size) = shard.old.remove(&id)?;
        shard.old_bytes -= size;
        shard.insert(id, value.clone(), size, self.generation_bytes);
        Some(value)
    }

    /// Cache a value, whose decoded size is about `size` bytes.
    pub(crate) fn insert(&self, id: u32, value: Decoded, size: usize) {
        self.shard(id).insert(id, value, size, self.generation_bytes);
    }

    pub(crate) fn remove(&self, id: u32) {
        let mut shard = self.shard(id);
        if let Some((_, size)) = shard.young.remove(&id) {
            shard.young_bytes -= size;
        }
        if let Some((_, size)) = shard.old.remove(&id) {
            shard.old_bytes -= size;
        }
    }

    pub(crate) fn stats(&self) -> CacheStats {
        let mut stats = CacheStats::default();
        for shard in self.shards.iter() {
            let shard = lock(shard);
            stats.entries += (shard.young.len() + shard.old.len()) as u64;
            stats.bytes += (shard.young_bytes + shard.old_bytes) as u64;
        }
        stats
    }

    fn shard(&self, id: u32) -> MutexGuard<'_, Generations> {
        lock(&self.shards[id as usize % SHARDS])
    }
}

impl Generations {
    fn insert(&mut self, id: u32, value: Decoded, size: usize, generation_bytes: usize) {
        if let Some((_, old_size)) = self.young.insert(id, (value, size)) {
            self.young_bytes -= old_size;
        }
        self.young_bytes += size;
        if self.young_bytes > generation_bytes {
            self.old = std::mem::take(&mut self.young);
            self.old_bytes = self.young_bytes;
            self.young_bytes = 0;
        }
    }
}

fn lock(shard: &Mutex<Generations>) -> MutexGuard<'_, Generations> {
    shard.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_evicted_beyond_the_budget() {
        let cache = ValueCache::new(SHARDS * 2 * 100);
        for id in 0..1000u32 {
            cache.insert(id * SHARDS as u32, Arc::new(id), 10);
        }
        let stats = cache.stats();
        assert!(stats.bytes <= 220, "{stats:?}");
        assert!(cache.get(999 * SHARDS as u32).is_some());
        assert!(cache.get(0).is_none());
    }

    #[test]
    fn reads_keep_values_cached() {
        let cache = ValueCache::new(SHARDS * 2 * 100);
        cache.insert(0, Arc::new(0u32), 10);
        for id in 1..100u32 {
            cache.insert(id * SHARDS as u32, Arc::new(id), 10);
            assert!(cache.get(0).is_some(), "evicted after {id}");
        }
    }
}
