//! A [`Cas`] that keeps blobs in memory.

use async_trait::async_trait;
use bytes::Bytes;
use cas::{BlobReader, BlobWriter, Cas, CasError, Digest, Usage};
use std::collections::HashMap;
use std::fs::Permissions;
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

const FILE_MODE: u32 = 0o644;
const EXECUTABLE_MODE: u32 = 0o755;

type Blobs = HashMap<Digest, Bytes>;

/// Keeps blobs in memory, for tests and short-lived servers.
#[derive(Debug, Clone)]
pub struct MemCas {
    blobs: Arc<Mutex<Blobs>>,
}

impl MemCas {
    pub fn new() -> MemCas {
        let mut blobs = HashMap::new();
        // Clients may reference the empty blob without uploading it.
        blobs.insert(Digest::empty(), Bytes::new());
        MemCas {
            blobs: Arc::new(Mutex::new(blobs)),
        }
    }

    fn get(&self, digest: &Digest) -> Result<Bytes, CasError> {
        match lock(&self.blobs).get(digest) {
            Some(data) => Ok(data.clone()),
            None => Err(CasError::NotFound(digest.clone())),
        }
    }
}

impl Default for MemCas {
    fn default() -> Self {
        MemCas::new()
    }
}

#[async_trait]
impl Cas for MemCas {
    async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
        let blobs = lock(&self.blobs);
        let mut missing = vec![];
        for digest in digests {
            if !blobs.contains_key(digest) {
                missing.push(digest.clone());
            }
        }
        Ok(missing)
    }

    async fn read(&self, digest: &Digest) -> Result<BlobReader, CasError> {
        let data = self.get(digest)?;
        Ok(Box::pin(Cursor::new(data)))
    }

    async fn write(&self, expected: &Digest) -> Result<Box<dyn BlobWriter>, CasError> {
        let writer = MemBlobWriter {
            blobs: self.blobs.clone(),
            expected: expected.clone(),
            data: vec![],
        };
        Ok(Box::new(writer))
    }

    async fn put_file(&self, path: &Path) -> Result<Digest, CasError> {
        let data = tokio::fs::read(path)
            .await
            .map_err(|error| CasError::io(format!("failed to read {}", path.display()), error))?;
        self.put_bytes(&data).await
    }

    async fn materialize(&self, digest: &Digest, dest: &Path, executable: bool) -> Result<(), CasError> {
        let data = self.get(digest)?;
        tokio::fs::write(dest, &data)
            .await
            .map_err(|error| CasError::io(format!("failed to write {}", dest.display()), error))?;

        let mode = if executable { EXECUTABLE_MODE } else { FILE_MODE };
        tokio::fs::set_permissions(dest, Permissions::from_mode(mode))
            .await
            .map_err(|error| CasError::io(format!("failed to set permissions on {}", dest.display()), error))
    }

    async fn usage(&self) -> Result<Usage, CasError> {
        let blobs = lock(&self.blobs);
        let mut usage = Usage::default();
        for data in blobs.values() {
            usage.blobs += 1;
            usage.bytes += data.len() as u64;
        }
        Ok(usage)
    }
}

struct MemBlobWriter {
    blobs: Arc<Mutex<Blobs>>,
    expected: Digest,
    data: Vec<u8>,
}

#[async_trait]
impl BlobWriter for MemBlobWriter {
    async fn write(&mut self, data: &[u8]) -> Result<(), CasError> {
        self.data.extend_from_slice(data);
        Ok(())
    }

    async fn commit(self: Box<Self>) -> Result<(), CasError> {
        let actual = Digest::of_bytes(&self.data);
        if actual != self.expected {
            return Err(CasError::DigestMismatch {
                expected: self.expected,
                actual,
            });
        }

        lock(&self.blobs).insert(actual, Bytes::from(self.data));
        Ok(())
    }
}

/// The map is never left inconsistent, so a panic while it was held doesn't
/// make it unusable.
fn lock(blobs: &Mutex<Blobs>) -> MutexGuard<'_, Blobs> {
    blobs.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn conformance() {
        cas_conformance::run_all(|_dir| async move { Box::new(MemCas::new()) as Box<dyn Cas> }).await;
    }
}
