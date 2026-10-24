//! An append-only table of 64-bit values that can be read without locking.

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::Mutex;

/// The first bucket holds `1 << FIRST_BUCKET_BITS` entries, and each bucket
/// after it twice as many as the one before.
const FIRST_BUCKET_BITS: u32 = 10;

/// Enough buckets for every `u32` index.
const BUCKETS: usize = (u32::BITS - FIRST_BUCKET_BITS + 1) as usize;

/// A table indexed by `u32`, made of buckets that double in size so that
/// existing entries never move. Entries are written once, before their index
/// is shared, and can then be read concurrently without a lock.
pub(crate) struct SegVec {
    buckets: [AtomicPtr<AtomicU64>; BUCKETS],
    grow: Mutex<()>,
}

impl SegVec {
    pub(crate) fn new() -> SegVec {
        SegVec {
            buckets: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())),
            grow: Mutex::new(()),
        }
    }

    /// Store the entry at `index`. Must be called before `index` is given to
    /// anything that reads it.
    pub(crate) fn set(&self, index: u32, value: u64) {
        let (bucket, offset) = locate(index);
        let entries = self.bucket(bucket);
        // SAFETY: `bucket` returns an allocation of `bucket_len(bucket)`
        // entries, and `locate` keeps `offset` within it.
        unsafe { (*entries.add(offset)).store(value, Ordering::Release) };
    }

    /// The entry at `index`, which must have been set.
    pub(crate) fn get(&self, index: u32) -> u64 {
        let (bucket, offset) = locate(index);
        let entries = self.buckets[bucket].load(Ordering::Acquire);
        assert!(!entries.is_null(), "read of unset entry {index}");
        // SAFETY: as in `set`.
        unsafe { (*entries.add(offset)).load(Ordering::Acquire) }
    }

    /// The number of bytes the allocated buckets take.
    pub(crate) fn allocated_bytes(&self) -> usize {
        let mut bytes = 0;
        for (bucket, entries) in self.buckets.iter().enumerate() {
            if !entries.load(Ordering::Acquire).is_null() {
                bytes += bucket_len(bucket) * std::mem::size_of::<AtomicU64>();
            }
        }
        bytes
    }

    fn bucket(&self, bucket: usize) -> *mut AtomicU64 {
        let entries = self.buckets[bucket].load(Ordering::Acquire);
        if !entries.is_null() {
            return entries;
        }

        let _guard = self.grow.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let entries = self.buckets[bucket].load(Ordering::Acquire);
        if !entries.is_null() {
            return entries;
        }
        let allocation: Box<[AtomicU64]> = (0..bucket_len(bucket)).map(|_| AtomicU64::new(0)).collect();
        let entries = Box::leak(allocation).as_mut_ptr();
        self.buckets[bucket].store(entries, Ordering::Release);
        entries
    }
}

fn bucket_len(bucket: usize) -> usize {
    1 << (FIRST_BUCKET_BITS as usize + bucket)
}

/// The bucket that holds `index`, and its offset within that bucket.
fn locate(index: u32) -> (usize, usize) {
    let n = index as u64 + (1 << FIRST_BUCKET_BITS);
    let bucket = (u64::BITS - 1 - n.leading_zeros() - FIRST_BUCKET_BITS) as usize;
    let offset = (n - (1 << (FIRST_BUCKET_BITS as usize + bucket))) as usize;
    (bucket, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locate_covers_every_index_once() {
        assert_eq!(locate(0), (0, 0));
        assert_eq!(locate(1023), (0, 1023));
        assert_eq!(locate(1024), (1, 0));
        assert_eq!(locate(3071), (1, 2047));
        assert_eq!(locate(3072), (2, 0));
        let (bucket, offset) = locate(u32::MAX);
        assert!(bucket < BUCKETS);
        assert!(offset < bucket_len(bucket));
    }

    #[test]
    fn set_then_get() {
        let table = SegVec::new();
        for index in [0, 1, 1023, 1024, 5000, 100_000] {
            table.set(index, index as u64 * 3);
        }
        for index in [0, 1, 1023, 1024, 5000, 100_000] {
            assert_eq!(table.get(index), index as u64 * 3);
        }
    }
}
