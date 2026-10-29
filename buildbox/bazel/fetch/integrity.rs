//! Checksums that downloads are verified against.

use data_encoding::{BASE64, HEXLOWER, HEXLOWER_PERMISSIVE};
use ring::digest::{Algorithm, Context, SHA256, SHA384, SHA512};
use std::fmt;

/// An expected checksum: a hash algorithm and the hash. Written either as
/// a hex SHA-256 (Bazel's `sha256` attribute) or as Subresource Integrity,
/// `sha256-<base64>` (its `integrity` attribute).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Integrity {
    algorithm: HashAlgorithm,
    hash: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HashAlgorithm {
    Sha256,
    Sha384,
    Sha512,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid checksum '{0}'")]
pub struct IntegrityError(pub String);

impl HashAlgorithm {
    fn ring(self) -> &'static Algorithm {
        match self {
            HashAlgorithm::Sha256 => &SHA256,
            HashAlgorithm::Sha384 => &SHA384,
            HashAlgorithm::Sha512 => &SHA512,
        }
    }

    fn name(self) -> &'static str {
        match self {
            HashAlgorithm::Sha256 => "sha256",
            HashAlgorithm::Sha384 => "sha384",
            HashAlgorithm::Sha512 => "sha512",
        }
    }
}

/// The SHA-256 of `bytes`, in lowercase hex, as Bazel records registry
/// files' hashes.
pub fn sha256_hex_of(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&SHA256, bytes);
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

impl Integrity {
    /// A hex-encoded SHA-256, as `sha256 = "..."` gives.
    pub fn sha256_hex(hex: &str) -> Result<Integrity, IntegrityError> {
        let hash = HEXLOWER_PERMISSIVE
            .decode(hex.trim().as_bytes())
            .map_err(|_| IntegrityError(hex.to_string()))?;
        if hash.len() != 32 {
            return Err(IntegrityError(hex.to_string()));
        }
        Ok(Integrity {
            algorithm: HashAlgorithm::Sha256,
            hash,
        })
    }

    /// Subresource Integrity, as `integrity = "sha256-..."` gives.
    pub fn sri(text: &str) -> Result<Integrity, IntegrityError> {
        let (algorithm, encoded) = text.trim().split_once('-').ok_or_else(|| IntegrityError(text.to_string()))?;
        let algorithm = match algorithm {
            "sha256" => HashAlgorithm::Sha256,
            "sha384" => HashAlgorithm::Sha384,
            "sha512" => HashAlgorithm::Sha512,
            _ => return Err(IntegrityError(text.to_string())),
        };
        let hash = BASE64.decode(encoded.as_bytes()).map_err(|_| IntegrityError(text.to_string()))?;
        Ok(Integrity { algorithm, hash })
    }

    pub fn algorithm(&self) -> HashAlgorithm {
        self.algorithm
    }

    /// The SHA-256 in hex, if that's the algorithm, for addressing the
    /// download cache.
    pub fn sha256(&self) -> Option<String> {
        (self.algorithm == HashAlgorithm::Sha256).then(|| HEXLOWER.encode(&self.hash))
    }

    /// The checksum as Subresource Integrity.
    pub fn to_sri(&self) -> String {
        format!("{}-{}", self.algorithm.name(), BASE64.encode(&self.hash))
    }

    pub fn hasher(&self) -> Hasher {
        Hasher::new(self.algorithm)
    }
}

impl fmt::Debug for Integrity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_sri())
    }
}

impl fmt::Display for Integrity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_sri())
    }
}

/// Hashes data as it's written, to check it against an [`Integrity`].
pub struct Hasher {
    algorithm: HashAlgorithm,
    context: Context,
}

impl Hasher {
    pub fn new(algorithm: HashAlgorithm) -> Hasher {
        Hasher {
            algorithm,
            context: Context::new(algorithm.ring()),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.context.update(data);
    }

    pub fn finish(self) -> Integrity {
        Integrity {
            algorithm: self.algorithm,
            hash: self.context.finish().as_ref().to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn hex_and_sri_forms_agree() {
        let hex = Integrity::sha256_hex(HELLO_SHA256).unwrap();
        let sri = Integrity::sri(&hex.to_sri()).unwrap();
        assert_eq!(hex, sri);
        assert_eq!(sri.sha256().as_deref(), Some(HELLO_SHA256));

        let mut hasher = Hasher::new(HashAlgorithm::Sha256);
        hasher.update(b"hello");
        assert_eq!(hasher.finish(), hex);
    }

    #[test]
    fn invalid_checksums_are_rejected() {
        assert!(Integrity::sha256_hex("abc").is_err());
        assert!(Integrity::sri("md5-abc").is_err());
        assert!(Integrity::sri("sha256").is_err());
    }
}
