//! What the runtime and its helper process tell each other, through files in
//! a directory made for each container.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The helper's modes, its first argument; the second is the container's
/// directory. In `run` mode it runs the command in the [`InitSpec`] there.
pub const RUN_MODE: &str = "run";

/// In `probe` mode the helper checks that containers can be created here.
pub const PROBE_MODE: &str = "probe";

/// The file the runtime writes the [`InitSpec`] to.
pub const SPEC_FILE: &str = "spec.json";

/// The file the helper writes the [`Outcome`] to.
pub const RESULT_FILE: &str = "result.json";

/// The container to create, and the command to run in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitSpec {
    pub rootfs: PathBuf,

    /// Host directories to mount, as (source on the host, absolute target in
    /// the container) pairs.
    pub mounts: Vec<(PathBuf, PathBuf)>,

    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_dir: PathBuf,

    /// Whether the container gets its own network, with only loopback.
    pub isolate_network: bool,
}

/// How running the container went.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// The command ran, and exited with this code, or 128 plus the signal
    /// that killed it.
    Exited { exit_code: i32 },

    /// The command couldn't be started.
    StartFailed { message: String },

    /// The container couldn't be created.
    SetupFailed { message: String },
}
