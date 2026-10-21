use bytes::BytesMut;
use common::hash::Hasher;
use proto::bazel::exec::Digest;
use common::{Error, Result};
use prost::Message;
use std::default::Default;
use std::io::{BufReader, Read, Write};

use crate::{Store, WriteHandle};
use crate::tee::TeeWriter;

/// Convenience extensions for using the store as content-addressable storage
/// for serialized proto messages.
pub trait ProtoStoreExt {
    /// Read data identified by a [`Digest`].
    fn read_digest(&self, digest: &Digest) -> Result<impl Read>;

    /// Read a proto message identified by a [`Digest`].
    fn read_message<T>(&self, digest: &Digest) -> Result<T>
    where
        T: Message + Default;

    /// Write bytes to a file identified by a [`Digest`] hash.
    fn write_digest(&self, src: impl Read) -> Result<Digest>;
}

impl<S: Store> ProtoStoreExt for S {
    fn read_digest(&self, digest: &Digest) -> Result<impl Read> {
        self.read(&digest.hash)
    }

    fn read_message<T>(&self, digest: &Digest) -> Result<T>
    where
        T: Message + Default,
    {
        let mut buf = vec![];
        let mut reader = self.read_digest(digest)?;
        reader.read_to_end(&mut buf).map_err(Error::io)?;

        let data = BytesMut::from(buf.as_slice());
        T::decode(data).map_err(Error::boxed)
    }

    fn write_digest(&self, src: impl Read) -> Result<Digest> {
        let mut writer = self.write()?;
        let mut hasher = Hasher::sha256();
        let mut tee = TeeWriter::new(&mut writer, &mut hasher);

        let mut reader = BufReader::new(src);
        let size_bytes = std::io::copy(&mut reader, &mut tee).map_err(Error::io)?;
        tee.flush().map_err(Error::io)?;

        let hash = hasher.finish().to_string();
        writer.seal(&hash)?;

        Ok(Digest { hash, size_bytes: size_bytes as i64 })
    }
}