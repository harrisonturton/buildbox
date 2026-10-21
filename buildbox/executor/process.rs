//! Running an action's command.

use executor::ExecuteError;
use remote_execution_proto::build::bazel::remote::execution::v2::Command;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;

/// How many times to try starting a process whose program is busy, and how
/// long to wait before the first retry. Each retry waits longer.
const SPAWN_ATTEMPTS: u32 = 5;
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(10);

/// What a finished command produced.
#[derive(Debug)]
pub struct ProcessOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Run `command` in `working_dir` and wait for it to finish.
///
/// The command gets exactly the environment variables it specifies. It runs
/// in its own process group, and when it finishes, times out or is abandoned,
/// every process still in the group is killed.
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
        .process_group(0)
        // Stop the process if the action is abandoned, e.g. on timeout.
        .kill_on_drop(true);
    for variable in &command.environment_variables {
        process.env(&variable.name, &variable.value);
    }

    let child = spawn(&mut process).await.map_err(|error| {
        ExecuteError::InvalidAction(format!("failed to start {}: {error}", program.display()))
    })?;

    let finished = wait_for_output(child);
    let Some(timeout) = timeout else {
        return finished.await;
    };
    match tokio::time::timeout(timeout, finished).await {
        Ok(output) => output,
        Err(_elapsed) => Err(ExecuteError::Timeout(timeout)),
    }
}

/// Start a process, retrying while its program is "busy".
///
/// Running a file fails with ETXTBSY while any process has it open for
/// writing. That happens briefly, and harmlessly, when another thread forks
/// just as the file is written: the child holds a copy of the file
/// descriptor until it calls exec. Retrying shortly afterwards succeeds.
async fn spawn(process: &mut tokio::process::Command) -> std::io::Result<Child> {
    let mut attempt = 1;
    loop {
        let error = match process.spawn() {
            Ok(child) => return Ok(child),
            Err(error) => error,
        };
        if error.raw_os_error() != Some(libc::ETXTBSY) || attempt == SPAWN_ATTEMPTS {
            return Err(error);
        }
        tokio::time::sleep(SPAWN_RETRY_DELAY * attempt).await;
        attempt += 1;
    }
}

/// Wait for a command to exit, then kill anything it left running, and
/// collect its output.
///
/// Processes left running may hold copies of the command's stdout and
/// stderr, so killing them is also what lets reading the output finish.
/// Dropping the returned future kills the command and everything it started.
async fn wait_for_output(mut child: Child) -> Result<ProcessOutput, ExecuteError> {
    let group = ProcessGroup::of(&child);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let wait = async move {
        let status = child.wait().await;
        drop(group);
        status
    };
    let (status, stdout, stderr) = tokio::join!(wait, read_pipe(stdout), read_pipe(stderr));

    let status = status.map_err(|error| ExecuteError::Internal(format!("failed to wait for command: {error}")))?;
    let stdout = stdout.map_err(|error| ExecuteError::Internal(format!("failed to read stdout: {error}")))?;
    let stderr = stderr.map_err(|error| ExecuteError::Internal(format!("failed to read stderr: {error}")))?;
    Ok(ProcessOutput {
        exit_code: exit_code(status),
        stdout,
        stderr,
    })
}

async fn read_pipe(pipe: Option<impl AsyncRead + Unpin>) -> std::io::Result<Vec<u8>> {
    let mut data = vec![];
    if let Some(mut pipe) = pipe {
        pipe.read_to_end(&mut data).await?;
    }
    Ok(data)
}

/// The process group a command runs in. Dropping it kills every process
/// still in the group, so nothing a command started outlives it.
struct ProcessGroup {
    id: libc::pid_t,
}

impl ProcessGroup {
    /// The group led by `child`, which must have been started in a new group.
    fn of(child: &Child) -> Option<ProcessGroup> {
        let pid = child.id()?;
        let id = libc::pid_t::try_from(pid).ok()?;
        Some(ProcessGroup { id })
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // The group usually has no processes left, in which case this fails
        // harmlessly. The group's ID can't be reused while any process is in
        // it, and is killed immediately after the leader exits.
        //
        // SAFETY: killpg only sends a signal, and has no memory safety
        // requirements.
        unsafe {
            libc::killpg(self.id, libc::SIGKILL);
        }
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

    fn process_exists(pid: libc::pid_t) -> bool {
        Path::new(&format!("/proc/{pid}")).exists()
    }

    async fn wait_for_exit(pid: libc::pid_t) {
        for _ in 0..500 {
            if !process_exists(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("process {pid} is still running");
    }

    #[tokio::test]
    async fn kills_background_processes_when_the_command_exits() {
        let dir = tempfile::tempdir().unwrap();
        let command = shell_command("sleep 300 & echo $!");

        let output = run(&command, dir.path(), None).await.unwrap();

        let background: libc::pid_t = String::from_utf8(output.stdout).unwrap().trim().parse().unwrap();
        wait_for_exit(background).await;
    }

    #[tokio::test]
    async fn kills_child_processes_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let command = shell_command(&format!("sleep 300 & echo $! > {}; wait", pid_file.display()));

        let err = run(&command, dir.path(), Some(Duration::from_millis(200))).await.unwrap_err();

        assert!(matches!(err, ExecuteError::Timeout(_)), "{err}");
        let child: libc::pid_t = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        wait_for_exit(child).await;
    }

    #[tokio::test]
    async fn rejects_empty_commands() {
        let dir = tempfile::tempdir().unwrap();

        let err = run(&Command::default(), dir.path(), None).await.unwrap_err();

        assert!(matches!(err, ExecuteError::InvalidAction(_)), "{err}");
    }
}
