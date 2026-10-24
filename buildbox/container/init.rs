//! The helper process that creates a container and runs a command in it.
//!
//! The runtime runs the helper as a new process, because creating a user
//! namespace requires a single-threaded process, and the server has many
//! threads. The helper:
//!
//! 1. creates new user, mount, PID, IPC, UTS and (optionally) network
//!    namespaces, mapping root in them to the user it runs as;
//! 2. forks the container's first process, PID 1 in the new PID namespace,
//!    which builds the container's filesystem, then runs the command as its
//!    child and reaps every process until the command exits.
//!
//! When PID 1 exits, the kernel kills everything left in the container.
//! The outcome is written to a result file, so the runtime can tell the
//! command's exit code from a failure to create the container.

use crate::mounts;
use crate::protocol::{InitSpec, Outcome, PROBE_MODE, RESULT_FILE, RUN_MODE, SPEC_FILE};
use crate::sys::{self, Fork};
use std::ffi::{CString, OsString};
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HOSTNAME: &str = "buildbox";

/// `PATH` for finding the program when the command doesn't set one.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Run the helper with its arguments, the mode and the directory for this
/// container.
pub fn main(args: &[OsString]) -> ExitCode {
    let [mode, dir] = args else {
        eprintln!("usage: {RUN_MODE}|{PROBE_MODE} <dir>");
        return ExitCode::FAILURE;
    };
    let dir = Path::new(dir);
    let result_path = dir.join(RESULT_FILE);
    let result = match File::create(&result_path) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("failed to create {}: {error}", result_path.display());
            return ExitCode::FAILURE;
        }
    };

    let outcome = match mode.to_str() {
        Some(RUN_MODE) => run(dir, &result),
        Some(PROBE_MODE) => in_container(true, &result, || probe(dir)),
        _ => Some(setup_failed(format!("unknown mode {mode:?}"))),
    };
    if let Some(outcome) = outcome {
        write_outcome(&result, &outcome);
    }
    ExitCode::SUCCESS
}

fn run(dir: &Path, result: &File) -> Option<Outcome> {
    let spec = match read_spec(dir) {
        Ok(spec) => spec,
        Err(message) => return Some(setup_failed(message)),
    };
    in_container(spec.isolate_network, result, || run_in_container(&spec))
}

fn read_spec(dir: &Path) -> Result<InitSpec, String> {
    let path = dir.join(SPEC_FILE);
    let data = std::fs::read(&path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    serde_json::from_slice(&data).map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

/// Create new namespaces and run `init` as the first process in them, which
/// records its outcome in `result`. Returns an outcome to record if `init`
/// couldn't.
fn in_container(isolate_network: bool, result: &File, init: impl FnOnce() -> Outcome) -> Option<Outcome> {
    if let Err(message) = enter_namespaces(isolate_network) {
        return Some(setup_failed(message));
    }

    let child = match sys::fork() {
        Ok(Fork::Child) => run_as_init(result, init),
        Ok(Fork::Parent { child }) => child,
        Err(error) => return Some(setup_failed(format!("failed to start the container: {error}"))),
    };

    let status = match sys::wait(child) {
        Ok((_, status)) => status,
        Err(error) => return Some(setup_failed(format!("failed to wait for the container: {error}"))),
    };
    let reported = result.metadata().is_ok_and(|metadata| metadata.len() > 0);
    if reported {
        return None;
    }
    let exit_code = sys::exit_code(status);
    let message = format!("the container's first process exited with {exit_code} without reporting what happened");
    Some(setup_failed(message))
}

/// The container's first process: run `init`, record its outcome, and
/// exit, which kills anything else in the container.
fn run_as_init(result: &File, init: impl FnOnce() -> Outcome) -> ! {
    let outcome = match sys::die_with_parent() {
        Ok(()) => init(),
        Err(error) => setup_failed(format!("failed to tie the container to the helper: {error}")),
    };
    write_outcome(result, &outcome);
    sys::exit_immediately(0)
}

/// Move into new namespaces, with root in the user namespace mapped to the
/// user and group this process runs as.
fn enter_namespaces(isolate_network: bool) -> Result<(), String> {
    // SAFETY: getuid and getgid take no arguments and can't fail.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };

    let mut flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWPID | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS;
    if isolate_network {
        flags |= libc::CLONE_NEWNET;
    }
    sys::unshare(flags).map_err(|error| namespace_error("failed to create namespaces", error))?;

    // Groups can't be mapped without first giving up setgroups.
    write_proc_file("setgroups", "deny")?;
    write_proc_file("uid_map", &format!("0 {uid} 1"))?;
    write_proc_file("gid_map", &format!("0 {gid} 1"))
}

fn write_proc_file(name: &str, contents: &str) -> Result<(), String> {
    let path = Path::new("/proc/self").join(name);
    std::fs::write(&path, contents).map_err(|error| namespace_error(&format!("failed to write {}", path.display()), error))
}

/// Explain the usual reason for being refused a user namespace.
fn namespace_error(action: &str, error: std::io::Error) -> String {
    let is_permission_error = matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES | libc::ENOSPC));
    if !is_permission_error {
        return format!("{action}: {error}");
    }
    format!(
        "{action}: {error}. This machine doesn't let buildbox create user namespaces; on Ubuntu, allow it \
         with an AppArmor profile, as the buildbox README describes"
    )
}

fn run_in_container(spec: &InitSpec) -> Outcome {
    if let Err(message) = set_up_container(spec) {
        return setup_failed(message);
    }
    let command = match Command::prepare(spec) {
        Ok(command) => command,
        Err(message) => return Outcome::StartFailed { message },
    };
    command.run()
}

fn set_up_container(spec: &InitSpec) -> Result<(), String> {
    mounts::set_up(&spec.rootfs, &spec.mounts)?;
    sys::set_hostname(HOSTNAME).map_err(|error| format!("failed to set the hostname: {error}"))?;
    if spec.isolate_network {
        sys::bring_up_loopback().map_err(|error| format!("failed to bring up loopback: {error}"))?;
    }
    std::env::set_current_dir(&spec.working_dir)
        .map_err(|error| format!("failed to enter {}: {error}", spec.working_dir.display()))
}

/// Check that a container's filesystem can be built, by mounting a tmpfs
/// and a `/proc` in it.
fn probe(dir: &Path) -> Outcome {
    let mount_private = sys::mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE, None);
    let scratch = dir.join("probe");
    let proc = scratch.join("proc");
    let probed = mount_private
        .and_then(|()| std::fs::create_dir(&scratch))
        .and_then(|()| sys::mount(Some(Path::new("tmpfs")), &scratch, Some("tmpfs"), 0, None))
        .and_then(|()| std::fs::create_dir(&proc))
        .and_then(|()| sys::mount(Some(Path::new("proc")), &proc, Some("proc"), 0, None));
    match probed {
        Ok(()) => Outcome::Exited { exit_code: 0 },
        Err(error) => setup_failed(format!("failed to mount a filesystem in a container: {error}")),
    }
}

/// A command, ready to run with execve.
struct Command {
    program: CString,
    args: Vec<CString>,
    env: Vec<CString>,
}

impl Command {
    fn prepare(spec: &InitSpec) -> Result<Command, String> {
        let Some(name) = spec.args.first() else {
            return Err("the command has no arguments".to_string());
        };
        let program = find_program(name, &spec.env)?;

        let program = sys::path_cstring(&program).map_err(|error| error.to_string())?;
        let mut args = vec![];
        for arg in &spec.args {
            args.push(CString::new(arg.as_str()).map_err(|_| format!("argument {arg:?} contains NUL"))?);
        }
        let mut env = vec![];
        for (name, value) in &spec.env {
            if name.is_empty() || name.contains('=') {
                return Err(format!("invalid environment variable name {name:?}"));
            }
            let variable = format!("{name}={value}");
            env.push(CString::new(variable).map_err(|_| format!("environment variable {name} contains NUL"))?);
        }
        Ok(Command { program, args, env })
    }

    /// Run the command as a child of this process, then reap every process
    /// until it exits.
    fn run(self) -> Outcome {
        match self.start() {
            Ok(child) => reap_until(child),
            Err(outcome) => outcome,
        }
    }

    /// Start the command. Whether exec succeeded is reported back through a
    /// pipe that exec closes: data on it is the error from a failed exec.
    fn start(&self) -> Result<libc::pid_t, Outcome> {
        let stdin = File::open("/dev/null").map_err(|error| setup_failed(format!("failed to open /dev/null: {error}")))?;
        let (exec_error_read, exec_error_write) =
            sys::pipe().map_err(|error| setup_failed(format!("failed to create a pipe: {error}")))?;

        // Built before forking, so the child doesn't allocate.
        let args = null_terminated(&self.args);
        let env = null_terminated(&self.env);
        let child = match sys::fork() {
            Ok(Fork::Child) => {
                let error = match sys::set_stdin(stdin.as_raw_fd()) {
                    Ok(()) => sys::execve(&self.program, &args, &env),
                    Err(error) => error,
                };
                let errno = error.raw_os_error().unwrap_or(libc::EIO);
                sys::write_ignoring_errors(exec_error_write.as_raw_fd(), &errno.to_ne_bytes());
                sys::exit_immediately(127)
            }
            Ok(Fork::Parent { child }) => child,
            Err(error) => return Err(setup_failed(format!("failed to start the command: {error}"))),
        };
        drop(exec_error_write);

        let mut errno = [0; 4];
        let read = sys::read(exec_error_read.as_raw_fd(), &mut errno).unwrap_or(0);
        if read == errno.len() {
            let _ = sys::wait(child);
            let error = std::io::Error::from_raw_os_error(i32::from_ne_bytes(errno));
            let message = format!("failed to start {}: {error}", self.program.to_string_lossy());
            return Err(Outcome::StartFailed { message });
        }
        Ok(child)
    }
}

/// Reap children, which include orphans re-parented to this process, until
/// `command` exits.
fn reap_until(command: libc::pid_t) -> Outcome {
    loop {
        match sys::wait(-1) {
            Ok((pid, status)) if pid == command => {
                return Outcome::Exited {
                    exit_code: sys::exit_code(status),
                }
            }
            Ok(_) => continue,
            Err(error) => return setup_failed(format!("failed to wait for the command: {error}")),
        }
    }
}

/// Pointers to each string, then a null pointer, as execve takes.
fn null_terminated(strings: &[CString]) -> Vec<*const libc::c_char> {
    let mut pointers: Vec<*const libc::c_char> = strings.iter().map(|string| string.as_ptr()).collect();
    pointers.push(std::ptr::null());
    pointers
}

/// Find the program a command names: a path if it has a slash, or else a
/// file in `PATH`, as a shell would.
fn find_program(name: &str, env: &[(String, String)]) -> Result<PathBuf, String> {
    if name.contains('/') {
        return Ok(PathBuf::from(name));
    }

    let path = env
        .iter()
        .rev()
        .find(|(variable, _)| variable == "PATH")
        .map_or(DEFAULT_PATH, |(_, value)| value.as_str());
    for dir in path.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let candidate = Path::new(dir).join(name);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    Err(format!("failed to start {name}: not found in PATH ({path})"))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

fn write_outcome(mut result: &File, outcome: &Outcome) {
    // Serializing these types can't fail.
    let json = serde_json::to_vec(outcome).unwrap_or_default();
    if let Err(error) = result.write_all(&json) {
        eprintln!("failed to record the container's outcome: {error}");
    }
}

fn setup_failed(message: String) -> Outcome {
    Outcome::SetupFailed { message }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_in_path() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("tool");
        std::fs::write(&tool, b"").unwrap();
        std::fs::set_permissions(&tool, PermissionsExt::from_mode(0o755)).unwrap();
        let env = vec![("PATH".to_string(), format!("/nonexistent:{}", dir.path().display()))];

        assert_eq!(find_program("tool", &env).unwrap(), tool);
        assert_eq!(find_program("./relative", &env).unwrap(), Path::new("./relative"));
        assert!(find_program("missing", &env).is_err());
    }

    #[test]
    fn rejects_invalid_environments() {
        let spec = InitSpec {
            rootfs: PathBuf::from("/"),
            mounts: vec![],
            args: vec!["/bin/true".to_string()],
            env: vec![("A=B".to_string(), "c".to_string())],
            working_dir: PathBuf::from("/"),
            isolate_network: true,
        };

        assert!(Command::prepare(&spec).is_err());
    }
}
