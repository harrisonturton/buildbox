//! A [`ContainerRuntime`] built directly on Linux namespaces, with no
//! daemon or other tools.
//!
//! Each container is created by a helper process: the buildbox binary
//! itself, run with [`HELPER_COMMAND`]. See `init.rs` for what it does.
//! Containers are rootless: root in a container is the user buildbox runs
//! as, and has no more access to the host than that user.

#[cfg(target_os = "linux")]
mod init;
#[cfg(target_os = "linux")]
mod mounts;
mod protocol;
#[cfg(target_os = "linux")]
mod sys;

use async_trait::async_trait;
use container::{ContainerError, ContainerRuntime, ContainerSpec, Network, ProcessOutput};
use fsutil::run_blocking;
use process::{Invocation, ProcessError};
use protocol::{InitSpec, Outcome, PROBE_MODE, RESULT_FILE, RUN_MODE, SPEC_FILE};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;
use tempfile::TempDir;

/// The first argument that makes the buildbox binary run as the helper.
pub const HELPER_COMMAND: &str = "__container-init";

/// How long checking that containers work may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The helper's entry point, given the arguments after [`HELPER_COMMAND`].
///
/// Call this before starting any threads: creating a user namespace
/// requires a single-threaded process.
pub fn helper_main(args: &[OsString]) -> ExitCode {
    #[cfg(target_os = "linux")]
    return init::main(args);

    #[cfg(not(target_os = "linux"))]
    {
        let _ = args;
        eprintln!("containers are only supported on Linux");
        ExitCode::FAILURE
    }
}

/// How to start the helper process.
#[derive(Clone, Debug)]
pub struct Helper {
    pub program: PathBuf,

    /// Arguments to pass before the helper's own.
    pub args: Vec<String>,
}

impl Helper {
    /// The running buildbox binary. Running it through `/proc/self/exe`
    /// means the helper is always the same version as the server, even if
    /// the binary is replaced on disk.
    pub fn this_binary() -> Helper {
        Helper {
            program: PathBuf::from("/proc/self/exe"),
            args: vec![HELPER_COMMAND.to_string()],
        }
    }
}

/// Runs each container with a helper process. See the module docs.
pub struct NamespaceRuntime {
    helper: Helper,
    state_dir: PathBuf,
}

impl NamespaceRuntime {
    /// Create a runtime that keeps the state of running containers in
    /// `state_dir`, deleting anything left there by a previous run. Fails,
    /// explaining why, if this machine can't run containers.
    pub async fn open(helper: Helper, state_dir: &Path) -> Result<NamespaceRuntime, ContainerError> {
        let dir = state_dir.to_path_buf();
        run_blocking(move || fsutil::reset_dir(&dir))
            .await
            .map_err(|error| ContainerError::Internal(format!("failed to reset {}: {error}", state_dir.display())))?;

        let runtime = NamespaceRuntime {
            helper,
            state_dir: state_dir.to_path_buf(),
        };
        runtime.probe().await?;
        Ok(runtime)
    }

    async fn probe(&self) -> Result<(), ContainerError> {
        let dir = self.create_dir().await?;
        let (outcome, output) = self.run_helper(PROBE_MODE, &dir, Some(PROBE_TIMEOUT)).await?;
        remove_dir(dir).await;
        match outcome {
            Outcome::Exited { .. } => Ok(()),
            outcome => Err(outcome_error(outcome, &output)),
        }
    }

    /// A new directory for one container's spec and result.
    async fn create_dir(&self) -> Result<TempDir, ContainerError> {
        let state_dir = self.state_dir.clone();
        run_blocking(move || tempfile::Builder::new().prefix("container-").tempdir_in(&state_dir))
            .await
            .map_err(|error| ContainerError::Internal(format!("failed to create a container directory: {error}")))
    }

    /// Run the helper in `mode` on `dir`, returning the outcome it reported
    /// and what it printed.
    async fn run_helper(
        &self,
        mode: &str,
        dir: &TempDir,
        timeout: Option<Duration>,
    ) -> Result<(Outcome, ProcessOutput), ContainerError> {
        let mut args = self.helper.args.clone();
        args.push(mode.to_string());
        args.push(dir.path().display().to_string());
        let invocation = Invocation {
            program: self.helper.program.clone(),
            args,
            env: vec![],
            working_dir: PathBuf::from("/"),
        };

        let output = process::run(&invocation, timeout).await.map_err(process_error)?;
        let result_path = dir.path().join(RESULT_FILE);
        let result = run_blocking(move || std::fs::read(&result_path)).await.unwrap_or_default();
        let Ok(outcome) = serde_json::from_slice(&result) else {
            let message = format!(
                "the container helper exited with {} without reporting a result: {}",
                output.exit_code,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return Err(ContainerError::Internal(message));
        };
        Ok((outcome, output))
    }
}

#[async_trait]
impl ContainerRuntime for NamespaceRuntime {
    #[tracing::instrument(name = "run_container", skip_all)]
    async fn run(&self, spec: &ContainerSpec, timeout: Option<Duration>) -> Result<ProcessOutput, ContainerError> {
        let dir = self.create_dir().await?;
        let init_spec = init_spec(spec);
        let spec_path = dir.path().join(SPEC_FILE);
        run_blocking(move || {
            let json = serde_json::to_vec(&init_spec).map_err(std::io::Error::other)?;
            std::fs::write(&spec_path, json)
        })
        .await
        .map_err(|error| ContainerError::Internal(format!("failed to write the container spec: {error}")))?;

        let (outcome, output) = self.run_helper(RUN_MODE, &dir, timeout).await?;
        remove_dir(dir).await;
        match outcome {
            Outcome::Exited { exit_code } => Ok(ProcessOutput { exit_code, ..output }),
            outcome => Err(outcome_error(outcome, &output)),
        }
    }
}

fn init_spec(spec: &ContainerSpec) -> InitSpec {
    InitSpec {
        rootfs: spec.rootfs.clone(),
        mounts: spec
            .mounts
            .iter()
            .map(|mount| (mount.source.clone(), mount.target.clone()))
            .collect(),
        args: spec.args.clone(),
        env: spec.env.clone(),
        working_dir: spec.working_dir.clone(),
        isolate_network: spec.network == Network::Isolated,
    }
}

fn outcome_error(outcome: Outcome, output: &ProcessOutput) -> ContainerError {
    match outcome {
        Outcome::StartFailed { message } => ContainerError::Start(message),
        Outcome::SetupFailed { message } => ContainerError::Setup(message),
        Outcome::Exited { exit_code } => ContainerError::Internal(format!(
            "unexpected exit {exit_code}: {}",
            String::from_utf8_lossy(&output.stderr)
        )),
    }
}

fn process_error(error: ProcessError) -> ContainerError {
    match error {
        ProcessError::Timeout(timeout) => ContainerError::Timeout(timeout),
        error => ContainerError::Internal(format!("container helper: {error}")),
    }
}

/// Delete a container's directory. If this isn't reached, e.g. because the
/// container was stopped, dropping the directory deletes it instead.
async fn remove_dir(dir: TempDir) {
    let path = dir.path().to_path_buf();
    let removed = run_blocking(move || dir.close()).await;
    if let Err(error) = removed {
        tracing::warn!("failed to remove {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests;
