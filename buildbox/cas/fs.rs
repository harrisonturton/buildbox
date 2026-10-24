//! A [`Cas`] that stores each blob as a file on the local filesystem.

use async_trait::async_trait;
use cas::{BlobReader, BlobWriter, Cas, CasError, Digest, Hasher, Usage};
use fsutil::run_blocking;
use std::fs::{File, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempPath;
use tokio::io::AsyncWriteExt;

/// Committed blobs are read-only, so a materialized hard link to one can't be
/// used to modify it.
const BLOB_MODE: u32 = 0o444;
const EXECUTABLE_BLOB_MODE: u32 = 0o555;

/// Modes for materialized copies, used when hard links aren't possible.
const FILE_MODE: u32 = 0o644;
const EXECUTABLE_MODE: u32 = 0o755;

const COPY_CHUNK_SIZE: usize = 64 * 1024;

/// Stores blobs as files named after their hash.
///
/// The directory layout is:
///
/// - `lock`: held while the store is open, so only one process uses it.
/// - `blobs/<hash>`: committed blobs.
/// - `exec/<hash>`: executable copies of blobs, made the first time a blob is
///   materialized as an executable. Hard links to a file share its mode, so
///   executable and non-executable links need separate files.
/// - `tmp/`: blobs being written. Anything here when the store is opened was
///   abandoned, and is deleted.
///
/// Blobs are materialized as hard links when the destination is on the same
/// filesystem, so populating a sandbox doesn't copy any data. Linked files are
/// read-only, but anything that changes their permissions or contents in
/// place changes the stored blob too.
#[derive(Debug, Clone)]
pub struct FsCas {
    dirs: Arc<Dirs>,
}

#[derive(Debug)]
struct Dirs {
    blobs: PathBuf,
    executables: PathBuf,
    tmp: PathBuf,
    // Never read: holding the file holds the lock.
    _lock: File,
}

impl FsCas {
    /// Open the store in `dir`, creating it if needed.
    ///
    /// Fails if another process already has the store open.
    pub async fn open(dir: &Path) -> Result<FsCas, CasError> {
        let dir = dir.to_path_buf();
        run_blocking(move || open_blocking(&dir)).await
    }

    fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.dirs.blobs.join(digest.hash())
    }

    fn executable_path(&self, digest: &Digest) -> PathBuf {
        self.dirs.executables.join(digest.hash())
    }
}

fn open_blocking(dir: &Path) -> Result<FsCas, CasError> {
    create_dir_all(dir)?;
    let lock = fsutil::lock_dir(dir).map_err(|error| io_error("failed to lock", dir, error))?;

    let blobs = dir.join("blobs");
    create_dir_all(&blobs)?;

    let executables = dir.join("exec");
    create_dir_all(&executables)?;

    let tmp = dir.join("tmp");
    fsutil::reset_dir(&tmp).map_err(|error| io_error("failed to reset", &tmp, error))?;

    let dirs = Dirs {
        blobs,
        executables,
        tmp,
        _lock: lock,
    };
    let cas = FsCas {
        dirs: Arc::new(dirs),
    };

    store_empty_blob(&cas)?;
    Ok(cas)
}

/// Clients may reference the empty blob without uploading it.
fn store_empty_blob(cas: &FsCas) -> Result<(), CasError> {
    let path = cas.blob_path(&Digest::empty());
    if path.exists() {
        return Ok(());
    }

    let temp_path = new_temp_file(&cas.dirs.tmp)?.1;
    persist_blob(temp_path, &path)
}

// The spans here are at debug level: there is one for every blob an action
// reads or writes, which is too many to trace by default.
#[async_trait]
impl Cas for FsCas {
    #[tracing::instrument(level = "debug", name = "cas_find_missing", skip_all, fields(blobs = digests.len()))]
    async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
        let cas = self.clone();
        let digests = digests.to_vec();
        run_blocking(move || {
            let mut missing = vec![];
            for digest in digests {
                if !cas.blob_path(&digest).exists() {
                    missing.push(digest);
                }
            }
            Ok(missing)
        })
        .await
    }

    #[tracing::instrument(level = "debug", name = "cas_open_blob", skip_all, fields(digest = %digest))]
    async fn read(&self, digest: &Digest) -> Result<BlobReader, CasError> {
        let path = self.blob_path(digest);
        match tokio::fs::File::open(&path).await {
            Ok(file) => Ok(Box::pin(file)),
            Err(error) if error.kind() == ErrorKind::NotFound => Err(CasError::NotFound(digest.clone())),
            Err(error) => Err(io_error("failed to open", &path, error)),
        }
    }

    #[tracing::instrument(level = "debug", name = "cas_start_write", skip_all, fields(digest = %expected))]
    async fn write(&self, expected: &Digest) -> Result<Box<dyn BlobWriter>, CasError> {
        let tmp_dir = self.dirs.tmp.clone();
        let (file, temp_path) = run_blocking(move || new_temp_file(&tmp_dir)).await?;

        let writer = FsBlobWriter {
            file: tokio::fs::File::from_std(file),
            temp_path,
            blob_path: self.blob_path(expected),
            expected: expected.clone(),
            hasher: Hasher::new(),
        };
        Ok(Box::new(writer))
    }

    #[tracing::instrument(level = "debug", name = "cas_put_file", skip_all)]
    async fn put_file(&self, path: &Path) -> Result<Digest, CasError> {
        let cas = self.clone();
        let path = path.to_path_buf();
        run_blocking(move || put_file_blocking(&cas, &path)).await
    }

    #[tracing::instrument(level = "debug", name = "cas_materialize", skip_all, fields(digest = %digest))]
    async fn materialize(&self, digest: &Digest, dest: &Path, executable: bool) -> Result<(), CasError> {
        let cas = self.clone();
        let digest = digest.clone();
        let dest = dest.to_path_buf();
        run_blocking(move || materialize_blocking(&cas, &digest, &dest, executable)).await
    }

    async fn usage(&self) -> Result<Usage, CasError> {
        let blobs_dir = self.dirs.blobs.clone();
        run_blocking(move || usage_blocking(&blobs_dir)).await
    }
}

/// Copies a file into the store in one pass, hashing it along the way.
fn put_file_blocking(cas: &FsCas, path: &Path) -> Result<Digest, CasError> {
    let mut source = File::open(path).map_err(|error| io_error("failed to open", path, error))?;
    let (mut temp_file, temp_path) = new_temp_file(&cas.dirs.tmp)?;

    let mut hasher = Hasher::new();
    let mut buf = vec![0; COPY_CHUNK_SIZE];
    loop {
        let read = source.read(&mut buf).map_err(|error| io_error("failed to read", path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        temp_file
            .write_all(&buf[..read])
            .map_err(|error| io_error("failed to write", &temp_path, error))?;
    }
    temp_file
        .sync_data()
        .map_err(|error| io_error("failed to sync", &temp_path, error))?;

    let digest = hasher.finish();
    persist_blob(temp_path, &cas.blob_path(&digest))?;
    Ok(digest)
}

/// Hard link a blob into place, or copy it where hard links can't be used.
fn materialize_blocking(cas: &FsCas, digest: &Digest, dest: &Path, executable: bool) -> Result<(), CasError> {
    let source = if executable {
        executable_blob(cas, digest)?
    } else {
        cas.blob_path(digest)
    };

    let error = match std::fs::hard_link(&source, dest) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    if error.kind() == ErrorKind::NotFound && !source.exists() {
        return Err(CasError::NotFound(digest.clone()));
    }
    if !can_copy_instead_of_linking(&error) {
        return Err(io_error("failed to link", dest, error));
    }
    tracing::debug!("copying blob {digest}, since it can't be linked: {error}");

    let mode = if executable { EXECUTABLE_MODE } else { FILE_MODE };
    copy_file(&source, dest, mode)
}

/// The executable copy of a blob, made on first use.
fn executable_blob(cas: &FsCas, digest: &Digest) -> Result<PathBuf, CasError> {
    let path = cas.executable_path(digest);
    if path.exists() {
        return Ok(path);
    }

    let blob_path = cas.blob_path(digest);
    let mut source = match File::open(&blob_path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Err(CasError::NotFound(digest.clone())),
        Err(error) => return Err(io_error("failed to open", &blob_path, error)),
    };

    let (mut temp_file, temp_path) = new_temp_file(&cas.dirs.tmp)?;
    std::io::copy(&mut source, &mut temp_file).map_err(|error| io_error("failed to write", &temp_path, error))?;
    // Close the file before it can be run: running a file that's open for
    // writing fails.
    drop(temp_file);
    persist_with_mode(temp_path, &path, EXECUTABLE_BLOB_MODE)?;
    Ok(path)
}

/// Hard links fail across filesystems, and when a file has the most links
/// the filesystem allows. A copy works in both cases.
fn can_copy_instead_of_linking(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::CrossesDevices | ErrorKind::TooManyLinks)
}

fn copy_file(source: &Path, dest: &Path, mode: u32) -> Result<(), CasError> {
    let mut source_file = File::open(source).map_err(|error| io_error("failed to open", source, error))?;
    let mut file = File::create(dest).map_err(|error| io_error("failed to create", dest, error))?;
    std::io::copy(&mut source_file, &mut file).map_err(|error| io_error("failed to write", dest, error))?;
    file.set_permissions(Permissions::from_mode(mode))
        .map_err(|error| io_error("failed to set permissions on", dest, error))
}

fn usage_blocking(blobs_dir: &Path) -> Result<Usage, CasError> {
    let entries = std::fs::read_dir(blobs_dir).map_err(|error| io_error("failed to list", blobs_dir, error))?;

    let mut usage = Usage::default();
    for entry in entries {
        let entry = entry.map_err(|error| io_error("failed to list", blobs_dir, error))?;
        let metadata = entry
            .metadata()
            .map_err(|error| io_error("failed to stat", &entry.path(), error))?;
        usage.blobs += 1;
        usage.bytes += metadata.len();
    }
    Ok(usage)
}

struct FsBlobWriter {
    file: tokio::fs::File,
    temp_path: TempPath,
    blob_path: PathBuf,
    expected: Digest,
    hasher: Hasher,
}

#[async_trait]
impl BlobWriter for FsBlobWriter {
    async fn write(&mut self, data: &[u8]) -> Result<(), CasError> {
        self.hasher.update(data);
        self.file
            .write_all(data)
            .await
            .map_err(|error| io_error("failed to write", &self.temp_path, error))
    }

    #[tracing::instrument(level = "debug", name = "cas_commit", skip_all, fields(digest = %self.expected))]
    async fn commit(self: Box<Self>) -> Result<(), CasError> {
        let FsBlobWriter {
            mut file,
            temp_path,
            blob_path,
            expected,
            hasher,
        } = *self;

        let actual = hasher.finish();
        if actual != expected {
            // Dropping temp_path deletes the data.
            return Err(CasError::DigestMismatch { expected, actual });
        }

        file.flush()
            .await
            .map_err(|error| io_error("failed to write", &temp_path, error))?;
        file.sync_data()
            .await
            .map_err(|error| io_error("failed to sync", &temp_path, error))?;
        drop(file);

        run_blocking(move || persist_blob(temp_path, &blob_path)).await
    }
}

/// Create a file in `tmp_dir` that is deleted when the returned path is
/// dropped, unless it is persisted first.
fn new_temp_file(tmp_dir: &Path) -> Result<(File, TempPath), CasError> {
    let temp_file = tempfile::NamedTempFile::new_in(tmp_dir)
        .map_err(|error| io_error("failed to create temporary file in", tmp_dir, error))?;
    Ok(temp_file.into_parts())
}

/// Make a fully-written temporary file visible as a blob.
fn persist_blob(temp_path: TempPath, blob_path: &Path) -> Result<(), CasError> {
    persist_with_mode(temp_path, blob_path, BLOB_MODE)
}

fn persist_with_mode(temp_path: TempPath, path: &Path, mode: u32) -> Result<(), CasError> {
    std::fs::set_permissions(&temp_path, Permissions::from_mode(mode))
        .map_err(|error| io_error("failed to set permissions on", &temp_path, error))?;

    // If the file already exists this replaces it with identical contents,
    // which is harmless.
    temp_path
        .persist(path)
        .map_err(|error| io_error("failed to rename into", path, error.error))
}

fn create_dir_all(path: &Path) -> Result<(), CasError> {
    std::fs::create_dir_all(path).map_err(|error| io_error("failed to create", path, error))
}

fn io_error(action: &str, path: &Path, error: std::io::Error) -> CasError {
    CasError::io(format!("{action} {}", path.display()), error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn conformance() {
        cas_conformance::run_all(|dir| async move {
            let cas = FsCas::open(&dir).await.unwrap();
            Box::new(cas) as Box<dyn Cas>
        })
        .await;
    }

    #[tokio::test]
    async fn open_deletes_abandoned_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let abandoned = dir.path().join("tmp/abandoned");
        std::fs::create_dir_all(abandoned.parent().unwrap()).unwrap();
        std::fs::write(&abandoned, b"partial").unwrap();

        let _cas = FsCas::open(dir.path()).await.unwrap();

        assert!(!abandoned.exists());
    }

    #[tokio::test]
    async fn open_fails_while_another_store_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let _first = FsCas::open(dir.path()).await.unwrap();

        let err = FsCas::open(dir.path()).await.unwrap_err();

        assert!(err.to_string().contains("in use by another process"), "{err}");
    }

    #[tokio::test]
    async fn materialize_hard_links_read_only_files() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let cas = FsCas::open(&dir.path().join("cas")).await.unwrap();
        let digest = cas.put_bytes(b"shared").await.unwrap();
        let [a, b, x, y] = ["a", "b", "x", "y"].map(|name| dir.path().join(name));

        cas.materialize(&digest, &a, false).await.unwrap();
        cas.materialize(&digest, &b, false).await.unwrap();
        cas.materialize(&digest, &x, true).await.unwrap();
        cas.materialize(&digest, &y, true).await.unwrap();

        let [a, b, x, y] = [a, b, x, y].map(|path| std::fs::metadata(path).unwrap());
        assert_eq!(a.ino(), b.ino());
        assert_eq!(x.ino(), y.ino());
        assert_ne!(a.ino(), x.ino());
        assert_eq!(a.permissions().mode() & 0o777, BLOB_MODE);
        assert_eq!(x.permissions().mode() & 0o777, EXECUTABLE_BLOB_MODE);
    }

    #[tokio::test]
    async fn uncommitted_writes_leave_no_temporary_files() {
        let dir = tempfile::tempdir().unwrap();
        let cas = FsCas::open(dir.path()).await.unwrap();

        let mut writer = cas.write(&Digest::of_bytes(b"hello")).await.unwrap();
        writer.write(b"hello").await.unwrap();
        drop(writer);

        let leftovers = std::fs::read_dir(dir.path().join("tmp")).unwrap().count();
        assert_eq!(leftovers, 0);
    }
}
