//! Content-addressable storage: blobs stored and looked up by their digest.

mod digest;
pub use digest::{Digest, DigestError, Hasher};

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Reads the contents of a stored blob.
pub type BlobReader = Pin<Box<dyn AsyncRead + Send>>;

/// The initial capacity to reserve when reading a whole blob into memory, so a
/// large digest doesn't cause a large allocation before any data is read.
const MAX_INITIAL_READ_CAPACITY: u64 = 1 << 20;

/// A store of immutable blobs, each identified by the [`Digest`] of its
/// contents.
///
/// Implementations must treat the empty blob as always present, since clients
/// may reference it without uploading it.
#[async_trait]
pub trait Cas: Send + Sync + 'static {
    /// Return the digests that aren't stored, in the order given.
    async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError>;

    /// Open a stored blob for reading.
    async fn read(&self, digest: &Digest) -> Result<BlobReader, CasError>;

    /// Start writing a blob that is expected to have the given digest.
    ///
    /// The blob becomes visible only when the writer is committed, which fails
    /// if the written data doesn't match the digest. Dropping the writer
    /// without committing discards the data.
    async fn write(&self, expected: &Digest) -> Result<Box<dyn BlobWriter>, CasError>;

    /// Store the contents of a local file, returning its digest.
    async fn put_file(&self, path: &Path) -> Result<Digest, CasError>;

    /// Store the contents of a local file that nothing else will use, such
    /// as an action's output, returning its digest. Stores may take the
    /// file itself rather than copy it, so it must not be used afterwards,
    /// and must not be written to by anything still running. `executable`
    /// says whether it is mostly materialized as an executable.
    async fn adopt_file(&self, path: &Path, executable: bool) -> Result<Digest, CasError> {
        let _ = executable;
        self.put_file(path).await
    }

    /// Create a file at `dest` with the contents of a stored blob.
    ///
    /// The file must not be modified afterwards, so implementations are free
    /// to share storage with the blob (for example, with a hard link).
    async fn materialize(&self, digest: &Digest, dest: &Path, executable: bool) -> Result<(), CasError>;

    /// Create many files, as [`Cas::materialize`] does. Stores that can
    /// create them more cheaply together than one at a time override this.
    async fn materialize_all(&self, files: Vec<Materialization>) -> Result<(), CasError> {
        for file in files {
            self.materialize(&file.digest, &file.dest, file.executable).await?;
        }
        Ok(())
    }

    /// Report how much data is stored.
    async fn usage(&self) -> Result<Usage, CasError>;

    /// Store some data, returning its digest.
    async fn put_bytes(&self, data: &[u8]) -> Result<Digest, CasError> {
        let digest = Digest::of_bytes(data);
        let mut writer = self.write(&digest).await?;
        writer.write(data).await?;
        writer.commit().await?;
        Ok(digest)
    }

    /// Read a whole blob into memory.
    async fn read_bytes(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let mut reader = self.read(digest).await?;
        let capacity = digest.size_bytes().min(MAX_INITIAL_READ_CAPACITY);
        let mut data = Vec::with_capacity(capacity as usize);
        reader
            .read_to_end(&mut data)
            .await
            .map_err(|error| CasError::io(format!("failed to read blob {digest}"), error))?;
        Ok(data)
    }
}

/// A file to create with the contents of a stored blob. See
/// [`Cas::materialize_all`].
#[derive(Clone, Debug)]
pub struct Materialization {
    pub digest: Digest,
    pub dest: PathBuf,
    pub executable: bool,
}

/// Writes a blob to a [`Cas`]. See [`Cas::write`].
#[async_trait]
pub trait BlobWriter: Send {
    /// Append data to the blob.
    async fn write(&mut self, data: &[u8]) -> Result<(), CasError>;

    /// Verify the data against the expected digest and make the blob visible.
    async fn commit(self: Box<Self>) -> Result<(), CasError>;
}

/// How much data a [`Cas`] holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub blobs: u64,
    pub bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum CasError {
    #[error("blob {0} not found")]
    NotFound(Digest),

    #[error("data doesn't match its digest: expected {expected}, got {actual}")]
    DigestMismatch { expected: Digest, actual: Digest },

    #[error("{context}: {error}")]
    Io {
        context: String,
        error: std::io::Error,
    },
}

impl CasError {
    pub fn io(context: impl Into<String>, error: std::io::Error) -> CasError {
        CasError::Io {
            context: context.into(),
            error,
        }
    }
}
