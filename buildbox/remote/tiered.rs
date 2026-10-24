//! Stores that keep blobs and results both here and on a remote service,
//! for building locally with a remote cache.

use actioncache::{ActionCache, ActionCacheError, Usage as CacheUsage};
use async_trait::async_trait;
use cas::{BlobReader, BlobWriter, Cas, CasError, Digest, Usage};
use remote_execution_rust_connect::build::bazel::remote::execution::v2::ActionResult;
use std::path::Path;
use std::sync::Arc;

/// A local CAS in front of a remote one. Blobs are written to both; reads
/// are served locally, fetching from the remote CAS what isn't here.
pub struct TieredCas {
    local: Arc<dyn Cas>,
    remote: Arc<dyn Cas>,
}

impl TieredCas {
    pub fn new(local: Arc<dyn Cas>, remote: Arc<dyn Cas>) -> TieredCas {
        TieredCas { local, remote }
    }

    /// Copy a blob from the remote CAS if it isn't here.
    async fn fetch(&self, digest: &Digest) -> Result<(), CasError> {
        if self.local.find_missing(std::slice::from_ref(digest)).await?.is_empty() {
            return Ok(());
        }
        let data = self.remote.read_bytes(digest).await?;
        self.local.put_bytes(&data).await?;
        Ok(())
    }
}

#[async_trait]
impl Cas for TieredCas {
    /// The blobs missing from either store, so that uploading them puts
    /// them in both.
    async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
        let mut missing = self.local.find_missing(digests).await?;
        for digest in self.remote.find_missing(digests).await? {
            if !missing.contains(&digest) {
                missing.push(digest);
            }
        }
        Ok(missing)
    }

    async fn read(&self, digest: &Digest) -> Result<BlobReader, CasError> {
        self.fetch(digest).await?;
        self.local.read(digest).await
    }

    async fn write(&self, expected: &Digest) -> Result<Box<dyn BlobWriter>, CasError> {
        Ok(Box::new(TieredWriter {
            local: self.local.clone(),
            remote: self.remote.clone(),
            expected: expected.clone(),
            data: Vec::new(),
        }))
    }

    async fn put_file(&self, path: &Path) -> Result<Digest, CasError> {
        let digest = self.local.put_file(path).await?;
        self.remote.put_file(path).await?;
        Ok(digest)
    }

    async fn materialize(&self, digest: &Digest, dest: &Path, executable: bool) -> Result<(), CasError> {
        self.fetch(digest).await?;
        self.local.materialize(digest, dest, executable).await
    }

    async fn usage(&self) -> Result<Usage, CasError> {
        self.local.usage().await
    }
}

/// Collects a blob's data, and writes it to both stores on commit.
struct TieredWriter {
    local: Arc<dyn Cas>,
    remote: Arc<dyn Cas>,
    expected: Digest,
    data: Vec<u8>,
}

#[async_trait]
impl BlobWriter for TieredWriter {
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
        self.local.put_bytes(&self.data).await?;
        self.remote.put_bytes(&self.data).await?;
        Ok(())
    }
}

/// A local action cache in front of a remote one. Results are stored in
/// both, and looked up here first.
pub struct TieredActionCache {
    local: Arc<dyn ActionCache>,
    remote: Arc<dyn ActionCache>,
}

impl TieredActionCache {
    pub fn new(local: Arc<dyn ActionCache>, remote: Arc<dyn ActionCache>) -> TieredActionCache {
        TieredActionCache { local, remote }
    }
}

#[async_trait]
impl ActionCache for TieredActionCache {
    async fn get(&self, action: &Digest) -> Result<Option<ActionResult>, ActionCacheError> {
        if let Some(result) = self.local.get(action).await? {
            return Ok(Some(result));
        }
        self.remote.get(action).await
    }

    async fn put(&self, action: &Digest, result: &ActionResult) -> Result<(), ActionCacheError> {
        self.local.put(action, result).await?;
        self.remote.put(action, result).await
    }

    async fn usage(&self) -> Result<CacheUsage, ActionCacheError> {
        self.local.usage().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cas_mem::MemCas;

    #[tokio::test]
    async fn writes_to_both_and_reads_through() {
        let local = Arc::new(MemCas::new());
        let remote = Arc::new(MemCas::new());
        let tiered = TieredCas::new(local.clone(), remote.clone());

        let written = tiered.put_bytes(b"both").await.unwrap();
        assert!(local.find_missing(std::slice::from_ref(&written)).await.unwrap().is_empty());
        assert!(remote.find_missing(std::slice::from_ref(&written)).await.unwrap().is_empty());

        let remote_only = remote.put_bytes(b"remote only").await.unwrap();
        assert_eq!(tiered.find_missing(std::slice::from_ref(&remote_only)).await.unwrap(), vec![remote_only.clone()]);
        assert_eq!(tiered.read_bytes(&remote_only).await.unwrap(), b"remote only");
        assert!(local.find_missing(std::slice::from_ref(&remote_only)).await.unwrap().is_empty());
    }
}
