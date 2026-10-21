//! The seam between the execution service and whatever runs actions.
//!
//! An [`Executor`] takes a whole action and returns its result. How it runs
//! the action (locally or remotely, isolated or not) is up to the
//! implementation, as long as it reads inputs from and writes outputs to the
//! CAS.

use async_trait::async_trait;
use cas::{CasError, Digest};
use remote_execution_proto::build::bazel::remote::execution::v2::{ActionResult, Command};
use std::time::Duration;

/// An action to run.
#[derive(Clone, Debug)]
pub struct Action {
    /// The digest of the `Action` proto, which identifies the action.
    pub digest: Digest,

    pub command: Command,

    /// The digest of the `Directory` proto at the root of the input tree.
    pub input_root: Digest,

    /// How long the action may run before it is stopped. `None` means no
    /// limit.
    pub timeout: Option<Duration>,
}

/// Runs actions.
#[async_trait]
pub trait Executor: Send + Sync + 'static {
    /// Run an action to completion and return its result, with its outputs
    /// stored in the CAS.
    ///
    /// An action that runs and exits with a non-zero code has still executed
    /// successfully: the exit code is part of the result.
    ///
    /// Dropping the returned future stops the action.
    async fn execute(&self, action: &Action) -> Result<ActionResult, ExecuteError>;
}

#[derive(Debug, thiserror::Error)]
pub enum ExecuteError {
    /// Inputs the action refers to aren't in the CAS. The client should upload
    /// them and try again.
    #[error("missing inputs: {}", format_digests(.0))]
    MissingInputs(Vec<Digest>),

    /// The action is malformed, for example with a path that escapes the
    /// input root.
    #[error("invalid action: {0}")]
    InvalidAction(String),

    /// The action ran but produced outputs that can't be returned, for
    /// example a directory where a file was expected.
    #[error("invalid output: {0}")]
    InvalidOutput(String),

    /// The action ran for longer than its timeout and was stopped.
    #[error("action timed out after {0:?}")]
    Timeout(Duration),

    /// Something went wrong in the executor itself.
    #[error("{0}")]
    Internal(String),
}

impl From<CasError> for ExecuteError {
    fn from(error: CasError) -> Self {
        match error {
            CasError::NotFound(digest) => ExecuteError::MissingInputs(vec![digest]),
            error => ExecuteError::Internal(error.to_string()),
        }
    }
}

fn format_digests(digests: &[Digest]) -> String {
    let formatted: Vec<String> = digests.iter().map(Digest::to_string).collect();
    formatted.join(", ")
}
