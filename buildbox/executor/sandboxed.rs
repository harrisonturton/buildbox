//! An [`Executor`] that runs each action in a directory on this machine,
//! populated with the action's inputs. How the command itself is run, e.g.
//! directly on the host or in a container, is up to a [`Runner`].

mod inputs;
mod outputs;
mod paths;
mod sandbox;

pub use process::ProcessOutput;

use async_trait::async_trait;
use buffa_types::google::protobuf::Timestamp;
use cas::{Cas, Digest};
use executor::{Action, ExecuteError, Executor};
use remote_execution_rust_connect::build::bazel::remote::execution::v2::{
    ActionResult, Command, ExecutedActionMetadata,
};
use sandbox::Sandbox;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Runs an action's command in a sandbox that has been populated with the
/// action's inputs.
#[async_trait]
pub trait Runner: Send + Sync + 'static {
    /// Run the command to completion. A command that runs and exits with a
    /// non-zero code has still run successfully.
    ///
    /// Dropping the returned future stops the command.
    async fn run(&self, request: &RunRequest<'_>) -> Result<ProcessOutput, ExecuteError>;
}

/// A command to run, and where.
#[derive(Debug)]
pub struct RunRequest<'a> {
    pub command: &'a Command,

    /// The sandbox directory on this machine, holding the action's inputs.
    pub sandbox: &'a Path,

    /// The directory to run in, relative to the sandbox. It exists, and has
    /// been checked not to escape the sandbox.
    pub working_dir: &'a Path,

    /// How long the command may run before it is stopped.
    pub timeout: Option<Duration>,
}

/// Runs each action in its own sandbox directory, with a [`Runner`].
pub struct SandboxedExecutor {
    cas: Arc<dyn Cas>,
    sandbox_root: PathBuf,
    keep_sandboxes: bool,
    runner: Arc<dyn Runner>,
    worker_name: String,
    directories: inputs::DirectoryCache,
}

impl SandboxedExecutor {
    /// Create an executor that makes sandboxes inside `sandbox_root`, which
    /// must exist. If `keep_sandboxes` is set they aren't deleted after use,
    /// for debugging. `worker_name` is reported in each result's metadata.
    pub fn new(
        cas: Arc<dyn Cas>,
        sandbox_root: PathBuf,
        keep_sandboxes: bool,
        runner: Arc<dyn Runner>,
        worker_name: String,
    ) -> SandboxedExecutor {
        SandboxedExecutor {
            cas,
            sandbox_root,
            keep_sandboxes,
            runner,
            worker_name,
            directories: inputs::DirectoryCache::default(),
        }
    }

    /// Store the command's stdout and stderr in the CAS, returning their
    /// digests.
    #[tracing::instrument(skip_all)]
    async fn store_output_streams(&self, output: &ProcessOutput) -> Result<(Digest, Digest), ExecuteError> {
        let stdout = self.cas.put_bytes(&output.stdout).await?;
        let stderr = self.cas.put_bytes(&output.stderr).await?;
        Ok((stdout, stderr))
    }
}

/// Create the directory sandboxes are made in if needed and, unless
/// sandboxes are being kept, delete any left over from a previous run. Call
/// this once, before creating executors that share the directory.
pub async fn prepare_sandbox_root(sandbox_root: &Path, keep_sandboxes: bool) -> std::io::Result<()> {
    let root = sandbox_root.to_path_buf();
    fsutil::run_blocking(move || {
        if keep_sandboxes {
            std::fs::create_dir_all(&root)
        } else {
            sandbox::prepare_root(&root)
        }
    })
    .await
}

#[async_trait]
impl Executor for SandboxedExecutor {
    #[tracing::instrument(name = "run_action", skip_all, fields(action = %action.digest))]
    async fn execute(&self, action: &Action) -> Result<ActionResult, ExecuteError> {
        let command = &action.command;
        let working_dir = paths::parse_relative_path(&command.working_directory).map_err(ExecuteError::InvalidAction)?;

        let sandbox = Sandbox::create(&self.sandbox_root, self.keep_sandboxes).await?;
        tracing::info!("running action {} in {}", action.digest, sandbox.path().display());

        inputs::stage_inputs(&*self.cas, &self.directories, &action.input_root, sandbox.path()).await?;
        let host_working_dir = sandbox.path().join(&working_dir);
        tokio::fs::create_dir_all(&host_working_dir).await.map_err(|error| {
            ExecuteError::Internal(format!("failed to create {}: {error}", host_working_dir.display()))
        })?;
        outputs::create_output_parents(command, &host_working_dir).await?;

        let request = RunRequest {
            command,
            sandbox: sandbox.path(),
            working_dir: &working_dir,
            timeout: action.timeout,
        };
        let start = SystemTime::now();
        let process_output = self.runner.run(&request).await?;
        let completed = SystemTime::now();

        let outputs = outputs::collect_outputs(&*self.cas, command, &host_working_dir).await?;
        let (stdout, stderr) = self.store_output_streams(&process_output).await?;

        let metadata = ExecutedActionMetadata {
            worker: self.worker_name.clone(),
            execution_start_timestamp: Timestamp::from(start).into(),
            execution_completed_timestamp: Timestamp::from(completed).into(),
            ..Default::default()
        };
        Ok(ActionResult {
            output_files: outputs.files,
            output_directories: outputs.directories,
            exit_code: process_output.exit_code,
            stdout_digest: stdout.to_proto().into(),
            stderr_digest: stderr.to_proto().into(),
            execution_metadata: metadata.into(),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffa::Message;
    use cas_mem::MemCas;
    use remote_execution_rust_connect::build::bazel::remote::execution::v2::{Directory, FileNode};

    /// Runs commands with `sh -c` on the host, with no environment.
    struct ShellRunner;

    #[async_trait]
    impl Runner for ShellRunner {
        async fn run(&self, request: &RunRequest<'_>) -> Result<ProcessOutput, ExecuteError> {
            let invocation = process::Invocation {
                program: PathBuf::from("/bin/sh"),
                args: vec!["-c".to_string(), request.command.arguments.join(" ")],
                env: vec![],
                working_dir: request.sandbox.join(request.working_dir),
            };
            process::run(&invocation, request.timeout)
                .await
                .map_err(|error| ExecuteError::Internal(error.to_string()))
        }
    }

    fn executor(cas: &Arc<MemCas>, sandboxes: &Path, keep_sandboxes: bool) -> SandboxedExecutor {
        let runner = Arc::new(ShellRunner);
        SandboxedExecutor::new(cas.clone(), sandboxes.to_path_buf(), keep_sandboxes, runner, "test".to_string())
    }

    async fn action_with_input(cas: &MemCas, script: &str, output_files: &[&str]) -> Action {
        let input = cas.put_bytes(b"input data").await.unwrap();
        let root = Directory {
            files: vec![FileNode {
                name: "input.txt".to_string(),
                digest: input.to_proto().into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let input_root = cas.put_bytes(&root.encode_to_vec()).await.unwrap();
        let command = Command {
            arguments: vec![script.to_string()],
            output_files: output_files.iter().map(|path| path.to_string()).collect(),
            ..Default::default()
        };

        Action {
            digest: Digest::of_bytes(b"action"),
            command,
            input_root,
            timeout: None,
            platform: vec![],
        }
    }

    #[tokio::test]
    async fn runs_action_and_collects_outputs() {
        let cas = Arc::new(MemCas::new());
        let sandboxes = tempfile::tempdir().unwrap();
        let executor = executor(&cas, sandboxes.path(), false);
        let action = action_with_input(&cas, "mkdir -p out && cp input.txt out/copy.txt && echo done", &["out/copy.txt"]).await;

        let result = executor.execute(&action).await.unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output_files[0].path, "out/copy.txt");
        assert_eq!(result.output_files[0].digest.as_option(), Some(&Digest::of_bytes(b"input data").to_proto()));
        let stdout = Digest::from_required_proto(result.stdout_digest.as_option()).unwrap();
        assert_eq!(cas.read_bytes(&stdout).await.unwrap(), b"done\n");
        assert_eq!(result.execution_metadata.as_option().unwrap().worker, "test");
    }

    #[tokio::test]
    async fn creates_output_parent_directories() {
        let cas = Arc::new(MemCas::new());
        let sandboxes = tempfile::tempdir().unwrap();
        let executor = executor(&cas, sandboxes.path(), false);
        let action = action_with_input(&cas, "echo hi > a/b/out.txt", &["a/b/out.txt"]).await;

        let result = executor.execute(&action).await.unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output_files.len(), 1);
    }

    #[tokio::test]
    async fn reports_failing_actions_in_the_result() {
        let cas = Arc::new(MemCas::new());
        let sandboxes = tempfile::tempdir().unwrap();
        let executor = executor(&cas, sandboxes.path(), false);
        let action = action_with_input(&cas, "echo broken >&2; exit 1", &[]).await;

        let result = executor.execute(&action).await.unwrap();

        assert_eq!(result.exit_code, 1);
        let stderr = Digest::from_required_proto(result.stderr_digest.as_option()).unwrap();
        assert_eq!(cas.read_bytes(&stderr).await.unwrap(), b"broken\n");
    }

    #[tokio::test]
    async fn removes_sandboxes_unless_kept() {
        let cas = Arc::new(MemCas::new());
        let sandboxes = tempfile::tempdir().unwrap();
        let executor = executor(&cas, sandboxes.path(), false);
        let action = action_with_input(&cas, "true", &[]).await;

        executor.execute(&action).await.unwrap();

        // Removal happens in the background, so wait for it.
        for _ in 0..100 {
            if std::fs::read_dir(sandboxes.path()).unwrap().count() == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("sandbox wasn't removed");
    }

    #[tokio::test]
    async fn prepare_sandbox_root_removes_leftover_sandboxes_only() {
        let sandboxes = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(sandboxes.path().join("action-old/nested")).unwrap();
        std::fs::write(sandboxes.path().join("unrelated"), b"keep me").unwrap();

        prepare_sandbox_root(sandboxes.path(), false).await.unwrap();

        assert!(!sandboxes.path().join("action-old").exists());
        assert!(sandboxes.path().join("unrelated").exists());
    }

    #[tokio::test]
    async fn keeps_sandboxes_when_asked() {
        let cas = Arc::new(MemCas::new());
        let sandboxes = tempfile::tempdir().unwrap();
        let executor = executor(&cas, sandboxes.path(), true);
        let action = action_with_input(&cas, "true", &[]).await;

        executor.execute(&action).await.unwrap();

        assert_eq!(std::fs::read_dir(sandboxes.path()).unwrap().count(), 1);
    }
}
