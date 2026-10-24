//! Where actions run and their results are kept.

use actioncache::ActionCache;
use cas::Cas;
use executor::Executor;
use std::sync::Arc;

/// The stores and executor actions use, as a graph service: those of this
/// process, or clients of a remote execution service.
pub struct Backend {
    pub cas: Arc<dyn Cas>,
    pub action_cache: Arc<dyn ActionCache>,
    pub executor: Arc<dyn Executor>,
}
