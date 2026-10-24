//! Sharing equal values between the targets and actions that hold them.
//!
//! Many actions hold the same large values, such as the runfiles tree of a
//! tool every compile runs. A [`Sharing`] table hands out one `Arc` for all
//! equal values that are alive at once, and forgets values once nothing
//! holds them.

use rustc_hash::FxHasher;
use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

const SHARDS: usize = 16;

type ByHash<T> = HashMap<u64, Vec<Weak<T>>, BuildHasherDefault<FxHasher>>;

/// A table of values alive somewhere, by their contents.
pub struct Sharing<T> {
    shards: [Mutex<Shard<T>>; SHARDS],
}

struct Shard<T> {
    values: ByHash<T>,
    /// How many values the shard may hold before dead ones are swept.
    sweep_at: usize,
}

impl<T: Hash + Eq> Default for Sharing<T> {
    fn default() -> Sharing<T> {
        Sharing {
            shards: std::array::from_fn(|_| {
                Mutex::new(Shard {
                    values: HashMap::default(),
                    sweep_at: 64,
                })
            }),
        }
    }
}

impl<T: Hash + Eq> Sharing<T> {
    /// The shared value equal to `value`: one already alive, or `value`.
    pub fn share(&self, value: T) -> Arc<T> {
        let hash = hash_of(&value);
        let mut shard = self.shard(hash);
        let candidates = shard.values.entry(hash).or_default();
        if let Some(existing) = candidates.iter().filter_map(Weak::upgrade).find(|existing| **existing == value) {
            return existing;
        }
        let shared = Arc::new(value);
        candidates.push(Arc::downgrade(&shared));
        shard.sweep_if_full();
        shared
    }

    fn shard(&self, hash: u64) -> MutexGuard<'_, Shard<T>> {
        let shard = &self.shards[(hash as usize) % SHARDS];
        shard.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<T> Shard<T> {
    /// Forget values nothing holds, once the shard has doubled in size, so
    /// sweeping costs a constant amount per value shared.
    fn sweep_if_full(&mut self) {
        if self.values.len() < self.sweep_at {
            return;
        }
        self.values.retain(|_, candidates| {
            candidates.retain(|candidate| candidate.strong_count() > 0);
            !candidates.is_empty()
        });
        self.sweep_at = (self.values.len() * 2).max(64);
    }
}

fn hash_of<T: Hash>(value: &T) -> u64 {
    let mut hasher = FxHasher::default();
    value.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_values_are_shared() {
        let sharing = Sharing::default();
        let a = sharing.share(vec![1, 2, 3]);
        let b = sharing.share(vec![1, 2, 3]);
        let c = sharing.share(vec![4]);
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
    }

    #[test]
    fn dropped_values_are_forgotten() {
        let sharing = Sharing::default();
        for i in 0..1000u32 {
            drop(sharing.share(i));
        }
        let held: usize = sharing.shards.iter().map(|shard| shard.lock().unwrap().values.len()).sum();
        assert!(held < 1000, "{held}");
    }
}
