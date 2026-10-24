//! The filesystem, as the build graph sees it.
//!
//! Files are read through keys, so the graph knows what each value depends
//! on. The keys that read the disk are external: a workspace's graph
//! recomputes them when told they may have changed. Everything computed from
//! them, such as [`Glob`] results, is an ordinary key, shared between
//! workspaces that have the same files.

mod glob;

pub use glob::{Glob, GlobError, GlobPattern, GlobResult};

use intern::{PathId, Symbol};
use skycodec::{Decode, Encode, Fingerprint};
use skyframe::{encoded_value, Ctx, Key, SkyError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A directory tree that files are read from.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Encode, Decode)]
pub enum Root {
    /// The workspace being built, wherever its graph's [`Workspace`] says it
    /// is. Workspaces at different paths have the same keys, so their
    /// results can be shared.
    Workspace,
    /// A directory elsewhere, by absolute path, such as a fetched repository.
    Dir(Symbol),
}

/// Where a workspace is: the service that resolves [`Root::Workspace`].
#[derive(Debug)]
pub struct Workspace {
    pub root: PathBuf,
}

/// A file or directory within a [`Root`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Encode, Decode)]
pub struct RootedPath {
    pub root: Root,
    pub path: PathId,
}

impl RootedPath {
    pub fn new(root: Root, path: PathId) -> RootedPath {
        RootedPath { root, path }
    }

    /// The path on disk, given the graph's workspace.
    pub fn resolve(&self, ctx: &Ctx) -> Result<PathBuf, SkyError> {
        let base = match self.root {
            Root::Workspace => ctx.global::<Workspace>()?.root.clone(),
            Root::Dir(dir) => PathBuf::from(dir.as_str()),
        };
        if self.path.is_root() {
            return Ok(base);
        }
        Ok(base.join(self.path.to_string()))
    }

    pub fn join(&self, name: Symbol) -> RootedPath {
        RootedPath::new(self.root, self.path.join(name))
    }
}

/// What is at a path, following symlinks.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum FileState {
    Missing,
    File { size: u64, executable: bool },
    Directory,
    /// Something else, such as a socket.
    Other,
}

encoded_value!(FileState);

/// Reads a path's [`FileState`]. A file's contents can change without its
/// state changing; read them with [`FileContentKey`].
#[derive(Clone, PartialEq, Eq, Hash, Debug, Encode, Decode)]
pub struct FileStateKey(pub RootedPath);

impl Key for FileStateKey {
    type Value = FileState;
    const KIND: &'static str = "FILE_STATE";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<FileState, SkyError> {
        let path = self.0.resolve(ctx)?;
        blocking(move || stat(&path)).await
    }
}

fn stat(path: &Path) -> Result<FileState, SkyError> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if is_missing(&error) => return Ok(FileState::Missing),
        Err(error) => return Err(io_error("stat", path, error)),
    };
    if metadata.is_dir() {
        return Ok(FileState::Directory);
    }
    if !metadata.is_file() {
        return Ok(FileState::Other);
    }
    Ok(FileState::File {
        size: metadata.len(),
        executable: is_executable(&metadata),
    })
}

/// The kind of a directory entry, following symlinks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum EntryKind {
    File,
    Directory,
    /// A symlink that points nowhere, or something that isn't a file or
    /// directory.
    Other,
}

/// A directory's entries, sorted by name. Empty if the directory is missing.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct DirListing {
    pub entries: Vec<(Symbol, EntryKind)>,
}

encoded_value!(DirListing);

impl DirListing {
    pub fn get(&self, name: Symbol) -> Option<EntryKind> {
        let index = self.entries.binary_search_by(|(entry, _)| entry.as_str().cmp(name.as_str())).ok()?;
        Some(self.entries[index].1)
    }
}

/// Lists a directory.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Encode, Decode)]
pub struct DirListingKey(pub RootedPath);

impl Key for DirListingKey {
    type Value = DirListing;
    const KIND: &'static str = "DIRECTORY_LISTING";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<DirListing, SkyError> {
        let path = self.0.resolve(ctx)?;
        blocking(move || list(&path)).await
    }
}

fn list(path: &Path) -> Result<DirListing, SkyError> {
    let reader = match std::fs::read_dir(path) {
        Ok(reader) => reader,
        Err(error) if is_missing(&error) || error.kind() == ErrorKind::NotADirectory => {
            return Ok(DirListing { entries: Vec::new() });
        }
        Err(error) => return Err(io_error("list", path, error)),
    };
    let mut entries = Vec::new();
    for entry in reader {
        let entry = entry.map_err(|error| io_error("list", path, error))?;
        let Some(name) = entry.file_name().to_str().map(Symbol::intern) else {
            continue;
        };
        entries.push((name, entry_kind(&entry)));
    }
    entries.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    Ok(DirListing { entries })
}

fn entry_kind(entry: &std::fs::DirEntry) -> EntryKind {
    let Ok(file_type) = entry.file_type() else {
        return EntryKind::Other;
    };
    if file_type.is_dir() {
        return EntryKind::Directory;
    }
    if file_type.is_file() {
        return EntryKind::File;
    }
    match std::fs::metadata(entry.path()) {
        Ok(metadata) if metadata.is_dir() => EntryKind::Directory,
        Ok(metadata) if metadata.is_file() => EntryKind::File,
        _ => EntryKind::Other,
    }
}

/// A file's contents, or `None` if there is no file at the path.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct FileContent {
    pub bytes: Option<Vec<u8>>,
}

encoded_value!(FileContent);

impl FileContent {
    pub fn digest(&self) -> Option<Fingerprint> {
        self.bytes.as_deref().map(Fingerprint::of)
    }
}

/// Reads a file, such as a BUILD or `.bzl` file.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Encode, Decode)]
pub struct FileContentKey(pub RootedPath);

impl Key for FileContentKey {
    type Value = FileContent;
    const KIND: &'static str = "FILE_CONTENT";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<FileContent, SkyError> {
        let path = self.0.resolve(ctx)?;
        blocking(move || read(&path)).await
    }
}

fn read(path: &Path) -> Result<FileContent, SkyError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(FileContent { bytes: Some(bytes) }),
        Err(error) if is_missing(&error) || error.kind() == ErrorKind::IsADirectory => Ok(FileContent { bytes: None }),
        Err(error) => Err(io_error("read", path, error)),
    }
}

fn is_missing(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> SkyError {
    SkyError::transient(format!("failed to {action} {}: {error}", path.display()))
}

/// Run filesystem work on a blocking thread.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, SkyError> + Send + 'static) -> Result<T, SkyError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) => Err(SkyError::transient(format!("filesystem task failed: {error}"))),
    }
}

/// The services a graph's filesystem keys need.
pub fn workspace(root: impl Into<PathBuf>) -> Arc<Workspace> {
    Arc::new(Workspace { root: root.into() })
}

#[cfg(test)]
mod tests;
