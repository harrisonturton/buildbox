//! The limit a build's `--jobs` puts on how many actions run at once.

use skyframe::{Ctx, SkyError};
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// How many of a workspace's actions may run at once, as its latest
/// build's `--jobs` says, beneath the server's own limit. Bazel runs one
/// command at a time; builds here may overlap, and the latest one's limit
/// applies to actions that haven't started yet.
#[derive(Default)]
pub struct Jobs {
    limit: Mutex<Option<(usize, Arc<Semaphore>)>>,
}

/// A turn to run an action, held while it runs.
pub struct JobTurn {
    _permit: Option<OwnedSemaphorePermit>,
}

impl Jobs {
    /// Let at most `jobs` actions run at once from now on, or as many as
    /// the server allows if `None`. Actions already running keep their
    /// turns, and builds asking for the same limit share it.
    pub fn set(&self, jobs: Option<usize>) {
        let mut limit = self.limit.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if limit.as_ref().map(|(current, _)| *current) == jobs {
            return;
        }
        *limit = jobs.map(|jobs| (jobs, Arc::new(Semaphore::new(jobs.max(1)))));
    }

    /// Wait for a turn to run an action under the current limit.
    pub async fn turn(&self) -> Result<JobTurn, SkyError> {
        let semaphore = {
            let limit = self.limit.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            limit.as_ref().map(|(_, semaphore)| semaphore.clone())
        };
        let Some(semaphore) = semaphore else {
            return Ok(JobTurn { _permit: None });
        };
        let span = tracing::debug_span!("jobs_wait");
        let permit = tracing::Instrument::instrument(semaphore.acquire_owned(), span).await;
        let permit = permit.map_err(|_| SkyError::transient("the build's job limit was closed"))?;
        Ok(JobTurn { _permit: Some(permit) })
    }
}

/// Wait for a turn to run an action in `ctx`'s workspace, if its graph has
/// a job limit.
pub async fn job_turn(ctx: &Ctx) -> Result<JobTurn, SkyError> {
    match ctx.global::<Jobs>() {
        Ok(jobs) => jobs.turn().await,
        Err(_) => Ok(JobTurn { _permit: None }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    #[tokio::test]
    async fn runs_at_most_the_limit_at_once() {
        let jobs = Jobs::default();
        jobs.set(Some(2));
        let first = jobs.turn().await.unwrap();
        let _second = jobs.turn().await.unwrap();
        let mut third = Box::pin(jobs.turn());
        assert!((&mut third).now_or_never().is_none());
        drop(first);
        assert!(third.now_or_never().is_some());
    }

    #[tokio::test]
    async fn a_new_limit_applies_to_actions_yet_to_start() {
        let jobs = Jobs::default();
        jobs.set(Some(1));
        let _running = jobs.turn().await.unwrap();
        jobs.set(None);
        assert!(jobs.turn().now_or_never().is_some());
        jobs.set(Some(1));
        let _next = jobs.turn().await.unwrap();
        assert!(jobs.turn().now_or_never().is_none());
    }

    #[tokio::test]
    async fn the_same_limit_is_shared() {
        let jobs = Jobs::default();
        jobs.set(Some(1));
        let _running = jobs.turn().await.unwrap();
        jobs.set(Some(1));
        assert!(jobs.turn().now_or_never().is_none());
    }
}
