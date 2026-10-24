//! The action cache: results of previously executed actions, keyed by the
//! digest of the action.

use async_trait::async_trait;
use cas::Digest;
use remote_execution_rust_connect::build::bazel::remote::execution::v2::ActionResult;

/// Maps action digests to the results of running those actions.
///
/// The cache only records results. It doesn't check that the outputs a
/// result refers to are still in the CAS; callers must do that before
/// trusting a cached result.
#[async_trait]
pub trait ActionCache: Send + Sync + 'static {
    /// Look up the result of an action, if one is cached.
    async fn get(&self, action: &Digest) -> Result<Option<ActionResult>, ActionCacheError>;

    /// Record the result of an action, replacing any previous result.
    async fn put(&self, action: &Digest, result: &ActionResult) -> Result<(), ActionCacheError>;

    /// Report how much the cache holds.
    async fn usage(&self) -> Result<Usage, ActionCacheError>;
}

/// How much an [`ActionCache`] holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub entries: u64,
    pub bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ActionCacheError {
    #[error("{context}: {error}")]
    Io {
        context: String,
        error: std::io::Error,
    },
}

impl ActionCacheError {
    pub fn io(context: impl Into<String>, error: std::io::Error) -> ActionCacheError {
        ActionCacheError::Io {
            context: context.into(),
            error,
        }
    }
}
