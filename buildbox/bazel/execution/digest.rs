//! Content digests, as the graph keeps them.

use data_encoding::HEXLOWER;
use skycodec::{Decode, DecodeError, Decoder, Encode, Encoder};
use std::fmt;

/// A blob's SHA-256 hash and size: a [`cas::Digest`] in 40 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlobDigest {
    pub hash: [u8; 32],
    pub size: u64,
}

impl BlobDigest {
    pub fn from_cas(digest: &cas::Digest) -> BlobDigest {
        let mut hash = [0u8; 32];
        // A `cas::Digest`'s hash is always 64 hex characters.
        if let Ok(bytes) = HEXLOWER.decode(digest.hash().as_bytes()) {
            hash.copy_from_slice(&bytes[..32.min(bytes.len())]);
        }
        BlobDigest {
            hash,
            size: digest.size_bytes(),
        }
    }

    pub fn to_cas(&self) -> cas::Digest {
        cas::Digest::new(&HEXLOWER.encode(&self.hash), self.size).unwrap_or_else(|_| cas::Digest::empty())
    }
}

impl fmt::Debug for BlobDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", HEXLOWER.encode(&self.hash), self.size)
    }
}

impl Encode for BlobDigest {
    fn encode(&self, encoder: &mut Encoder) {
        encoder.raw(&self.hash);
        encoder.u64(self.size);
    }
}

impl Decode for BlobDigest {
    fn decode(decoder: &mut Decoder<'_>) -> Result<BlobDigest, DecodeError> {
        let mut hash = [0u8; 32];
        hash.copy_from_slice(decoder.raw(32)?);
        let size = decoder.u64()?;
        Ok(BlobDigest { hash, size })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_round_trip() {
        let digest = cas::Digest::of_bytes(b"hello");
        let stored = BlobDigest::from_cas(&digest);
        assert_eq!(stored.to_cas(), digest);
        let bytes = skycodec::to_bytes(&stored);
        assert_eq!(bytes.len(), 33);
        assert_eq!(skycodec::from_bytes::<BlobDigest>(&bytes).unwrap(), stored);
    }
}
