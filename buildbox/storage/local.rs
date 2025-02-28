use bytes::BytesMut;
use common::hash::Hasher;
use common::rand;
use proto::bazel::exec::Digest;
use common::{Error, Result};
use prost::Message;
use std::fs::OpenOptions;
use std::io::{copy, BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::PathBuf;

use crate::tee::TeeWriter;

#[derive(Debug, Clone, PartialEq)]
pub struct Storage {
    dir: PathBuf,
}

impl Storage {
    /// Create a new [`Storage`] instance that uses the local disk.
    pub fn local(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Check that the storage contains this digest.
    pub fn contains(&self, digest: &Digest) -> Result<bool> {
        let path = local_path(&self.dir, &digest.hash);
        match OpenOptions::new().read(true).open(path) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
            Err(err) => Err(Error::io(err)),
        }
    }

    /// Read the file identified by the digest.
    pub fn read(&self, digest: &Digest) -> Result<impl Read> {
        let path = local_path(&self.dir, &digest.hash);
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(Error::io_msg("failed to read digest from storage"))?;

        let metadata = file.metadata().map_err(Error::io)?;
        if metadata.len() != digest.size_bytes as u64 {
            return Err(Error::invalid("file length doesn't match digest"));
        }

        Ok(BufReader::new(file))
    }

    /// Read the complete content of the file identified by the digest.
    pub fn read_to_end(&self, digest: &Digest) -> Result<Vec<u8>> {
        let mut data = vec![];
        let mut reader = self.read(digest)?;
        reader.read_to_end(&mut data).map_err(Error::io)?;
        Ok(data)
    }

    /// Read the complete content of the file identified by the digest and attempt
    /// to convert it to the corresponding proto message.
    pub fn read_message<T>(&self, digest: &Digest) -> Result<T>
    where
        T: Message + Default,
    {
        let data = self.read_to_end(digest)?;
        let data = BytesMut::from(data.as_slice());
        T::decode(data).map_err(Error::boxed)
    }

    /// Write content to the storage and return the digest.
    pub fn write(&self, src: impl Read) -> Result<Digest> {
        let tmp = format!("tmp-{}", rand::string(20));
        let tmp_path = local_path(&self.dir, &tmp);

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(&tmp_path)
            .map_err(Error::io)?;

        // Write to both the file and the hasher at the same time. This avoids
        // needing to accumulate the entire file in memory.
        let mut writer = BufWriter::new(&file);
        let mut hasher = Hasher::sha256();
        let mut tee = TeeWriter::new(&mut writer, &mut hasher);

        let mut reader = BufReader::new(src);
        copy(&mut reader, &mut tee).map_err(Error::io)?;
        tee.flush().map_err(Error::io)?;

        let hash = hasher.finish().base64();
        let metadata = file.metadata().map_err(Error::io)?;
        let size_bytes = metadata.len() as i64;

        let path = local_path(&self.dir, &hash);
        std::fs::rename(tmp_path, path).map_err(Error::io)?;

        Ok(Digest { hash, size_bytes })
    }
}

fn local_path(dir: &PathBuf, name: &str) -> PathBuf {
    let mut path = dir.clone();
    path.push(name);
    path
}
