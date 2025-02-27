use std::path::PathBuf;
use crate::Result;
use rand::{thread_rng, Rng};
use rand::distr::Alphanumeric;

#[derive(Debug, Clone, PartialEq)]
pub struct LocalExecService {
    dir: PathBuf,
}

impl LocalExecService {
    /// Create a new [`LocalExecService`] instance.
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    #[must_use]
    pub fn open_sandbox(&self) -> Result<ExecSandbox> {
      let id = sandbox_id();

      let mut path = self.dir.clone();
      path.push(id);

      Ok(ExecSandbox { dir: path })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PrepareAction {
    CreateFile { sha256: String, path: PathBuf, executable: bool },
    CreateDir { path: PathBuf },
    CreateSymlink { path: PathBuf, target: PathBuf },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecCommand {
  pub args: Vec<String>,
  pub env: Vec<(String, String)>,
  pub outputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecSandbox {
    dir: PathBuf,
}

impl ExecSandbox {
  pub fn prepare(&self, actions: &[PrepareAction]) -> Result<()> {
    tracing::info!("ExecSandbox::prepare {:?}", self.dir);

    for action in actions {
      tracing::info!("{action:?}");
    }

    Ok(())
  }

  pub fn exec(&self, cmd: &ExecCommand) -> Result<()> {
    tracing::info!("ExecSandbox::exec {cmd:?}");
    Ok(())
  }
}

fn sandbox_id() -> String {
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(30)
        .map(char::from)
        .collect()
}