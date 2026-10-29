//! The filesystem, as the build graph sees it.
//!
//! Files are read through keys, so the graph knows what each value depends
//! on. The keys that read the disk are external: a workspace's graph
//! recomputes them when told they may have changed. Everything computed from
//! them, such as [`Glob`] results, is an ordinary key, shared between
//! workspaces that have the same files.

mod changes;
mod glob;

pub use changes::FileChanges;

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
        let workspace = ctx.global::<Workspace>()?;
        Ok(self.resolve_in(&workspace.root))
    }

    /// The path on disk, for the workspace at `workspace`.
    pub fn resolve_in(&self, workspace: &Path) -> PathBuf {
        let base = match self.root {
            Root::Workspace => workspace.to_path_buf(),
            Root::Dir(dir) => PathBuf::from(dir.as_str()),
        };
        if self.path.is_root() {
            return base;
        }
        base.join(self.path.to_string())
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
        let changes = ctx.global::<FileChanges>().ok();
        let rooted = self.0;
        blocking(move || {
            record(changes.as_deref(), rooted, &path);
            stat(&path)
        })
        .await
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
    /// Each entry's kind, following symlinks.
    pub entries: Vec<(Symbol, EntryKind)>,
    /// The entries that are symlinks, sorted.
    pub symlinks: Vec<Symbol>,
}

encoded_value!(DirListing);

impl DirListing {
    pub fn is_symlink(&self, name: Symbol) -> bool {
        self.symlinks.binary_search_by(|entry| entry.as_str().cmp(name.as_str())).is_ok()
    }

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
        let changes = ctx.global::<FileChanges>().ok();
        let rooted = self.0;
        blocking(move || {
            record(changes.as_deref(), rooted, &path);
            list(&path)
        })
        .await
    }
}

fn list(path: &Path) -> Result<DirListing, SkyError> {
    let reader = match std::fs::read_dir(path) {
        Ok(reader) => reader,
        Err(error) if is_missing(&error) || error.kind() == ErrorKind::NotADirectory => {
            return Ok(DirListing {
                entries: Vec::new(),
                symlinks: Vec::new(),
            });
        }
        Err(error) => return Err(io_error("list", path, error)),
    };
    let mut entries = Vec::new();
    let mut symlinks = Vec::new();
    for entry in reader {
        let entry = entry.map_err(|error| io_error("list", path, error))?;
        let Some(name) = entry.file_name().to_str().map(Symbol::intern) else {
            continue;
        };
        if entry.file_type().is_ok_and(|file_type| file_type.is_symlink()) {
            symlinks.push(name);
        }
        entries.push((name, entry_kind(&entry)));
    }
    entries.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    symlinks.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(DirListing { entries, symlinks })
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

/// A file's contents, as their digest, or `None` if there is no file at
/// the path.
///
/// The graph keeps only the digest: what depends on a file's contents
/// changes when the digest does, and reads the contents itself with
/// [`read_file`], which checks them against it. So the store and cache
/// don't hold a copy of every BUILD and `.bzl` file.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct FileContent {
    pub digest: Option<Fingerprint>,
}

encoded_value!(FileContent);

/// Reads a file's digest, such as a BUILD or `.bzl` file's.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Encode, Decode)]
pub struct FileContentKey(pub RootedPath);

impl Key for FileContentKey {
    type Value = FileContent;
    const KIND: &'static str = "FILE_CONTENT";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<FileContent, SkyError> {
        let path = self.0.resolve(ctx)?;
        let changes = ctx.global::<FileChanges>().ok();
        let rooted = self.0;
        blocking(move || {
            record(changes.as_deref(), rooted, &path);
            read(&path)
        })
        .await
    }
}

fn read(path: &Path) -> Result<FileContent, SkyError> {
    Ok(FileContent {
        digest: read_bytes(path)?.as_deref().map(Fingerprint::of),
    })
}

fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, SkyError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if is_missing(&error) || error.kind() == ErrorKind::IsADirectory => Ok(None),
        Err(error) => Err(io_error("read", path, error)),
    }
}

/// Read a file's contents, or `None` if it doesn't exist, recording that
/// what's being computed depends on them. The contents must have the digest
/// the graph has for them; if the file changed since, the read fails as a
/// transient error, and the change is seen by the next evaluation.
pub async fn read_file(ctx: &Ctx, path: RootedPath) -> Result<Option<Vec<u8>>, SkyError> {
    let content = ctx.compute(&FileContentKey(path)).await?;
    let Some(digest) = content.digest else {
        return Ok(None);
    };
    let absolute = path.resolve(ctx)?;
    let bytes = blocking(move || read_bytes(&absolute)).await?;
    match bytes {
        Some(bytes) if Fingerprint::of(&bytes) == digest => Ok(Some(bytes)),
        _ => Err(SkyError::transient(format!("{} changed while it was being read", path.path))),
    }
}

fn record(changes: Option<&FileChanges>, rooted: RootedPath, path: &Path) {
    if let Some(changes) = changes {
        changes.record(rooted, path);
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

/// The directory of the workspace a graph is for, as a value keys can
/// depend on, so that what they compute from it isn't shared with other
/// workspaces.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct WorkspaceRoot(pub String);

encoded_value!(WorkspaceRoot);

#[derive(Clone, PartialEq, Eq, Hash, Debug, Encode, Decode)]
pub struct WorkspaceRootKey;

impl Key for WorkspaceRootKey {
    type Value = WorkspaceRoot;
    const KIND: &'static str = "WORKSPACE_ROOT";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<WorkspaceRoot, SkyError> {
        let workspace = ctx.global::<Workspace>()?;
        Ok(WorkspaceRoot(workspace.root.display().to_string()))
    }
}

/// The services a graph's filesystem keys need.
pub fn workspace(root: impl Into<PathBuf>) -> Arc<Workspace> {
    Arc::new(Workspace { root: root.into() })
}

#[cfg(test)]
mod tests;
