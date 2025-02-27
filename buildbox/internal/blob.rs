use crate::Error;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::io::{BufReader, BufWriter, Read, Write};
use std::marker::Unpin;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct LocalBlobStore {
    dir: PathBuf,
}

impl LocalBlobStore {
    /// Create a new [`LocalBlobStore`] instance.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Check that the blob store contains an object.
    pub fn contains(&self, key: &str) -> Result<bool, Error> {
        let path = self.create_path(key);
        match OpenOptions::new().read(true).open(key) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
            Err(err) => Err(Error::io(err)),
        }
    }

    /// Read the blob identified by the key.
    pub fn read(&self, key: &str) -> Result<impl Read, Error> {
        let path = self.create_path(key);
        let file = OpenOptions::new()
            .read(true)
            .open(path)
            .map_err(Error::io)?;
        Ok(BufReader::new(file))
    }

    pub fn read_to_end(&self, key: &str) -> Result<Vec<u8>, Error> {
        let mut data = vec![];
        let mut reader = self.read(key)?;
        reader.read_to_end(&mut data).map_err(Error::io)?;
        Ok(data)
    }

    /// Create the blob identified by the key.
    pub fn write(&self, key: &str, src: impl Read) -> Result<(), Error> {
        let path = self.create_path(key);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(path)
            .map_err(Error::io)?;
        let mut writer = BufWriter::new(&file);
        let mut reader = BufReader::new(src);

        std::io::copy(&mut reader, &mut writer).map_err(Error::io)?;
        writer.flush().map_err(Error::io)?;

        Ok(())
    }

    fn create_path(&self, name: &str) -> PathBuf {
        let mut path = self.dir.clone();
        path.push(name);
        path
    }
}
