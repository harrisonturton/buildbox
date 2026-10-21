//! The directory an action runs in.

use executor::ExecuteError;
use fsutil::run_blocking;
use std::path::{Path, PathBuf};

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
            let dir = tempfile::Builder::new().prefix("action-").tempdir_in(&root)?;
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
