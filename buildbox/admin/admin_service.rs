//! The admin service, for inspecting and managing a running server.

use actioncache::ActionCache;
use admin_proto::buildbox::admin::v1::admin_server::Admin;
use admin_proto::buildbox::admin::v1::{
    ActionInfo, ActionState, ExecutionStatus, GetStatusRequest, GetStatusResponse, ListActionsRequest,
    ListActionsResponse, StorageUsage,
};
use cas::Cas;
use scheduler::Scheduler;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tonic::{Request, Response, Status};

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

    async fn cas_usage(&self) -> Result<StorageUsage, Status> {
        let usage = self.cas.usage().await.map_err(|error| Status::internal(error.to_string()))?;
        Ok(StorageUsage {
            location: self.cas_location.clone(),
            entries: usage.blobs,
            bytes: usage.bytes,
        })
    }

    async fn action_cache_usage(&self) -> Result<StorageUsage, Status> {
        let usage = self
            .action_cache
            .usage()
            .await
            .map_err(|error| Status::internal(error.to_string()))?;
        Ok(StorageUsage {
            location: self.action_cache_location.clone(),
            entries: usage.entries,
            bytes: usage.bytes,
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
        }
    }
}

#[tonic::async_trait]
impl Admin for AdminService {
    async fn get_status(&self, _request: Request<GetStatusRequest>) -> Result<Response<GetStatusResponse>, Status> {
        let response = GetStatusResponse {
            version: VERSION.to_string(),
            uptime_seconds: self.started.elapsed().as_secs(),
            cas: Some(self.cas_usage().await?),
            action_cache: Some(self.action_cache_usage().await?),
            execution: Some(self.execution_status()),
        };
        Ok(Response::new(response))
    }

    async fn list_actions(
        &self,
        _request: Request<ListActionsRequest>,
    ) -> Result<Response<ListActionsResponse>, Status> {
        let now = SystemTime::now();
        let actions = self
            .scheduler
            .actions()
            .into_iter()
            .map(|action| action_info(action, now))
            .collect();
        Ok(Response::new(ListActionsResponse { actions }))
    }
}

fn action_info(action: scheduler::ActionInfo, now: SystemTime) -> ActionInfo {
    let state = match action.state {
        scheduler::ActionState::Queued => ActionState::Queued,
        scheduler::ActionState::Running => ActionState::Running,
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
    use remote_execution_proto::build::bazel::remote::execution::v2::{ActionResult, Command};
    use tokio::sync::Notify;

    /// An executor whose actions run until released.
    #[derive(Default)]
    struct BlockingExecutor {
        release: Notify,
    }

    #[tonic::async_trait]
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

        let status = service.get_status(Request::new(GetStatusRequest {})).await.unwrap().into_inner();

        let cas = status.cas.unwrap();
        assert_eq!(cas.location, "/cas");
        // The empty blob and "hello".
        assert_eq!((cas.entries, cas.bytes), (2, 5));
        assert_eq!(status.action_cache.unwrap().entries, 0);
        assert_eq!(
            status.execution.unwrap(),
            ExecutionStatus {
                max_concurrent_actions: 1,
                running_actions: 1,
                queued_actions: 1,
            }
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

        let response = service.list_actions(Request::new(ListActionsRequest {})).await.unwrap().into_inner();

        let info = &response.actions[0];
        assert_eq!(info.action_digest, cas::Digest::of_bytes(b"compile").to_string());
        assert_eq!(info.state, ActionState::Running as i32);
        assert_eq!(info.arguments, ["compile", "--flag"]);

        release_until_finished(&executor, task).await;
    }
}
