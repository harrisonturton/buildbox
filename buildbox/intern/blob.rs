//! Interned byte strings.

use crate::arena::{Arena, Text};
use crate::table::{Hashing, Table};
use std::hash::BuildHasher;

/// Interns byte strings, such as encoded build graph keys, giving each
/// distinct one a dense `u32` id.
///
/// Unlike [`Symbol`](crate::Symbol)s, which share one table for the whole
/// process, each `BlobTable` has ids of its own. Its storage is never freed,
/// even when the table is dropped, so tables should live as long as the
/// process.
pub struct BlobTable {
    arena: Arena,
    table: Table,
}

struct BlobHashing;

impl Hashing for BlobHashing {
    fn hash(&self, value: u64) -> u64 {
        hash_bytes(blob(value).as_bytes())
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    rustc_hash::FxBuildHasher.hash_one(bytes)
}

fn blob(value: u64) -> Text {
    // SAFETY: every value in a blob table came from `Text::as_ptr`.
    unsafe { Text::from_ptr(value as *const u8) }
}

impl BlobTable {
    pub fn new() -> BlobTable {
        BlobTable {
            arena: Arena::new(),
            table: Table::new(),
        }
    }

    /// The id of `bytes`, interning them if needed.
    pub fn intern(&self, bytes: &[u8]) -> u32 {
        let hash = hash_bytes(bytes);
        let is_match = |value| blob(value).as_bytes() == bytes;
        let make = || self.arena.alloc(bytes).as_ptr() as u64;
        self.table.intern(&BlobHashing, hash, is_match, make)
    }

    /// The id of `bytes`, if they have been interned.
    pub fn get(&self, bytes: &[u8]) -> Option<u32> {
        self.table.find(hash_bytes(bytes), |value| blob(value).as_bytes() == bytes)
    }

    /// The bytes with the given id, which must have come from this table.
    pub fn bytes(&self, id: u32) -> &[u8] {
        assert!(id < self.table.len(), "blob id {id} is not from this table");
        blob(self.table.value(id)).as_bytes()
    }

    /// How many byte strings have been interned.
    pub fn len(&self) -> u32 {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes taken by the interned data and the index.
    pub fn allocated_bytes(&self) -> usize {
        self.arena.allocated_bytes() + self.table.allocated_bytes()
    }
}

impl Default for BlobTable {
    fn default() -> BlobTable {
        BlobTable::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_assigns_dense_ids() {
        let table = BlobTable::new();
        assert_eq!(table.intern(b"a"), 0);
        assert_eq!(table.intern(b"b"), 1);
        assert_eq!(table.intern(b"a"), 0);
        assert_eq!(table.bytes(1), b"b");
        assert_eq!(table.get(b"c"), None);
        assert_eq!(table.len(), 2);
    }
}
