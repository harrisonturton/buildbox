//! The admin service, for inspecting and managing a running server.

use actioncache::ActionCache;
use admin_rust_connect::buildbox::admin::v1::{
    ActionInfo, ActionState, Admin, ExecutionStatus, GetStatusRequest, GetStatusResponse, ListActionsRequest,
    ListActionsResponse, StorageUsage,
};
use cas::Cas;
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use scheduler::Scheduler;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The parts of the server the admin service reports on, and where they keep
/// their data.
pub struct AdminService {
    started: Instant,
    cas: Arc<dyn Cas>,
    cas_location: String,
    action_cache: Arc<dyn ActionCache>,
    action_cache_location: String,
    scheduler: Arc<Scheduler>,
}

impl AdminService {
    pub fn new(
        cas: Arc<dyn Cas>,
        cas_location: String,
        action_cache: Arc<dyn ActionCache>,
        action_cache_location: String,
        scheduler: Arc<Scheduler>,
    ) -> AdminService {
        AdminService {
            started: Instant::now(),
            cas,
            cas_location,
            action_cache,
            action_cache_location,
            scheduler,
        }
    }

    async fn status(&self) -> Result<GetStatusResponse, ConnectError> {
        Ok(GetStatusResponse {
            version: VERSION.to_string(),
            uptime_seconds: self.started.elapsed().as_secs(),
            cas: self.cas_usage().await?.into(),
            action_cache: self.action_cache_usage().await?.into(),
            execution: self.execution_status().into(),
            ..Default::default()
        })
    }

    async fn cas_usage(&self) -> Result<StorageUsage, ConnectError> {
        let usage = self.cas.usage().await.map_err(|error| ConnectError::internal(error.to_string()))?;
        Ok(StorageUsage {
            location: self.cas_location.clone(),
            entries: usage.blobs,
            bytes: usage.bytes,
            ..Default::default()
        })
    }

    async fn action_cache_usage(&self) -> Result<StorageUsage, ConnectError> {
        let usage = self
            .action_cache
            .usage()
            .await
            .map_err(|error| ConnectError::internal(error.to_string()))?;
        Ok(StorageUsage {
            location: self.action_cache_location.clone(),
            entries: usage.entries,
            bytes: usage.bytes,
            ..Default::default()
        })
    }

    fn execution_status(&self) -> ExecutionStatus {
        let actions = self.scheduler.actions();
        let running = actions
            .iter()
            .filter(|action| action.state == scheduler::ActionState::Running)
            .count();

        ExecutionStatus {
            max_concurrent_actions: saturating_u32(self.scheduler.max_concurrent()),
            running_actions: saturating_u32(running),
            queued_actions: saturating_u32(actions.len() - running),
            ..Default::default()
        }
    }

    fn actions(&self) -> ListActionsResponse {
        let now = SystemTime::now();
        let actions = self
            .scheduler
            .actions()
            .into_iter()
            .map(|action| action_info(action, now))
            .collect();
        ListActionsResponse {
            actions,
            ..Default::default()
        }
    }
}

// Generated traits return `impl Encodable<M>`; handlers narrow that to `M`.
#[allow(refining_impl_trait)]
impl Admin for AdminService {
    async fn get_status(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetStatusRequest>,
    ) -> ServiceResult<GetStatusResponse> {
        Response::ok(self.status().await?)
    }

    async fn list_actions(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListActionsRequest>,
    ) -> ServiceResult<ListActionsResponse> {
        Response::ok(self.actions())
    }
}

fn action_info(action: scheduler::ActionInfo, now: SystemTime) -> ActionInfo {
    let state = match action.state {
        scheduler::ActionState::Queued => ActionState::ACTION_STATE_QUEUED,
        scheduler::ActionState::Running => ActionState::ACTION_STATE_RUNNING,
    };
    let running_millis = match action.started_at {
        Some(started_at) => millis_since(started_at, now),
        None => 0,
    };

    ActionInfo {
        id: action.id,
        action_digest: action.digest.to_string(),
        state: state.into(),
        arguments: action.arguments,
        queued_millis: millis_since(action.queued_at, now),
        running_millis,
        ..Default::default()
    }
}

fn millis_since(earlier: SystemTime, now: SystemTime) -> u64 {
    let elapsed = now.duration_since(earlier).unwrap_or(Duration::ZERO);
    elapsed.as_millis().try_into().unwrap_or(u64::MAX)
}

fn saturating_u32(n: usize) -> u32 {
    n.try_into().unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actioncache_mem::MemActionCache;
    use cas_mem::MemCas;
    use executor::{Action, ExecuteError, Executor};
    use remote_execution_rust_connect::build::bazel::remote::execution::v2::{ActionResult, Command};
    use tokio::sync::Notify;

    /// An executor whose actions run until released.
    #[derive(Default)]
    struct BlockingExecutor {
        release: Notify,
    }

    #[async_trait::async_trait]
    impl Executor for BlockingExecutor {
        async fn execute(&self, _action: &Action) -> Result<ActionResult, ExecuteError> {
            self.release.notified().await;
            Ok(ActionResult::default())
        }
    }

    fn action(name: &str) -> Action {
        Action {
            digest: cas::Digest::of_bytes(name.as_bytes()),
            command: Command {
                arguments: vec![name.to_string(), "--flag".to_string()],
                ..Default::default()
            },
            input_root: cas::Digest::empty(),
            timeout: None,
        }
    }

    async fn service(executor: Arc<BlockingExecutor>, max_concurrent: usize) -> (AdminService, Arc<Scheduler>) {
        let cas = Arc::new(MemCas::new());
        cas.put_bytes(b"hello").await.unwrap();
        let scheduler = Arc::new(Scheduler::new(executor, max_concurrent));
        let service = AdminService::new(
            cas,
            "/cas".to_string(),
            Arc::new(MemActionCache::new()),
            "/ac".to_string(),
            scheduler.clone(),
        );
        (service, scheduler)
    }

    async fn wait_for_actions(scheduler: &Scheduler, count: usize) {
        for _ in 0..500 {
            if scheduler.actions().len() == count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("expected {count} actions");
    }

    async fn release_until_finished(executor: &BlockingExecutor, task: tokio::task::JoinHandle<Result<ActionResult, ExecuteError>>) {
        while !task.is_finished() {
            executor.release.notify_waiters();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn reports_storage_and_execution_status() {
        let executor = Arc::new(BlockingExecutor::default());
        let (service, scheduler) = service(executor.clone(), 1).await;
        let mut tasks = vec![];
        for name in ["a", "b"] {
            let scheduler = scheduler.clone();
            tasks.push(tokio::spawn(async move { scheduler.execute(&action(name)).await }));
        }
        wait_for_actions(&scheduler, 2).await;

        let status = service.status().await.unwrap();

        let cas = status.cas.as_option().unwrap();
        assert_eq!(cas.location, "/cas");
        // The empty blob and "hello".
        assert_eq!((cas.entries, cas.bytes), (2, 5));
        assert_eq!(status.action_cache.entries, 0);
        assert_eq!(
            status.execution.as_option(),
            Some(&ExecutionStatus {
                max_concurrent_actions: 1,
                running_actions: 1,
                queued_actions: 1,
                ..Default::default()
            })
        );

        for task in tasks {
            release_until_finished(&executor, task).await;
        }
    }

    #[tokio::test]
    async fn lists_running_actions() {
        let executor = Arc::new(BlockingExecutor::default());
        let (service, scheduler) = service(executor.clone(), 1).await;
        let task = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.execute(&action("compile")).await })
        };
        wait_for_actions(&scheduler, 1).await;

        let response = service.actions();

        let info = &response.actions[0];
        assert_eq!(info.action_digest, cas::Digest::of_bytes(b"compile").to_string());
        assert_eq!(info.state.as_known(), Some(ActionState::ACTION_STATE_RUNNING));
        assert_eq!(info.arguments, ["compile", "--flag"]);

        release_until_finished(&executor, task).await;
    }
}
