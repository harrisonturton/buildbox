//! Running an action's command.

use executor::ExecuteError;
use remote_execution_proto::build::bazel::remote::execution::v2::Command;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

/// What a finished command produced.
#[derive(Debug)]
pub struct ProcessOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run `command` in `working_dir` and wait for it to finish.
///
/// The command gets exactly the environment variables it specifies. If it
/// runs longer than `timeout` it is killed.
pub async fn run(command: &Command, working_dir: &Path, timeout: Option<Duration>) -> Result<ProcessOutput, ExecuteError> {
    let Some((program, args)) = command.arguments.split_first() else {
        return Err(ExecuteError::InvalidAction("command has no arguments".to_string()));
    };
    let program = resolve_program(program, working_dir);

    let mut process = tokio::process::Command::new(&program);
    process
        .args(args)
        .current_dir(working_dir)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Stop the process if the action is abandoned, e.g. on timeout.
        .kill_on_drop(true);
    for variable in &command.environment_variables {
        process.env(&variable.name, &variable.value);
    }

    let child = process.spawn().map_err(|error| {
        ExecuteError::InvalidAction(format!("failed to start {}: {error}", program.display()))
    })?;

    let output = match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(output) => output,
            Err(_elapsed) => return Err(ExecuteError::Timeout(timeout)),
        },
        None => child.wait_with_output().await,
    };
    let output = output.map_err(|error| ExecuteError::Internal(format!("failed to wait for command: {error}")))?;

    Ok(ProcessOutput {
        exit_code: exit_code(output.status),
        stdout: output.stdout,
        stderr: output.stderr,
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

/// The exit code, or for a process killed by a signal, 128 plus the signal
/// number, as shells report it.
fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    match status.signal() {
        Some(signal) => 128 + signal,
        None => -1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote_execution_proto::build::bazel::remote::execution::v2::command::EnvironmentVariable;

    fn shell_command(script: &str) -> Command {
        Command {
            arguments: vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn captures_exit_code_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let command = shell_command("echo out; echo err >&2; exit 3");

        let output = run(&command, dir.path(), None).await.unwrap();

        assert_eq!(output.exit_code, 3);
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }

    #[tokio::test]
    async fn sets_only_the_given_environment() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = shell_command("echo \"$GREETING\"; echo \"${HOME:-unset}\"");
        command.environment_variables = vec![EnvironmentVariable {
            name: "GREETING".to_string(),
            value: "hello".to_string(),
        }];

        let output = run(&command, dir.path(), None).await.unwrap();

        assert_eq!(output.stdout, b"hello\nunset\n");
    }

    #[tokio::test]
    async fn runs_in_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), b"").unwrap();
        let command = shell_command("ls");

        let output = run(&command, dir.path(), None).await.unwrap();

        assert_eq!(output.stdout, b"marker\n");
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
    async fn kills_commands_that_time_out() {
        let dir = tempfile::tempdir().unwrap();
        let command = shell_command("sleep 10");
        let timeout = Duration::from_millis(100);

        let err = run(&command, dir.path(), Some(timeout)).await.unwrap_err();

        assert!(matches!(err, ExecuteError::Timeout(t) if t == timeout), "{err}");
    }

    #[tokio::test]
    async fn reports_signals_as_128_plus_signal() {
        let dir = tempfile::tempdir().unwrap();
        let command = shell_command("kill -TERM $$");

        let output = run(&command, dir.path(), None).await.unwrap();

        assert_eq!(output.exit_code, 128 + 15);
    }

    #[tokio::test]
    async fn rejects_empty_commands() {
        let dir = tempfile::tempdir().unwrap();

        let err = run(&Command::default(), dir.path(), None).await.unwrap_err();

        assert!(matches!(err, ExecuteError::InvalidAction(_)), "{err}");
    }
}
