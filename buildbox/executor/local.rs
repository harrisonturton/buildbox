//! An [`Executor`] that runs actions directly on this machine.

mod inputs;
mod outputs;
mod paths;
mod process;
mod sandbox;

use async_trait::async_trait;
use cas::Cas;
use executor::{Action, ExecuteError, Executor};
use remote_execution_proto::build::bazel::remote::execution::v2::{ActionResult, ExecutedActionMetadata};
use remote_execution_proto::timestamp_proto::google::protobuf::Timestamp;
use sandbox::Sandbox;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const WORKER_NAME: &str = "buildbox-local";

/// Runs each action in its own directory, populated with the action's inputs.
///
/// There is no isolation: actions can see and modify the whole host, and see
/// whatever tools are installed on it.
pub struct LocalExecutor {
    cas: Arc<dyn Cas>,
    sandbox_root: PathBuf,
    keep_sandboxes: bool,
}

impl LocalExecutor {
    /// Create an executor that makes sandboxes inside `sandbox_root`. If
    /// `keep_sandboxes` is set they aren't deleted after use, for debugging.
    pub fn new(cas: Arc<dyn Cas>, sandbox_root: PathBuf, keep_sandboxes: bool) -> LocalExecutor {
        LocalExecutor {
            cas,
            sandbox_root,
            keep_sandboxes,
        }
    }
}

#[async_trait]
impl Executor for LocalExecutor {
    async fn execute(&self, action: &Action) -> Result<ActionResult, ExecuteError> {
        let command = &action.command;
        let working_dir = paths::parse_relative_path(&command.working_directory).map_err(ExecuteError::InvalidAction)?;

        let sandbox = Sandbox::create(&self.sandbox_root, self.keep_sandboxes).await?;
        tracing::info!("running action {} in {}", action.digest, sandbox.path().display());

        inputs::stage_inputs(&*self.cas, &action.input_root, sandbox.path()).await?;
        let working_dir = sandbox.path().join(working_dir);
        tokio::fs::create_dir_all(&working_dir).await.map_err(|error| {
            ExecuteError::Internal(format!("failed to create {}: {error}", working_dir.display()))
        })?;
        outputs::create_output_parents(command, &working_dir).await?;

        let start = SystemTime::now();
        let process_output = process::run(command, &working_dir, action.timeout).await?;
        let completed = SystemTime::now();

        let outputs = outputs::collect_outputs(&*self.cas, command, &working_dir).await?;
        let stdout = self.cas.put_bytes(&process_output.stdout).await?;
        let stderr = self.cas.put_bytes(&process_output.stderr).await?;

        let metadata = ExecutedActionMetadata {
            worker: WORKER_NAME.to_string(),
            execution_start_timestamp: Some(timestamp(start)),
            execution_completed_timestamp: Some(timestamp(completed)),
            ..Default::default()
        };
        Ok(ActionResult {
            output_files: outputs.files,
            output_directories: outputs.directories,
            exit_code: process_output.exit_code,
            stdout_digest: Some(stdout.to_proto()),
            stderr_digest: Some(stderr.to_proto()),
            execution_metadata: Some(metadata),
            ..Default::default()
        })
    }
}

fn timestamp(time: SystemTime) -> Timestamp {
    // A clock set before 1970 is reported as 1970.
    let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    Timestamp {
        seconds: since_epoch.as_secs() as i64,
        nanos: since_epoch.subsec_nanos() as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cas::Digest;
    use cas_mem::MemCas;
    use prost::Message;
    use remote_execution_proto::build::bazel::remote::execution::v2::{Command, Directory, FileNode};

    async fn action_with_input(cas: &MemCas, script: &str, output_files: &[&str]) -> Action {
        let input = cas.put_bytes(b"input data").await.unwrap();
        let root = Directory {
            files: vec![FileNode {
                name: "input.txt".to_string(),
                digest: Some(input.to_proto()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let input_root = cas.put_bytes(&root.encode_to_vec()).await.unwrap();
        let command = Command {
            arguments: vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
            output_files: output_files.iter().map(|path| path.to_string()).collect(),
            ..Default::default()
        };

        Action {
            digest: Digest::of_bytes(b"action"),
            command,
            input_root,
            timeout: None,
        }
    }

    #[tokio::test]
    async fn runs_action_and_collects_outputs() {
        let cas = Arc::new(MemCas::new());
        let sandboxes = tempfile::tempdir().unwrap();
        let executor = LocalExecutor::new(cas.clone(), sandboxes.path().to_path_buf(), false);
        let action = action_with_input(&cas, "mkdir -p out && cp input.txt out/copy.txt && echo done", &["out/copy.txt"]).await;

        let result = executor.execute(&action).await.unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output_files[0].path, "out/copy.txt");
        assert_eq!(result.output_files[0].digest, Some(Digest::of_bytes(b"input data").to_proto()));
        let stdout = Digest::from_required_proto(result.stdout_digest.as_ref()).unwrap();
        assert_eq!(cas.read_bytes(&stdout).await.unwrap(), b"done\n");
    }

    #[tokio::test]
    async fn creates_output_parent_directories() {
        let cas = Arc::new(MemCas::new());
        let sandboxes = tempfile::tempdir().unwrap();
        let executor = LocalExecutor::new(cas.clone(), sandboxes.path().to_path_buf(), false);
        let action = action_with_input(&cas, "echo hi > a/b/out.txt", &["a/b/out.txt"]).await;

        let result = executor.execute(&action).await.unwrap();

        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output_files.len(), 1);
    }

    #[tokio::test]
    async fn reports_failing_actions_in_the_result() {
        let cas = Arc::new(MemCas::new());
        let sandboxes = tempfile::tempdir().unwrap();
        let executor = LocalExecutor::new(cas.clone(), sandboxes.path().to_path_buf(), false);
        let action = action_with_input(&cas, "echo broken >&2; exit 1", &[]).await;

        let result = executor.execute(&action).await.unwrap();

        assert_eq!(result.exit_code, 1);
        let stderr = Digest::from_required_proto(result.stderr_digest.as_ref()).unwrap();
        assert_eq!(cas.read_bytes(&stderr).await.unwrap(), b"broken\n");
    }

    #[tokio::test]
    async fn removes_sandboxes_unless_kept() {
        let cas = Arc::new(MemCas::new());
        let sandboxes = tempfile::tempdir().unwrap();
        let executor = LocalExecutor::new(cas.clone(), sandboxes.path().to_path_buf(), false);
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
    async fn keeps_sandboxes_when_asked() {
        let cas = Arc::new(MemCas::new());
        let sandboxes = tempfile::tempdir().unwrap();
        let executor = LocalExecutor::new(cas.clone(), sandboxes.path().to_path_buf(), true);
        let action = action_with_input(&cas, "true", &[]).await;

        executor.execute(&action).await.unwrap();

        assert_eq!(std::fs::read_dir(sandboxes.path()).unwrap().count(), 1);
    }
}
