//! Finding which files the graph read have changed since.

use crate::RootedPath;
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

/// How many paths each blocking task stats when looking for changes.
const STATS_PER_TASK: usize = 512;

/// What a path looked like when it was read: enough to tell, by statting it
/// again, whether it has changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Signature {
    exists: bool,
    is_dir: bool,
    size: u64,
    modified_ns: i64,
    changed_ns: i64,
    inode: u64,
    link_changed_ns: i64,
}

fn signature(path: &Path) -> Signature {
    let link_changed_ns = std::fs::symlink_metadata(path).map(|m| m.ctime() * 1_000_000_000 + m.ctime_nsec()).unwrap_or(0);
    match std::fs::metadata(path) {
        Ok(metadata) => Signature {
            exists: true,
            is_dir: metadata.is_dir(),
            size: metadata.len(),
            modified_ns: metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
            changed_ns: metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec(),
            inode: metadata.ino(),
            link_changed_ns,
        },
        Err(_) => Signature {
            exists: false,
            is_dir: false,
            size: 0,
            modified_ns: 0,
            changed_ns: 0,
            inode: 0,
            link_changed_ns,
        },
    }
}

/// Remembers the signature of every path a workspace's file keys read.
///
/// File keys record a path's signature before reading it, so a change made
/// while it's read is still seen as a change later. [`FileChanges::changed`]
/// stats every recorded path again, in parallel, and returns those that
/// differ; [`FileChanges::settle`] then takes the new signatures as
/// recorded, so that a change is found once even if no key reads the path
/// again.
#[derive(Default)]
pub struct FileChanges {
    seen: Mutex<HashMap<RootedPath, (PathBuf, Signature)>>,
}

/// A recorded path whose signature has changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub path: RootedPath,
    recorded: Signature,
    found: Signature,
}

impl FileChanges {
    pub fn new() -> Arc<FileChanges> {
        Arc::new(FileChanges::default())
    }

    /// Record a path's current signature. Call before reading it.
    pub fn record(&self, path: RootedPath, absolute: &Path) {
        let signature = signature(absolute);
        self.lock().insert(path, (absolute.to_path_buf(), signature));
    }

    /// How many paths are being watched.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The recorded paths that have changed since they were recorded.
    pub async fn changed(&self) -> Vec<Change> {
        let entries: Vec<(RootedPath, PathBuf, Signature)> = self
            .lock()
            .iter()
            .map(|(path, (absolute, signature))| (*path, absolute.clone(), *signature))
            .collect();
        let mut tasks = Vec::new();
        for chunk in entries.chunks(STATS_PER_TASK) {
            let chunk = chunk.to_vec();
            tasks.push(tokio::task::spawn_blocking(move || chunk.into_iter().filter_map(compare).collect::<Vec<_>>()));
        }
        let mut changed = Vec::new();
        for task in tasks {
            match task.await {
                Ok(changes) => changed.extend(changes),
                Err(error) => tracing::warn!("failed to check files for changes: {error}"),
            }
        }
        changed
    }

    /// Take the signatures `changes` found as the recorded ones. Call once
    /// what read the paths has been invalidated, so a change is found only
    /// once. A path recorded again since keeps what was recorded then.
    pub fn settle(&self, changes: &[Change]) {
        let mut seen = self.lock();
        for change in changes {
            let Some((_, signature)) = seen.get_mut(&change.path) else {
                continue;
            };
            if *signature == change.recorded {
                *signature = change.found;
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<RootedPath, (PathBuf, Signature)>> {
        self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The change to a recorded path, if it has changed.
fn compare((path, absolute, recorded): (RootedPath, PathBuf, Signature)) -> Option<Change> {
    let found = signature(&absolute);
    if found == recorded {
        return None;
    }
    tracing::debug!(path = %absolute.display(), "changed");
    Some(Change { path, recorded, found })
}
