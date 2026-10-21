//! The directory an action runs in.

use executor::ExecuteError;
use fsutil::run_blocking;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// Sandbox directory names start with this, so leftovers can be recognised.
const SANDBOX_PREFIX: &str = "action-";

/// A fresh directory for one action, deleted when dropped unless it is kept
/// for debugging.
pub struct Sandbox {
    path: PathBuf,
    keep: bool,
}

impl Sandbox {
    /// Create an empty sandbox directory inside `root`.
    pub async fn create(root: &Path, keep: bool) -> Result<Sandbox, ExecuteError> {
        let root = root.to_path_buf();
        let path = run_blocking(move || {
            let dir = tempfile::Builder::new().prefix(SANDBOX_PREFIX).tempdir_in(&root)?;
            Ok(dir.keep())
        })
        .await
        .map_err(|error: std::io::Error| ExecuteError::Internal(format!("failed to create sandbox: {error}")))?;

        Ok(Sandbox { path, keep })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Create `root` if needed, and delete any sandboxes left in it by a previous
/// run. Other files in `root` are left alone.
pub fn prepare_root(root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(root)?;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let is_sandbox = entry.file_name().to_string_lossy().starts_with(SANDBOX_PREFIX);
        if is_sandbox {
            remove_dir_all_if_exists(&entry.path())?;
        }
    }
    Ok(())
}

fn remove_dir_all_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if self.keep {
            tracing::info!("keeping sandbox {}", self.path.display());
            return;
        }

        // Deleting a large tree is slow, so do it in the background.
        let path = std::mem::take(&mut self.path);
        let remove = move || {
            if let Err(error) = std::fs::remove_dir_all(&path) {
                tracing::warn!("failed to remove sandbox {}: {error}", path.display());
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(remove)),
            Err(_) => remove(),
        }
    }
}
