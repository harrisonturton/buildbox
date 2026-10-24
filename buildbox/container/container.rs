//! The interface to container runtimes: running a command in a container
//! whose root filesystem is an unpacked image, with directories from the
//! host mounted in.
//!
//! Buildbox's own runtime is in `:namespaces`. Others, such as one built on
//! an OCI runtime library, can implement the same trait.

use async_trait::async_trait;
use std::path::PathBuf;
use std::time::Duration;

pub use process::ProcessOutput;

/// Runs commands in containers.
#[async_trait]
pub trait ContainerRuntime: Send + Sync + 'static {
    /// Run the command in a new container and wait for it to finish. The
    /// container, and everything in it, is gone when this returns.
    ///
    /// Dropping the returned future stops the container.
    async fn run(&self, spec: &ContainerSpec, timeout: Option<Duration>) -> Result<ProcessOutput, ContainerError>;
}

/// What to run, and in what container.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerSpec {
    /// The container's root filesystem on the host. It is mounted read-only,
    /// and must not change while the container runs.
    pub rootfs: PathBuf,

    /// Host directories to mount, writable, in the container.
    pub mounts: Vec<Mount>,

    /// The command line. The program is looked up in the container's `PATH`
    /// if it has no slash.
    pub args: Vec<String>,

    /// The command's entire environment.
    pub env: Vec<(String, String)>,

    /// The absolute path, in the container, to run the command in.
    pub working_dir: PathBuf,

    pub network: Network,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    /// The directory on the host.
    pub source: PathBuf,

    /// The absolute path to mount it at in the container.
    pub target: PathBuf,
}

/// The network a container can reach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    /// Only a loopback interface of its own.
    Isolated,

    /// The host's network.
    Host,
}

#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    /// The command couldn't be started, e.g. because its program isn't in
    /// the container.
    #[error("{0}")]
    Start(String),

    /// The command ran for longer than its timeout and was stopped.
    #[error("timed out after {0:?}")]
    Timeout(Duration),

    /// The container couldn't be created.
    #[error("failed to set up the container: {0}")]
    Setup(String),

    #[error("{0}")]
    Internal(String),
}
