use proto::bazel::exec::Digest;
use common::Result;
use std::io::{Read, Write};

/// A content-addressable data store. Designed for potentially large blobs.
pub trait Store: Clone + Sync + Send {
    /// Type for reading files that have already been created.
    type ReadHandle: ReadHandle + Sync + Send;

    /// Type for files that have just been created.
    type WriteHandle: WriteHandle + Sync + Send;

    /// Read the file.
    fn read(&self, name: &str) -> Result<Self::ReadHandle>;

    /// Create a new file.
    fn write(&self) -> Result<Self::WriteHandle>;

    /// Check that the storage contains this digest.
    fn contains(&self, name: &str) -> Result<bool>;
}

pub trait WriteHandle: Write {
    /// Finish writing the file and store it under this name. Will fail if a
    /// file already exists with this name.
    fn seal(self, name: &str) -> Result<()>;
}

pub trait ReadHandle: Read {
    /// Metadata about the file.
    fn metadata(&self);
}