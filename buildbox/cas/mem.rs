use super::{ReadHandle, Store, WriteHandle};
use common::hash::Digest;
use common::hash::Hasher;
use common::{Error, Result};
use prost::Message;
use std::collections::HashMap;
use std::default::Default;
use std::io::{Cursor, Read, Write};
use std::sync::MutexGuard;
use std::sync::{Arc, Mutex};
use tracing::instrument::WithDispatch;
use tracing::Instrument;

type SharedMap<K, T> = Arc<Mutex<HashMap<K, T>>>;

#[derive(Debug, Clone)]
struct SharedVec<T>(Arc<Vec<T>>);

impl AsRef<[u8]> for SharedVec<u8> {
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref().as_ref()
    }
}

#[derive(Debug, Clone)]
pub struct MemStore {
    inner: SharedMap<String, SharedVec<u8>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Store for MemStore {
    type WriteHandle = MemWriteHandle;

    type ReadHandle = MemReadHandle;

    fn read(&self, name: &str) -> Result<Self::ReadHandle> {
        let mut inner = self.inner.lock().unwrap();
        let data = inner.get(name).unwrap().clone();
        Ok(MemReadHandle::new(data))
    }

    fn write(&self) -> Result<Self::WriteHandle> {
        Ok(MemWriteHandle {
            files: self.inner.clone(),
            data: Vec::new(),
        })
    }

    fn contains(&self, name: &str) -> Result<bool> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.contains_key(name))
    }
}

pub struct MemWriteHandle {
    files: SharedMap<String, SharedVec<u8>>,
    data: Vec<u8>,
}

impl WriteHandle for MemWriteHandle {
    fn seal(self, name: &str) -> Result<()> {
        let mut inner = self.files.lock().unwrap();
        let data = SharedVec(Arc::new(self.data));
        inner.insert(name.to_owned(), data);
        Ok(())
    }
}

impl Write for MemWriteHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.data.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.data.flush()
    }
}

pub struct MemReadHandle {
    data: Cursor<SharedVec<u8>>,
}

impl MemReadHandle {
    pub fn new(data: SharedVec<u8>) -> Self {
        Self {
            data: Cursor::new(data),
        }
    }
}

impl ReadHandle for MemReadHandle {
    fn metadata(&self) {
        todo!()
    }
}

impl Read for MemReadHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.data.read(buf)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test() {
        let store = MemStore::new();

        {
            let mut file = store.write().unwrap();
            file.write(&[1, 2, 3]).unwrap();
            file.seal("foo").unwrap();
        }

        let mut file = store.read("foo").unwrap();
        let mut read = vec![];
        file.read_to_end(&mut read);
        assert_eq!(read, vec![1, 2, 3]);
    }
}
