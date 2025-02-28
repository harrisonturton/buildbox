pub mod sandbox;

pub use sandbox::Sandbox;

use std::{collections::HashMap, path::PathBuf};
use proto::bazel::exec::Digest;

#[derive(Debug, Clone, PartialEq)]
pub struct ExecCommand {
  pub args: Vec<String>,
  pub env: HashMap<String, String>,
  pub outputs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SandboxTemplate {
    pub filesystem: Vec<DentryTemplate>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DentryTemplate {
    File(FileTemplate),
    Symlink(SymlinkTemplate),
    Dir(DirTemplate),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileTemplate {
    pub digest: Digest,
    pub path: PathBuf,
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SymlinkTemplate {
    pub path: PathBuf,
    pub target: PathBuf,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: Digest,
    pub stderr: Digest,
    pub outputs: Vec<GeneratedFile>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedFile {
    pub path: PathBuf,
    pub digest: Digest,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DirTemplate {
    pub path: PathBuf,
}

impl DirTemplate {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}