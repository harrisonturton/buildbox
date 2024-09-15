use std::error::Error;
use std::io::ErrorKind;
use std::marker::Unpin;
use std::path::PathBuf;
use std::fs::{File, OpenOptions};
use std::io::{Read, BufReader, BufWriter, Write};

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
    pub fn contains(&self, key: &str) -> Result<bool, Box<dyn Error>> {
        let path = self.create_path(key);
        match OpenOptions::new().read(true).open(key) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
            Err(err) => Err(Box::new(err)),
        }
    }

    /// Read the blob identified by the key.
    pub fn read(&self, key: &str) -> Result<impl Read, Box<dyn Error>> {
        let path = self.create_path(key);
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(BufReader::new(file))
    }

    /// Create the blob identified by the key.
    pub fn write(
        &self,
        key: &str,
        src: impl Read,
    ) -> Result<(), Box<dyn Error>> {
        let path = self.create_path(key);
        let file = OpenOptions::new().write(true).create(true).open(path)?;
        let mut writer = BufWriter::new(&file);
        let mut reader = BufReader::new(src);

        std::io::copy(&mut reader, &mut writer)?;
        writer.flush()?;

        Ok(())
    }

    fn create_path(&self, name: &str) -> PathBuf {
        let mut path = self.dir.clone();
        path.push(name);
        path
    }
}
