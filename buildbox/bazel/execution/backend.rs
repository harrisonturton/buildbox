//! Where actions run and their results are kept.

use actioncache::ActionCache;
use cas::Cas;
use executor::Executor;
use skyframe::SkyError;
use std::sync::Arc;
use tokio::sync::{Semaphore, SemaphorePermit};
use tracing::Instrument;

/// The stores and executor actions use, as a graph service: those of this
/// process, or clients of a remote execution service.
pub struct Backend {
    pub cas: Arc<dyn Cas>,
    pub action_cache: Arc<dyn ActionCache>,
    pub executor: Arc<dyn Executor>,
    staging: Semaphore,
}

impl Backend {
    /// A backend that prepares at most `max_staged` actions at once: builds
    /// their input trees and uploads their inputs, and runs them.
    pub fn new(cas: Arc<dyn Cas>, action_cache: Arc<dyn ActionCache>, executor: Arc<dyn Executor>, max_staged: usize) -> Backend {
        Backend {
            cas,
            action_cache,
            executor,
            staging: Semaphore::new(max_staged.max(1)),
        }
    }

    /// Wait for a turn to prepare and run an action. Actions whose inputs
    /// are ready wait here holding nothing but the action, so the memory
    /// their input trees take is bounded by `max_staged`.
    pub async fn stage(&self) -> Result<SemaphorePermit<'_>, SkyError> {
        let span = tracing::debug_span!("stage_wait");
        let permit = self.staging.acquire().instrument(span).await;
        permit.map_err(|_| SkyError::transient("the backend is shutting down"))
    }
}
