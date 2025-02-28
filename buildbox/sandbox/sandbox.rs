use super::{DentryTemplate, DirTemplate, FileTemplate, SandboxTemplate, SymlinkTemplate};
use crate::{ExecCommand, ExecResult, GeneratedFile};
use common::{rand, Error, Result};
use proto::bazel::exec::Digest;
use std::fs::{self, OpenOptions};
use std::io::{BufReader, Cursor, ErrorKind, Write};
use std::ops::Drop;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use storage::Storage;

/// Executes build actions within local directories.
///
/// The local sandbox implementation does not perform any isolation. It just
/// runs within a temporary directory. This means the entire host environment
/// will be made available to whatever command is executed.
#[derive(Debug, Clone)]
pub struct Sandbox {
    dir: PathBuf,
    storage: Storage,
    retain: bool,
}

impl Sandbox {
    /// Create a [`Sandbox`] instance for local execution.
    pub fn new(dir: PathBuf, storage: Storage, retain: bool) -> Self {
        Self {
            dir,
            storage,
            retain,
        }
    }

    /// Construct a new action execution environment.
    pub fn spawn(&self, template: &SandboxTemplate) -> Result<SandboxHandle> {
        let id = self.generate_id();
        let path = self.local_path(&id);
        tracing::info!("Creating sandbox at: {path:?}");

        std::fs::create_dir(&path).map_err(Error::io)?;
        tracing::info!("Created dir: {path:?}");

        Ok(SandboxHandle {
            dir: path,
            storage: self.storage.clone(),
            template: template.clone(),
            retain: self.retain,
        })
    }

    fn generate_id(&self) -> String {
        format!("sandbox-{}", rand::string(10))
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
    storage: Storage,
    template: SandboxTemplate,
    retain: bool,
}

impl SandboxHandle {
    /// Prepare the sandbox according to the template it was created with.
    pub fn prepare(&self) -> Result<()> {
        tracing::info!("Sandbox::prepare {:?}", self.template);

        for template in &self.template.filesystem {
            tracing::info!("preparing: {template:?}");
            let res = match template {
                DentryTemplate::File(file) => self.prepare_file(&file),
                DentryTemplate::Symlink(symlink) => self.prepare_symlink(&symlink),
                DentryTemplate::Dir(dir) => self.prepare_dir(&dir),
            };

            if let Err(err) = res {
                tracing::error!("failed to prepare template: {err:?}");
                return Err(err);
            }
        }

        Ok(())
    }

    fn prepare_file(&self, tpl: &FileTemplate) -> Result<()> {
        let path = self.relative_path(&tpl.path);
        tracing::info!("Preparing file: {path:?}");

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .open(path)
            .map_err(Error::io)?;

        let mut reader = self.storage.read_digest(&tpl.digest)?;
        std::io::copy(&mut reader, &mut file).map_err(Error::io)?;
        file.flush().map_err(Error::io)?;

        if tpl.executable {
            let mut perms = file.metadata().map_err(Error::io)?.permissions();
            perms.set_mode(0o777);
            file.set_permissions(perms).map_err(Error::io)?;
        }

        Ok(())
    }

    fn prepare_symlink(&self, symlink: &SymlinkTemplate) -> Result<()> {
        use std::os::unix::fs;
        let from = self.relative_path(&symlink.path);
        let to = self.relative_path(&symlink.target);
        fs::symlink(from, to).map_err(Error::io)
    }

    fn prepare_dir(&self, dir: &DirTemplate) -> Result<()> {
        let path = self.relative_path(&dir.path);
        if path.exists() {
            return Ok(());
        }

        fs::create_dir(path).map_err(Error::io)
    }

    /// Execute the given command.
    pub fn exec(&self, exec_cmd: &ExecCommand) -> Result<ExecResult> {
        tracing::info!("Sandbox::exec {exec_cmd:?}");

        // These are required environment variables on MacOS. Hardcode them for
        // testing.
        let mut envs = exec_cmd.env.clone();
        envs.insert(
            "DEVELOPER_DIR".to_string(),
            "/Library/Developer/CommandLineTools".to_string(),
        );
        envs.insert(
            "SDKROOT".to_string(),
            "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk".to_string(),
        );

        // For some reason the wrapped_clang script Bazel uses requires that the
        // output files are created before they are written to?
        for rel_path in &exec_cmd.outputs {
            let path = PathBuf::from(rel_path);
            let sandbox_path = self.relative_path(&path);
            tracing::info!("Creating parent directory for: {sandbox_path:?}");
            if let Some(parent) = sandbox_path.parent() {
                tracing::info!("actually creating parent");
                fs::create_dir_all(parent).map_err(|err| {
                    tracing::error!("failed to create parent: {err:?}");
                    Error::io(err)
                })?;
            }
        }

        let res = Command::new(&exec_cmd.args[0])
            .current_dir(&self.dir)
            .args(&exec_cmd.args[1..])
            .envs(&envs)
            .output();

        let output = res.map_err(|err| {
            tracing::error!("Failed to run command: {err:?}");
            Error::io(err)
        })?;

        let exit_code = output.status.code().unwrap_or(-1);
        tracing::info!("command finished with exit code {exit_code}");

        let mut outputs = vec![];
        for rel_path in &exec_cmd.outputs {
            let path = self.relative_path(&PathBuf::from(&rel_path));
            if !path.exists() {
                continue;
            }

            let file = OpenOptions::new()
                .read(true)
                .open(path)
                .map_err(Error::io)?;

            outputs.push(GeneratedFile {
                path: PathBuf::from(&rel_path),
                digest: self.storage.write(file)?,
            });
        }

        let stdout = {
            let cursor = Cursor::new(&output.stdout);
            let mut reader = BufReader::new(cursor);
            self.storage.write(reader)
        }?;

        let stderr = {
            let cursor = Cursor::new(&output.stderr);
            let mut reader = BufReader::new(cursor);
            self.storage.write(reader)
        }?;

        Ok(ExecResult {
            exit_code,
            outputs,
            stdout,
            stderr,
        })
    }

    fn relative_path(&self, path: &PathBuf) -> PathBuf {
        let mut rel = self.dir.clone();
        rel.push(path);
        rel
    }
}

impl Drop for SandboxHandle {
    fn drop(&mut self) {
        if self.retain {
            return;
        }

        if let Err(err) = std::fs::remove_dir_all(&self.dir) {
            tracing::error!("failed to remove sandbox: {err:?}");
        }
    }
}
