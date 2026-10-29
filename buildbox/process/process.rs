//! Running a process to completion, capturing its output, and making sure
//! nothing it started outlives it.

use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Child;
use tracing::field::Empty;
use tracing::Span;

/// How many times to try starting a process whose program is busy, and how
/// long to wait before the first retry. Each retry waits longer.
const SPAWN_ATTEMPTS: u32 = 5;
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(10);

/// A process to run.
#[derive(Clone, Debug)]
pub struct Invocation {
    /// The program to run. A bare name is looked up in `PATH`.
    pub program: PathBuf,
    pub args: Vec<String>,

    /// The process's entire environment.
    pub env: Vec<(String, String)>,

    pub working_dir: PathBuf,
}

/// What a finished process produced.
#[derive(Debug)]
pub struct ProcessOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("failed to start {}: {error}", program.display())]
    Start { program: PathBuf, error: std::io::Error },

    /// The process ran for longer than its timeout and was killed.
    #[error("timed out after {0:?}")]
    Timeout(Duration),

    #[error("failed to {action}: {error}")]
    Io { action: &'static str, error: std::io::Error },
}

/// Run `invocation` and wait for it to finish.
///
/// The process runs in its own process group, and when it finishes, times out
/// or is abandoned, every process still in the group is killed.
#[tracing::instrument(name = "run_command", skip_all, fields(program = Empty, exit_code = Empty))]
pub async fn run(invocation: &Invocation, timeout: Option<Duration>) -> Result<ProcessOutput, ProcessError> {
    let span = Span::current();
    span.record("program", invocation.program.display().to_string());

    let mut process = tokio::process::Command::new(&invocation.program);
    process
        .args(&invocation.args)
        .current_dir(&invocation.working_dir)
        .env_clear()
        .envs(invocation.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        // Stop the process if it is abandoned, e.g. on timeout.
        .kill_on_drop(true);

    let child = spawn(process).await.map_err(|error| ProcessError::Start {
        program: invocation.program.clone(),
        error,
    })?;

    let finished = wait_for_output(child);
    let output = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, finished)
            .await
            .unwrap_or(Err(ProcessError::Timeout(timeout))),
        None => finished.await,
    };
    if let Ok(output) = &output {
        span.record("exit_code", output.exit_code);
    }
    output
}

/// A process kept running to talk to, such as a persistent worker: its
/// stdin and stdout are connected, and what it writes to stderr is kept.
/// Dropping it kills the process and everything it started.
pub struct Interactive {
    pub stdin: tokio::process::ChildStdin,
    pub stdout: tokio::process::ChildStdout,
    child: Child,
    stderr: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    _group: Option<ProcessGroup>,
}

/// How much of a long-running process's stderr is kept: the end of it.
const KEPT_STDERR: usize = 64 * 1024;

impl Interactive {
    /// What the process has written to stderr, or the end of it.
    pub fn stderr(&self) -> String {
        let stderr = self.stderr.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        String::from_utf8_lossy(&stderr).into_owned()
    }

    /// Whether the process is still running.
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

/// Start `invocation` with its stdin and stdout connected, in its own
/// process group.
pub async fn start(invocation: &Invocation) -> Result<Interactive, ProcessError> {
    let mut process = tokio::process::Command::new(&invocation.program);
    process
        .args(&invocation.args)
        .current_dir(&invocation.working_dir)
        .env_clear()
        .envs(invocation.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = spawn(process).await.map_err(|error| ProcessError::Start {
        program: invocation.program.clone(),
        error,
    })?;
    let group = ProcessGroup::of(&child);
    let (Some(stdin), Some(stdout), Some(stderr_pipe)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
        return Err(io_error("connect to the process", std::io::Error::other("missing pipe")));
    };
    let stderr = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    tokio::spawn(keep_stderr(stderr_pipe, stderr.clone()));
    Ok(Interactive {
        stdin,
        stdout,
        child,
        stderr,
        _group: group,
    })
}

/// Keep the end of what a process writes to stderr, until it closes it.
async fn keep_stderr(mut pipe: tokio::process::ChildStderr, kept: std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
    let mut buffer = [0u8; 8192];
    while let Ok(read) = pipe.read(&mut buffer).await {
        if read == 0 {
            return;
        }
        let mut kept = kept.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        kept.extend_from_slice(&buffer[..read]);
        let excess = kept.len().saturating_sub(KEPT_STDERR);
        kept.drain(..excess);
    }
}

/// Start a process, retrying while its program is "busy".
///
/// Running a file fails with ETXTBSY while any process has it open for
/// writing. That happens briefly, and harmlessly, when another thread forks
/// just as the file is written: the child holds a copy of the file
/// descriptor until it calls exec. Retrying shortly afterwards succeeds.
///
/// Starting a process blocks until the child has called exec, which can
/// take milliseconds, so it happens on a blocking thread.
async fn spawn(mut process: tokio::process::Command) -> std::io::Result<Child> {
    let mut attempt = 1;
    loop {
        let started = tokio::task::spawn_blocking(move || {
            let child = process.spawn();
            (process, child)
        })
        .await;
        let (returned, child) = started.map_err(|error| std::io::Error::other(format!("process start task failed: {error}")))?;
        process = returned;
        let error = match child {
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

/// Wait for a process to exit, then kill anything it left running, and
/// collect its output.
///
/// Processes left running may hold copies of the process's stdout and
/// stderr, so killing them is also what lets reading the output finish.
/// Dropping the returned future kills the process and everything it started.
async fn wait_for_output(mut child: Child) -> Result<ProcessOutput, ProcessError> {
    let group = ProcessGroup::of(&child);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let wait = async move {
        let status = child.wait().await;
        drop(group);
        status
    };
    let (status, stdout, stderr) = tokio::join!(wait, read_pipe(stdout), read_pipe(stderr));

    let status = status.map_err(|error| io_error("wait for the process", error))?;
    let stdout = stdout.map_err(|error| io_error("read stdout", error))?;
    let stderr = stderr.map_err(|error| io_error("read stderr", error))?;
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

/// The process group a process runs in. Dropping it kills every process
/// still in the group, so nothing the process started outlives it.
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

fn io_error(action: &'static str, error: std::io::Error) -> ProcessError {
    ProcessError::Io { action, error }
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
    use std::path::Path;

    fn shell(script: &str, working_dir: &Path) -> Invocation {
        Invocation {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), script.to_string()],
            env: vec![],
            working_dir: working_dir.to_path_buf(),
        }
    }

    #[tokio::test]
    async fn captures_exit_code_and_output() {
        let dir = tempfile::tempdir().unwrap();
        let invocation = shell("echo out; echo err >&2; exit 3", dir.path());

        let output = run(&invocation, None).await.unwrap();

        assert_eq!(output.exit_code, 3);
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }

    #[tokio::test]
    async fn sets_only_the_given_environment() {
        let dir = tempfile::tempdir().unwrap();
        let mut invocation = shell("echo \"$GREETING\"; echo \"${HOME:-unset}\"", dir.path());
        invocation.env = vec![("GREETING".to_string(), "hello".to_string())];

        let output = run(&invocation, None).await.unwrap();

        assert_eq!(output.stdout, b"hello\nunset\n");
    }

    #[tokio::test]
    async fn runs_in_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), b"").unwrap();

        let output = run(&shell("ls", dir.path()), None).await.unwrap();

        assert_eq!(output.stdout, b"marker\n");
    }

    #[tokio::test]
    async fn reports_programs_that_fail_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut invocation = shell("true", dir.path());
        invocation.program = dir.path().join("missing");

        let err = run(&invocation, None).await.unwrap_err();

        assert!(matches!(err, ProcessError::Start { .. }), "{err}");
    }

    #[tokio::test]
    async fn kills_processes_that_time_out() {
        let dir = tempfile::tempdir().unwrap();
        let timeout = Duration::from_millis(100);

        let err = run(&shell("sleep 10", dir.path()), Some(timeout)).await.unwrap_err();

        assert!(matches!(err, ProcessError::Timeout(t) if t == timeout), "{err}");
    }

    #[tokio::test]
    async fn reports_signals_as_128_plus_signal() {
        let dir = tempfile::tempdir().unwrap();

        let output = run(&shell("kill -TERM $$", dir.path()), None).await.unwrap();

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
    async fn kills_background_processes_when_the_process_exits() {
        let dir = tempfile::tempdir().unwrap();

        let output = run(&shell("sleep 300 & echo $!", dir.path()), None).await.unwrap();

        let background: libc::pid_t = String::from_utf8(output.stdout).unwrap().trim().parse().unwrap();
        wait_for_exit(background).await;
    }

    #[tokio::test]
    async fn kills_child_processes_on_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!("sleep 300 & echo $! > {}; wait", pid_file.display());

        let err = run(&shell(&script, dir.path()), Some(Duration::from_millis(200))).await.unwrap_err();

        assert!(matches!(err, ProcessError::Timeout(_)), "{err}");
        let child: libc::pid_t = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        wait_for_exit(child).await;
    }
}
