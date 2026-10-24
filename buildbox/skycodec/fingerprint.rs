//! Fingerprints: short hashes that identify keys, values and traces.

use ring::digest::{Context, SHA256};
use std::fmt;

/// The first 128 bits of a SHA-256 hash.
///
/// That's enough to make accidental collisions practically impossible among
/// the values one machine's builds produce, at half the size of a full
/// digest. Full digests are still used where the remote execution protocol
/// asks for them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Fingerprint([u8; Fingerprint::LEN]);

impl Fingerprint {
    pub const LEN: usize = 16;

    pub fn of(bytes: &[u8]) -> Fingerprint {
        let mut fingerprinter = Fingerprinter::new();
        fingerprinter.bytes(bytes);
        fingerprinter.finish()
    }

    pub fn from_bytes(bytes: [u8; Fingerprint::LEN]) -> Fingerprint {
        Fingerprint(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; Fingerprint::LEN] {
        &self.0
    }

    /// The first eight bytes, for use as a hash table hash.
    pub fn short(&self) -> u64 {
        u64::from_le_bytes(self.0[..8].try_into().expect("fingerprints are 16 bytes"))
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

/// Computes a [`Fingerprint`] of a sequence of parts. Each part is written
/// with its length, so different ways of splitting the same bytes give
/// different fingerprints.
pub struct Fingerprinter {
    context: Context,
}

impl Fingerprinter {
    pub fn new() -> Fingerprinter {
        Fingerprinter {
            context: Context::new(&SHA256),
        }
    }

    pub fn bytes(&mut self, bytes: &[u8]) -> &mut Fingerprinter {
        self.context.update(&(bytes.len() as u64).to_le_bytes());
        self.context.update(bytes);
        self
    }

    pub fn u64(&mut self, value: u64) -> &mut Fingerprinter {
        self.context.update(&value.to_le_bytes());
        self
    }

    pub fn fingerprint(&mut self, fingerprint: &Fingerprint) -> &mut Fingerprinter {
        self.context.update(&fingerprint.0);
        self
    }

    pub fn finish(self) -> Fingerprint {
        let digest = self.context.finish();
        let bytes = digest.as_ref()[..Fingerprint::LEN].try_into().expect("SHA-256 is 32 bytes");
        Fingerprint(bytes)
    }
}

impl Default for Fingerprinter {
    fn default() -> Fingerprinter {
        Fingerprinter::new()
    }
}
