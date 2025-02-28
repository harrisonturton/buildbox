use common::{rand, Error, Result};
use std::ops::Drop;
use std::path::PathBuf;

/// Executes build actions within local directories.
///
/// The local sandbox implementation does not perform any isolation. It just
/// runs within a temporary directory. This means the entire host environment
/// will be made available to whatever command is executed.
#[derive(Debug)]
pub struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    /// Create a [`Sandbox`] instance for local execution.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Construct a new action execution environment.
    pub fn spawn(&self) -> Result<SandboxHandle> {
        let id = self.generate_id();
        let path = self.local_path(&id);
        std::fs::create_dir(&path).map_err(Error::io)?;
        Ok(SandboxHandle::new(path))
    }

    fn generate_id(&self) -> String {
        rand::string(10)
    }

    fn local_path(&self, name: &str) -> PathBuf {
        let mut path = self.dir.clone();
        path.push(name);
        path
    }
}

/// A handle to the constructed action execution environment.
#[derive(Debug)]
pub struct SandboxHandle {
    dir: PathBuf,
}

impl SandboxHandle {
    /// Create a [`SandboxHandle`] instance.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl Drop for SandboxHandle {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.dir) {
            tracing::error!("failed to remove sandbox: {err:?}");
        }
    }
}
