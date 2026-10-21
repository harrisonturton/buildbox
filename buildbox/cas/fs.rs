use super::{ReadHandle, Store, WriteHandle};
use crate::tee::TeeWriter;
use bytes::BytesMut;
use common::hash::Hasher;
use common::rand;
use common::{Error, Result};
use prost::Message;
use proto::bazel::exec::Digest;
use std::fs::{File, OpenOptions};
use std::io::{copy, BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct FileStore {
    dir: PathBuf,
}

impl FileStore {
    /// Create a new [`FileStore`] instance that uses the local disk.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn local_path(&self, name: &str) -> PathBuf {
        let mut path = self.dir.clone();
        path.push(name);
        path
    }
}

impl Store for FileStore {
    type ReadHandle = FileReadHandle;

    fn read(&self, name: &str) -> Result<Self::ReadHandle> {
        let path = self.local_path(name);

        let file = OpenOptions::new().read(true).open(path).map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                Error::not_found("file not found")
            } else {
                Error::io(err)
            }
        })?;

        Ok(FileReadHandle { file })
    }

    type WriteHandle = FileWriteHandle;

    fn write(&self) -> Result<Self::WriteHandle> {
        let temp_name = format!("tmp-{}", rand::string(20));
        let path = self.local_path(&temp_name);
        println!("path: {path:?}");

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)
            .map_err(Error::io)?;

        Ok(FileWriteHandle { path, file })
    }

    fn contains(&self, name: &str) -> Result<bool> {
        let path = self.local_path(&name);
        match OpenOptions::new().read(true).open(path) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
            Err(err) => Err(Error::io(err)),
        }
    }
}

pub struct FileReadHandle {
    file: File,
}

impl ReadHandle for FileReadHandle {
    fn metadata(&self) {
        todo!()
    }
}

impl Read for FileReadHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.file.read(buf)
    }
}

pub struct FileWriteHandle {
    path: PathBuf,
    file: File,
}

impl WriteHandle for FileWriteHandle {
    fn seal(self, name: &str) -> Result<()> {
        let mut new_path = self
            .path
            .parent()
            .ok_or_else(|| Error::invalid("file has no parent directory"))?
            .to_owned();

        new_path.push(name);
        std::fs::rename(self.path, new_path).map_err(Error::io)
    }
}

impl Write for FileWriteHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_read_and_write() {
        let dir = create_temp_dir();
        let store = FileStore::new(dir.clone());

        {
            let mut file = store.write().unwrap();
            file.write(&[1, 2, 3]).unwrap();
            file.seal("foo").unwrap();
        }

        let mut file = store.read("foo").unwrap();
        let mut read = vec![];
        file.read_to_end(&mut read);
        assert_eq!(read, vec![1, 2, 3]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn create_temp_dir() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("rust-test-{}", rand::string(20)));
        std::fs::create_dir(&path).unwrap();
        path
    }
}
