//! A [`Runner`] that runs commands directly on this machine.

use async_trait::async_trait;
use executor::ExecuteError;
use executor_sandboxed::{ProcessOutput, RunRequest, Runner};
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

        let invocation = Invocation {
            program: resolve_program(program, &working_dir),
            args: args.to_vec(),
            env,
            working_dir,
        };
        process::run(&invocation, request.timeout).await.map_err(execute_error)
    }
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
}
