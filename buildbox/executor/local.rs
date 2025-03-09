use super::{Executor, SandboxHandle};
use crate::{DentryTemplate, DirTemplate, FileTemplate, SandboxTemplate, SymlinkTemplate};
use crate::{ExecCommand, ExecResult, OutputDir, OutputFile};
use common::{rand, Error, Result};
use proto::bazel::exec::{Digest, OutputDirectory, Tree};
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{BufReader, Cursor, ErrorKind, Write};
use std::ops::Drop;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread::current;
use storage::{ProtoStoreExt, Store};

/// Executes build actions within local directories.
///
/// The local sandbox implementation does not perform any isolation. It just
/// runs within a temporary directory. This means the entire host environment
/// will be made available to whatever command is executed.
#[derive(Debug, Clone)]
pub struct LocalExecutor<S: Store> {
    dir: PathBuf,
    storage: S,
    retain: bool,
}

impl<S: Store> LocalExecutor<S> {
    /// Create a [`Executor`] instance for local execution.
    pub fn new(dir: PathBuf, storage: S, retain: bool) -> Self {
        Self {
            dir,
            storage,
            retain,
        }
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

impl<S: Store> Executor for LocalExecutor<S> {
    type Handle = LocalSandbox<S>;

    /// Construct a new action execution environment.
    fn spawn(&self, template: &SandboxTemplate) -> Result<Self::Handle> {
        let id = self.generate_id();
        let path = self.local_path(&id);

        std::fs::create_dir(&path).map_err(Error::io)?;
        tracing::trace!("sandbox directory: {path:?}");

        Ok(LocalSandbox {
            dir: path,
            storage: self.storage.clone(),
            template: template.clone(),
            retain: self.retain,
        })
    }
}

/// A handle to the constructed action execution environment.
#[derive(Debug)]
pub struct LocalSandbox<S: Store> {
    dir: PathBuf,
    storage: S,
    template: SandboxTemplate,
    retain: bool,
}

impl<S: Store> LocalSandbox<S> {
    fn prepare_file(&self, tpl: &FileTemplate) -> Result<()> {
        let path = self.absolute_path(&tpl.path);

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
        let from = self.absolute_path(&symlink.path);
        let to = self.absolute_path(&symlink.target);
        fs::symlink(from, to).map_err(Error::io)
    }

    fn prepare_dir(&self, dir: &DirTemplate) -> Result<()> {
        let path = self.absolute_path(&dir.path);
        if path.exists() {
            return Ok(());
        }

        fs::create_dir(path).map_err(Error::io)
    }

    fn absolute_path(&self, path: &PathBuf) -> PathBuf {
        let mut rel = self.dir.clone();
        rel.push(path);
        rel
    }

    fn gather_output_directory(&self, cmd: &ExecCommand, path: &str) -> Result<Tree> {
        let rel_path = PathBuf::from(path);
        let abs_path = self.absolute_path(&rel_path);

        if !abs_path.exists() {
            return Err(Error::not_found("output directory not found"));
        }

        super::tree::build_tree(&self.storage, &self.dir, &rel_path)
    }
}

impl<S: Store> SandboxHandle for LocalSandbox<S> {
    /// Prepare the sandbox according to the template it was created with.
    fn prepare(&self) -> Result<()> {
        for template in &self.template.filesystem {
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

    /// Execute the given command.
    fn exec(&self, exec_cmd: &ExecCommand) -> Result<ExecResult> {
        tracing::trace!("sandbox executing command \"{}\"", exec_cmd.args.join(" "));

        // These are required environment variables on MacOS. Hardcode them for
        // testing.
        let mut envs = exec_cmd.env.clone();

        let path = std::env::var("PATH").unwrap();
        let new_path = format!("{path}:/opt/homebrew/opt/llvm/bin");

        envs.insert(
            "DEVELOPER_DIR".to_string(),
            "/Library/Developer/CommandLineTools".to_string(),
        );
        envs.insert(
            "SDKROOT".to_string(),
            "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk".to_string(),
        );
        envs.insert("PATH".to_string(), path);

        // For some reason the wrapped_clang script Bazel uses requires that the
        // output files are created before they are written to?
        for rel_path in &exec_cmd.output_files {
            let path = PathBuf::from(rel_path);
            let sandbox_path = self.absolute_path(&path);
            if let Some(parent) = sandbox_path.parent() {
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

        let exit_code = output.status.code().unwrap_or(1);
        tracing::info!("command finished with exit code {:?}", output.status.code());

        let mut output_files = vec![];
        for rel_path in &exec_cmd.output_files {
            let path = self.absolute_path(&PathBuf::from(&rel_path));
            if !path.exists() {
                continue;
            }

            let file = OpenOptions::new()
                .read(true)
                .open(path)
                .map_err(Error::io)?;

            output_files.push(OutputFile {
                path: PathBuf::from(&rel_path),
                digest: self.storage.write_digest(file)?,
            });
        }

        let mut output_dirs = vec![];
        for path in &exec_cmd.output_dirs {
            let tree = match self.gather_output_directory(&exec_cmd, &path) {
                Ok(tree) => tree,
                Err(Error::NotFound(_)) => continue,
                Err(err) => return Err(err),
            };

            let digest = self.storage.write_message(&tree)?;
            output_dirs.push(OutputDirectory {
                path: path.to_owned(),
                tree_digest: Some(digest),
            });
        }

        if exit_code == 1 {
            tracing::error!("stdout: {}", String::from_utf8_lossy(&output.stdout));
            tracing::error!("stderr: {}", String::from_utf8_lossy(&output.stderr));
        }

        let stdout = {
            let cursor = Cursor::new(&output.stdout);
            let mut reader = BufReader::new(cursor);
            self.storage.write_digest(reader)
        }?;

        let stderr = {
            let cursor = Cursor::new(&output.stderr);
            let mut reader = BufReader::new(cursor);
            self.storage.write_digest(reader)
        }?;

        Ok(ExecResult {
            exit_code,
            output_files,
            output_dirs,
            stdout,
            stderr,
        })
    }
}

impl<S: Store> Drop for LocalSandbox<S> {
    fn drop(&mut self) {
        if self.retain {
            return;
        }

        if let Err(err) = std::fs::remove_dir_all(&self.dir) {
            tracing::error!("failed to remove sandbox: {err:?}");
        }
    }
}
