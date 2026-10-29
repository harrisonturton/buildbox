//! Where actions run and their results are kept.

use crate::digest::BlobDigest;
use actioncache::ActionCache;
use cas::Cas;
use executor::Executor;
use skyframe::SkyError;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::{OnceCell, Semaphore, SemaphorePermit};
use tracing::Instrument;

/// The stores and executor actions use, as a graph service: those of this
/// process, or clients of a remote execution service.
pub struct Backend {
    pub cas: Arc<dyn Cas>,
    pub action_cache: Arc<dyn ActionCache>,
    pub executor: Arc<dyn Executor>,
    pub runners: Runners,
    /// Whether actions that can run in persistent workers are marked to,
    /// for the executor's workers.
    pub persistent_workers: bool,
    staging: Semaphore,
    /// Source files being put in the CAS, by digest, so that actions that
    /// need the same file at once put it there once.
    uploads: Mutex<HashMap<BlobDigest, Arc<OnceCell<()>>>>,
}

/// What Bazel would call the ways a backend gets an action's result, for
/// progress output and build summaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Runners {
    /// Running the action.
    pub run: &'static str,
    /// Running the action in a persistent worker.
    pub worker: &'static str,
    /// Finding its result in the action cache.
    pub cache_hit: &'static str,
}

impl Runners {
    /// Actions run in sandboxes on this machine.
    pub const LOCAL: Runners = Runners {
        run: "linux-sandbox",
        worker: "worker",
        cache_hit: "disk cache hit",
    };

    /// Actions run by a remote execution service.
    pub const REMOTE: Runners = Runners {
        run: "remote",
        worker: "remote",
        cache_hit: "remote cache hit",
    };

    /// How an action is run: in a persistent worker or not.
    pub fn running(&self, in_worker: bool) -> &'static str {
        match in_worker {
            true => self.worker,
            false => self.run,
        }
    }

    /// How an action's result was got: from the cache, or by running it.
    pub fn name(&self, cached: bool, in_worker: bool) -> &'static str {
        match cached {
            true => self.cache_hit,
            false => self.running(in_worker),
        }
    }
}

impl Backend {
    /// A backend that prepares at most `max_staged` actions at once: builds
    /// their input trees and uploads their inputs, and runs them.
    pub fn new(cas: Arc<dyn Cas>, action_cache: Arc<dyn ActionCache>, executor: Arc<dyn Executor>, runners: Runners, max_staged: usize) -> Backend {
        Backend {
            cas,
            action_cache,
            executor,
            runners,
            persistent_workers: runners == Runners::LOCAL,
            staging: Semaphore::new(max_staged.max(1)),
            uploads: Mutex::default(),
        }
    }

    /// The same backend, marking actions for persistent workers or not, as
    /// a remote executor's config says.
    pub fn with_persistent_workers(mut self, persistent_workers: bool) -> Backend {
        self.persistent_workers = persistent_workers;
        self
    }

    /// Put the source file at `source` in the CAS as `digest`, or wait for
    /// the upload of it already under way.
    pub async fn upload_source(&self, digest: BlobDigest, source: PathBuf) -> Result<(), cas::CasError> {
        let upload = self.uploads().entry(digest).or_default().clone();
        let uploaded = upload.get_or_try_init(|| async { self.cas.put_file(&source).await.map(|_| ()) }).await;
        self.uploads().remove(&digest);
        uploaded.copied()
    }

    fn uploads(&self) -> MutexGuard<'_, HashMap<BlobDigest, Arc<OnceCell<()>>>> {
        self.uploads.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
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
