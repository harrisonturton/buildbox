//! Filesystem helpers shared by the on-disk stores.

use std::fs::{File, TryLockError};
use std::io::{self, ErrorKind};
use std::path::Path;

/// Run blocking work, such as filesystem calls, on tokio's blocking thread
/// pool rather than on the async runtime. The work runs in the current span,
/// so anything it logs or traces is attributed to the caller.
pub async fn run_blocking<T, E, F>(f: F) -> Result<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
    F: FnOnce() -> Result<T, E> + Send + 'static,
{
    let span = tracing::Span::current();
    match tokio::task::spawn_blocking(move || span.in_scope(f)).await {
        Ok(result) => result,
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        // Blocking tasks can only be cancelled by the runtime shutting down.
        Err(error) => panic!("blocking task cancelled: {error}"),
    }
}

/// Take an exclusive lock on `dir`, so only one process uses it at a time.
///
/// The lock is held until the returned file is dropped.
pub fn lock_dir(dir: &Path) -> io::Result<File> {
    let path = dir.join("lock");
    let file = File::create(&path)?;

    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(io::Error::new(ErrorKind::WouldBlock, "in use by another process")),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// Write everything buffered for the filesystem holding `path` to disk, so
/// files written there survive a crash.
pub fn sync_filesystem(path: &Path) -> io::Result<()> {
    let file = File::open(path)?;
    sync_filesystem_of(&file)
}

#[cfg(target_os = "linux")]
fn sync_filesystem_of(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: syncfs only reads the file descriptor, which `file` keeps open.
    let result = unsafe { libc::syncfs(file.as_raw_fd()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Other systems have no way to sync one filesystem, so sync them all.
#[cfg(not(target_os = "linux"))]
fn sync_filesystem_of(_file: &File) -> io::Result<()> {
    // SAFETY: sync has no arguments and can't fail.
    unsafe { libc::sync() };
    Ok(())
}

/// Delete `dir` and everything in it if it exists, then create it empty.
pub fn reset_dir(dir: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_dir_excludes_a_second_lock() {
        let dir = tempfile::tempdir().unwrap();
        let _first = lock_dir(dir.path()).unwrap();

        let err = lock_dir(dir.path()).unwrap_err();

        assert_eq!(err.kind(), ErrorKind::WouldBlock);
    }

    #[test]
    fn lock_dir_is_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        drop(lock_dir(dir.path()).unwrap());

        lock_dir(dir.path()).unwrap();
    }

    #[test]
    fn reset_dir_empties_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::create_dir_all(target.join("nested")).unwrap();
        std::fs::write(target.join("file"), b"data").unwrap();

        reset_dir(&target).unwrap();

        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    }

    #[test]
    fn reset_dir_creates_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a/b");

        reset_dir(&target).unwrap();

        assert!(target.is_dir());
    }
}
