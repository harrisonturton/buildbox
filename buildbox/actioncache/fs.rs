//! An [`ActionCache`] that stores each entry as a file in a directory.

use actioncache::{ActionCache, ActionCacheError, Usage};
use async_trait::async_trait;
use cas::Digest;
use fsutil::run_blocking;
use prost::Message;
use remote_execution_proto::build::bazel::remote::execution::v2::ActionResult;
use std::fs::File;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Stores each `ActionResult` as a file named after the action's hash.
///
/// The directory layout is:
///
/// - `lock`: held while the cache is open, so only one process uses it.
/// - `entries/<hash>`: serialized `ActionResult` protos.
/// - `tmp/`: entries being written. Anything here when the cache is opened
///   was abandoned, and is deleted.
#[derive(Debug, Clone)]
pub struct FsActionCache {
    dirs: Arc<Dirs>,
}

#[derive(Debug)]
struct Dirs {
    entries: PathBuf,
    tmp: PathBuf,
    // Never read: holding the file holds the lock.
    _lock: File,
}

impl FsActionCache {
    /// Open the cache in `dir`, creating it if needed.
    ///
    /// Fails if another process already has the cache open.
    pub async fn open(dir: &Path) -> Result<FsActionCache, ActionCacheError> {
        let dir = dir.to_path_buf();
        run_blocking(move || open_blocking(&dir)).await
    }

    fn entry_path(&self, action: &Digest) -> PathBuf {
        self.dirs.entries.join(action.hash())
    }
}

fn open_blocking(dir: &Path) -> Result<FsActionCache, ActionCacheError> {
    std::fs::create_dir_all(dir).map_err(|error| io_error("failed to create", dir, error))?;
    let lock = fsutil::lock_dir(dir).map_err(|error| io_error("failed to lock", dir, error))?;

    let entries = dir.join("entries");
    std::fs::create_dir_all(&entries).map_err(|error| io_error("failed to create", &entries, error))?;

    let tmp = dir.join("tmp");
    fsutil::reset_dir(&tmp).map_err(|error| io_error("failed to reset", &tmp, error))?;

    let dirs = Dirs {
        entries,
        tmp,
        _lock: lock,
    };
    Ok(FsActionCache {
        dirs: Arc::new(dirs),
    })
}

#[async_trait]
impl ActionCache for FsActionCache {
    async fn get(&self, action: &Digest) -> Result<Option<ActionResult>, ActionCacheError> {
        let path = self.entry_path(action);
        let data = match tokio::fs::read(&path).await {
            Ok(data) => data,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_error("failed to read", &path, error)),
        };

        match ActionResult::decode(data.as_slice()) {
            Ok(result) => Ok(Some(result)),
            Err(error) => {
                // A corrupt entry is as good as a missing one: the action will
                // run again and its result will replace this entry.
                tracing::warn!("ignoring corrupt action cache entry {}: {error}", path.display());
                Ok(None)
            }
        }
    }

    async fn put(&self, action: &Digest, result: &ActionResult) -> Result<(), ActionCacheError> {
        let data = result.encode_to_vec();
        let tmp_dir = self.dirs.tmp.clone();
        let entry_path = self.entry_path(action);
        run_blocking(move || write_entry(&tmp_dir, &entry_path, &data)).await
    }

    async fn usage(&self) -> Result<Usage, ActionCacheError> {
        let entries_dir = self.dirs.entries.clone();
        run_blocking(move || usage_blocking(&entries_dir)).await
    }
}

/// Write an entry to a temporary file, then rename it into place so readers
/// never see a partial entry.
fn write_entry(tmp_dir: &Path, entry_path: &Path, data: &[u8]) -> Result<(), ActionCacheError> {
    let mut temp_file = tempfile::NamedTempFile::new_in(tmp_dir)
        .map_err(|error| io_error("failed to create temporary file in", tmp_dir, error))?;

    temp_file
        .write_all(data)
        .map_err(|error| io_error("failed to write", temp_file.path(), error))?;
    temp_file
        .as_file()
        .sync_data()
        .map_err(|error| io_error("failed to sync", temp_file.path(), error))?;

    temp_file
        .persist(entry_path)
        .map_err(|error| io_error("failed to rename into", entry_path, error.error))?;
    Ok(())
}

fn usage_blocking(entries_dir: &Path) -> Result<Usage, ActionCacheError> {
    let entries = std::fs::read_dir(entries_dir).map_err(|error| io_error("failed to list", entries_dir, error))?;

    let mut usage = Usage::default();
    for entry in entries {
        let entry = entry.map_err(|error| io_error("failed to list", entries_dir, error))?;
        let metadata = entry
            .metadata()
            .map_err(|error| io_error("failed to stat", &entry.path(), error))?;
        usage.entries += 1;
        usage.bytes += metadata.len();
    }
    Ok(usage)
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> ActionCacheError {
    ActionCacheError::io(format!("{action} {}", path.display()), error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn conformance() {
        actioncache_conformance::run_all(|dir| async move {
            let cache = FsActionCache::open(&dir).await.unwrap();
            Box::new(cache) as Box<dyn ActionCache>
        })
        .await;
    }

    #[tokio::test]
    async fn entries_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let action = Digest::of_bytes(b"action");
        let result = ActionResult {
            exit_code: 3,
            ..Default::default()
        };

        let cache = FsActionCache::open(dir.path()).await.unwrap();
        cache.put(&action, &result).await.unwrap();
        drop(cache);
        let cache = FsActionCache::open(dir.path()).await.unwrap();

        assert_eq!(cache.get(&action).await.unwrap(), Some(result));
    }

    #[tokio::test]
    async fn corrupt_entries_are_misses() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FsActionCache::open(dir.path()).await.unwrap();
        let action = Digest::of_bytes(b"action");
        std::fs::write(cache.entry_path(&action), b"\xff\xff\xff not a proto").unwrap();

        assert_eq!(cache.get(&action).await.unwrap(), None);
    }
}
