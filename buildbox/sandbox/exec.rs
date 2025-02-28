use std::path::PathBuf;
use common::Result;
use common::rand;

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
      let id = rand::string(30);

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