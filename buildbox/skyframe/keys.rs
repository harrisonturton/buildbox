//! Interned keys.

use crate::key::Key;
use crate::kinds::{self, KindId};
use intern::BlobTable;
use skycodec::Encoder;

/// Identifies an interned key: its kind and encoding. The same key has the
/// same id in every workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyId(pub(crate) u32);

impl KeyId {
    pub fn id(self) -> u32 {
        self.0
    }
}

/// Every key any workspace has used, each stored once as its kind followed
/// by its encoding.
#[derive(Default)]
pub(crate) struct KeyTable {
    blobs: BlobTable,
}

impl KeyTable {
    pub(crate) fn intern<K: Key>(&self, key: &K) -> KeyId {
        KeyId(self.blobs.intern(&encode::<K>(key)))
    }

    /// The id of `key`, if it has been interned.
    pub(crate) fn get<K: Key>(&self, key: &K) -> Option<KeyId> {
        self.blobs.get(&encode::<K>(key)).map(KeyId)
    }

    pub(crate) fn kind(&self, key: KeyId) -> KindId {
        let bytes = self.blobs.bytes(key.0);
        KindId(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// The key's encoding, without its kind.
    pub(crate) fn encoding(&self, key: KeyId) -> &[u8] {
        &self.blobs.bytes(key.0)[2..]
    }

    /// A description of the key for messages, such as `PACKAGE://foo`.
    pub(crate) fn describe(&self, key: KeyId) -> String {
        let kind = self.kind(key);
        match kinds::info(kind) {
            Some(info) => (info.describe)(self.encoding(key)),
            None => format!("<unregistered kind {}>", kind.0),
        }
    }

    /// Every key's kind and encoding, in id order.
    pub(crate) fn blobs(&self) -> Vec<&[u8]> {
        (0..self.blobs.len()).map(|id| self.blobs.bytes(id)).collect()
    }

    /// The table of keys [`KeyTable::blobs`] gave, each keeping its id.
    pub(crate) fn restore<'a>(blobs: impl IntoIterator<Item = &'a [u8]>) -> Option<KeyTable> {
        let table = KeyTable::default();
        for (id, blob) in (0u32..).zip(blobs) {
            if blob.len() < 2 || table.blobs.intern(blob) != id {
                return None;
            }
        }
        Some(table)
    }

    pub(crate) fn len(&self) -> u32 {
        self.blobs.len()
    }

    pub(crate) fn allocated_bytes(&self) -> usize {
        self.blobs.allocated_bytes()
    }
}

fn encode<K: Key>(key: &K) -> Vec<u8> {
    let kind = kinds::kind_of::<K>();
    let mut encoder = Encoder::new();
    encoder.raw(&kind.0.to_le_bytes());
    key.encode(&mut encoder);
    encoder.into_bytes()
}
