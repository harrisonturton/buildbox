use common::Result;
use std::io::Read;
use prost::Message;
use proto::bazel::exec::Digest;
use std::default::Default;

pub trait Store: Clone + Sync + Send {
    /// Check that the storage contains this digest.
    fn contains(&self, digest: &Digest) -> Result<bool>;

    /// Read the file by name.
    fn read(&self, name: &str) -> Result<impl Read + 'static>;

    /// Read the file identified by the digest.
    fn read_digest(&self, digest: &Digest) -> Result<impl Read + 'static>;

    /// Read the complete content of the file identified by the digest.
    fn read_digest_to_end(&self, digest: &Digest) -> Result<Vec<u8>>;

    /// Read the complete content of the file identified by the digest and attempt
    /// to convert it to the corresponding proto message.
    fn read_message<T>(&self, digest: &Digest) -> Result<T>
    where
        T: Message + Default;

    /// Write content to the storage and return the digest.
    fn write(&self, src: impl Read) -> Result<Digest>;

    /// Write to a specific filename.
    fn write_with_name(&self, name: &str, src: impl Read) -> Result<()>;

    fn list(&self) -> Result<Vec<String>>;
}