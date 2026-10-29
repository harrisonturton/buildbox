//! A [`Runner`] that runs commands directly on this machine.

use async_trait::async_trait;
use executor::ExecuteError;
use executor_sandboxed::{ProcessOutput, RunRequest, Runner, StartedProcess};
use process::{Invocation, ProcessError};
use std::path::{Path, PathBuf};

/// Runs each command directly on the host, in its sandbox directory.
///
/// There is no isolation: commands can see and modify the whole host, and
/// see whatever tools are installed on it. A command gets exactly the
/// environment variables it specifies.
pub struct HostRunner;

#[async_trait]
impl Runner for HostRunner {
    async fn run(&self, request: &RunRequest<'_>) -> Result<ProcessOutput, ExecuteError> {
        let invocation = invocation(request)?;
        process::run(&invocation, request.timeout).await.map_err(execute_error)
    }

    async fn start(&self, request: &RunRequest<'_>) -> Result<StartedProcess, ExecuteError> {
        let invocation = invocation(request)?;
        let process = process::start(&invocation).await.map_err(execute_error)?;
        Ok(StartedProcess { process, keep: Box::new(()) })
    }
}

/// The request's command, run in its working directory with exactly its
/// environment.
fn invocation(request: &RunRequest<'_>) -> Result<Invocation, ExecuteError> {
    let Some((program, args)) = request.command.arguments.split_first() else {
        return Err(ExecuteError::InvalidAction("command has no arguments".to_string()));
    };
    let working_dir = request.sandbox.join(request.working_dir);
    let env = request
        .command
        .environment_variables
        .iter()
        .map(|variable| (variable.name.clone(), variable.value.clone()))
        .collect();
    Ok(Invocation {
        program: resolve_program(program, &working_dir),
        args: args.to_vec(),
        env,
        working_dir,
    })
}

/// A program given as a relative path (containing a slash) is relative to the
/// working directory. One given as a bare name is looked up in `PATH`.
fn resolve_program(program: &str, working_dir: &Path) -> PathBuf {
    let path = Path::new(program);
    if program.contains('/') && path.is_relative() {
        return working_dir.join(path);
    }
    path.to_path_buf()
}

fn execute_error(error: ProcessError) -> ExecuteError {
    match error {
        ProcessError::Timeout(timeout) => ExecuteError::Timeout(timeout),
        // The program is part of the action, so failing to start it is the
        // action's fault.
        error @ ProcessError::Start { .. } => ExecuteError::InvalidAction(error.to_string()),
        error @ ProcessError::Io { .. } => ExecuteError::Internal(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote_execution_rust_connect::build::bazel::remote::execution::v2::command::EnvironmentVariable;
    use remote_execution_rust_connect::build::bazel::remote::execution::v2::Command;
    use std::time::Duration;

    async fn run(command: &Command, sandbox: &Path, timeout: Option<Duration>) -> Result<ProcessOutput, ExecuteError> {
        let request = RunRequest {
            command,
            sandbox,
            working_dir: Path::new(""),
            timeout,
        };
        HostRunner.run(&request).await
    }

    #[tokio::test]
    async fn runs_with_the_commands_environment() {
        let dir = tempfile::tempdir().unwrap();
        let command = Command {
            arguments: vec!["/bin/sh".to_string(), "-c".to_string(), "echo \"$GREETING\"".to_string()],
            environment_variables: vec![EnvironmentVariable {
                name: "GREETING".to_string(),
                value: "hello".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let output = run(&command, dir.path(), None).await.unwrap();

        assert_eq!(output.stdout, b"hello\n");
    }

    #[tokio::test]
    async fn resolves_relative_programs_against_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("script.sh");
        std::fs::write(&script, b"#!/bin/sh\necho ran\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let command = Command {
            arguments: vec!["./script.sh".to_string()],
            ..Default::default()
        };

        let output = run(&command, dir.path(), None).await.unwrap();

        assert_eq!(output.stdout, b"ran\n");
    }

    #[tokio::test]
    async fn reports_timeouts() {
        let dir = tempfile::tempdir().unwrap();
        let command = Command {
            arguments: vec!["/bin/sleep".to_string(), "10".to_string()],
            ..Default::default()
        };
        let timeout = Duration::from_millis(100);

        let err = run(&command, dir.path(), Some(timeout)).await.unwrap_err();

        assert!(matches!(err, ExecuteError::Timeout(t) if t == timeout), "{err}");
    }

    #[tokio::test]
    async fn rejects_programs_that_cant_start() {
        let dir = tempfile::tempdir().unwrap();
        let command = Command {
            arguments: vec!["./missing".to_string()],
            ..Default::default()
        };

        let err = run(&command, dir.path(), None).await.unwrap_err();

        assert!(matches!(err, ExecuteError::InvalidAction(_)), "{err}");
    }

    #[tokio::test]
    async fn rejects_empty_commands() {
        let dir = tempfile::tempdir().unwrap();

        let err = run(&Command::default(), dir.path(), None).await.unwrap_err();

        assert!(matches!(err, ExecuteError::InvalidAction(_)), "{err}");
    }

    /// A worker speaking the JSON protocol, which counts the requests it
    /// gets and writes the count to the output its request names.
    const JSON_WORKER: &str = r#"
import json, sys
assert sys.argv[-1] == "--persistent_worker", sys.argv
count = 0
for line in sys.stdin:
    request = json.loads(line)
    count += 1
    out = request["arguments"][0]
    with open(out, "w") as f:
        f.write(str(count))
    print(json.dumps({"exitCode": 0, "output": "made " + out}), flush=True)
"#;

    /// An action run by the JSON worker, writing `output`, whose flagfile
    /// names it.
    async fn worker_action(cas: &cas_mem::MemCas, output: &str) -> executor::Action {
        use buffa::Message;
        use cas::Cas;
        use remote_execution_rust_connect::build::bazel::remote::execution::v2::{Directory, FileNode};
        let worker = cas.put_bytes(JSON_WORKER.as_bytes()).await.unwrap();
        let flagfile = cas.put_bytes(format!("{output}\n").as_bytes()).await.unwrap();
        let file = |name: &str, digest: &cas::Digest| FileNode {
            name: name.to_string(),
            digest: digest.to_proto().into(),
            ..Default::default()
        };
        let root = Directory {
            files: vec![file("args.txt", &flagfile), file("worker.py", &worker)],
            ..Default::default()
        };
        let input_root = cas.put_bytes(&root.encode_to_vec()).await.unwrap();
        let command = Command {
            arguments: vec!["python3".to_string(), "worker.py".to_string(), "@args.txt".to_string()],
            environment_variables: vec![EnvironmentVariable {
                name: "PATH".to_string(),
                value: "/usr/bin:/bin".to_string(),
                ..Default::default()
            }],
            output_files: vec![output.to_string()],
            ..Default::default()
        };
        executor::Action {
            digest: cas::Digest::of_bytes(output.as_bytes()),
            command,
            input_root,
            timeout: Some(Duration::from_secs(60)),
            platform: vec![
                ("persistentWorkerKey".to_string(), "tools".to_string()),
                ("persistentWorkerProtocol".to_string(), "json".to_string()),
            ],
        }
    }

    #[tokio::test]
    async fn runs_actions_in_one_persistent_worker() {
        use cas::Cas;
        use executor::Executor;
        let root = tempfile::tempdir().unwrap();
        let cas = std::sync::Arc::new(cas_mem::MemCas::default());
        let executor = executor_sandboxed::SandboxedExecutor::new(cas.clone(), root.path().to_path_buf(), false, std::sync::Arc::new(HostRunner), "test".to_string());
        for (output, count) in [("first.txt", "1"), ("second.txt", "2")] {
            let action = worker_action(&cas, output).await;
            let result = executor.execute(&action).await.unwrap();
            assert_eq!(result.exit_code, 0);
            let stderr = cas::Digest::from_required_proto(result.stderr_digest.as_option()).unwrap();
            assert_eq!(cas.read_bytes(&stderr).await.unwrap(), format!("made {output}").into_bytes());
            let file = &result.output_files[0];
            assert_eq!(file.path, output);
            let digest = cas::Digest::from_required_proto(file.digest.as_option()).unwrap();
            assert_eq!(cas.read_bytes(&digest).await.unwrap(), count.as_bytes(), "the second action runs in the first's worker");
        }
    }
}
