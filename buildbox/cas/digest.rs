//! Content digests: the SHA-256 hash and size of a blob.

use data_encoding::HEXLOWER;
use remote_execution_proto::build::bazel::remote::execution::v2 as reapi;
use ring::digest::{Context, SHA256};
use std::fmt;

const HASH_HEX_LEN: usize = 64;

/// Identifies a blob by the SHA-256 hash of its contents and its size.
///
/// A `Digest` is always valid: the hash is 64 lowercase hex characters and
/// the size fits in the `int64` the protocol uses. This makes it safe to use
/// the hash as a file name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    hash: String,
    size_bytes: u64,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum DigestError {
    #[error("invalid hash {0:?}: expected {HASH_HEX_LEN} lowercase hex characters")]
    InvalidHash(String),

    #[error("invalid size {0}: must be between 0 and 2^63-1")]
    InvalidSize(i128),

    #[error("digest is missing")]
    Missing,
}

impl Digest {
    /// Create a digest from a hex-encoded SHA-256 hash and a size.
    pub fn new(hash: &str, size_bytes: u64) -> Result<Digest, DigestError> {
        if !is_valid_hash(hash) {
            return Err(DigestError::InvalidHash(hash.to_string()));
        }
        if i64::try_from(size_bytes).is_err() {
            return Err(DigestError::InvalidSize(size_bytes.into()));
        }

        Ok(Digest {
            hash: hash.to_string(),
            size_bytes,
        })
    }

    /// Compute the digest of some data.
    pub fn of_bytes(data: &[u8]) -> Digest {
        let mut hasher = Hasher::new();
        hasher.update(data);
        hasher.finish()
    }

    /// The digest of the empty blob.
    pub fn empty() -> Digest {
        Digest::of_bytes(&[])
    }

    /// The lowercase hex-encoded SHA-256 hash.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    pub fn from_proto(proto: &reapi::Digest) -> Result<Digest, DigestError> {
        let Ok(size_bytes) = u64::try_from(proto.size_bytes) else {
            return Err(DigestError::InvalidSize(proto.size_bytes.into()));
        };
        Digest::new(&proto.hash, size_bytes)
    }

    /// Convert an optional proto field, which the protocol requires to be set.
    pub fn from_required_proto(proto: Option<&reapi::Digest>) -> Result<Digest, DigestError> {
        let Some(proto) = proto else {
            return Err(DigestError::Missing);
        };
        Digest::from_proto(proto)
    }

    pub fn to_proto(&self) -> reapi::Digest {
        reapi::Digest {
            hash: self.hash.clone(),
            // Can't truncate: `new` guarantees the size fits in an i64.
            size_bytes: self.size_bytes as i64,
        }
    }
}

impl fmt::Display for Digest {
    /// Formats as `{hash}/{size}`, the form used in ByteStream resource names.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.hash, self.size_bytes)
    }
}

fn is_valid_hash(hash: &str) -> bool {
    if hash.len() != HASH_HEX_LEN {
        return false;
    }
    hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Computes a [`Digest`] incrementally, for data that arrives in chunks.
pub struct Hasher {
    context: Context,
    size_bytes: u64,
}

impl Hasher {
    pub fn new() -> Hasher {
        Hasher {
            context: Context::new(&SHA256),
            size_bytes: 0,
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.context.update(data);
        self.size_bytes += data.len() as u64;
    }

    pub fn finish(self) -> Digest {
        let hash = HEXLOWER.encode(self.context.finish().as_ref());
        Digest {
            hash,
            size_bytes: self.size_bytes,
        }
    }
}

impl Default for Hasher {
    fn default() -> Self {
        Hasher::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const HELLO_HASH: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn of_bytes_computes_sha256_and_size() {
        assert_eq!(Digest::empty(), Digest::new(EMPTY_HASH, 0).unwrap());
        assert_eq!(Digest::of_bytes(b"hello"), Digest::new(HELLO_HASH, 5).unwrap());
    }

    #[test]
    fn hasher_matches_of_bytes_across_chunks() {
        let mut hasher = Hasher::new();
        hasher.update(b"he");
        hasher.update(b"");
        hasher.update(b"llo");
        assert_eq!(hasher.finish(), Digest::of_bytes(b"hello"));
    }

    #[test]
    fn new_rejects_invalid_hashes() {
        let uppercase = HELLO_HASH.to_uppercase();
        let too_short = &HELLO_HASH[1..];
        let path_traversal = format!("../{}", &HELLO_HASH[3..]);

        for hash in [uppercase.as_str(), too_short, path_traversal.as_str(), ""] {
            let err = Digest::new(hash, 5).unwrap_err();
            assert_eq!(err, DigestError::InvalidHash(hash.to_string()));
        }
    }

    #[test]
    fn new_rejects_sizes_that_overflow_int64() {
        let err = Digest::new(HELLO_HASH, u64::MAX).unwrap_err();
        assert_eq!(err, DigestError::InvalidSize(u64::MAX.into()));
    }

    #[test]
    fn from_proto_rejects_negative_sizes() {
        let proto = reapi::Digest {
            hash: HELLO_HASH.to_string(),
            size_bytes: -1,
        };
        assert_eq!(Digest::from_proto(&proto), Err(DigestError::InvalidSize(-1)));
    }

    #[test]
    fn from_required_proto_rejects_missing() {
        assert_eq!(Digest::from_required_proto(None), Err(DigestError::Missing));
    }

    #[test]
    fn proto_round_trip() {
        let digest = Digest::of_bytes(b"hello");
        assert_eq!(Digest::from_proto(&digest.to_proto()), Ok(digest));
    }

    #[test]
    fn displays_as_hash_and_size() {
        assert_eq!(Digest::of_bytes(b"hello").to_string(), format!("{HELLO_HASH}/5"));
    }
}
