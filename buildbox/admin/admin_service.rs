//! The admin service, for inspecting and managing a running server.

use actioncache::ActionCache;
use admin_rust_connect::buildbox::admin::v1::{
    ActionInfo, ActionState, Admin, Artifact, ArtifactKind, DeleteArtifactRequest, DeleteArtifactResponse,
    ExecutionStatus, GetStatusRequest, GetStatusResponse, ImportContainerImageRequest, ImportContainerImageResponse,
    ListActionsRequest, ListActionsResponse, ListArtifactsRequest, ListArtifactsResponse, StorageUsage,
};
use artifacts::{ArtifactError, ArtifactInfo, ArtifactStore};
use cas::{Cas, Digest};
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use scheduler::Scheduler;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The parts of the server the admin service reports on and manages, and
/// where they keep their data.
pub struct Components {
    pub cas: Arc<dyn Cas>,
    pub cas_location: String,
    pub action_cache: Arc<dyn ActionCache>,
    pub action_cache_location: String,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub artifacts_location: String,
    pub scheduler: Arc<Scheduler>,
}

pub struct AdminService {
    started: Instant,
    components: Components,
}

impl AdminService {
    pub fn new(components: Components) -> AdminService {
        AdminService {
            started: Instant::now(),
            components,
        }
    }

    async fn status(&self) -> Result<GetStatusResponse, ConnectError> {
        Ok(GetStatusResponse {
            version: VERSION.to_string(),
            uptime_seconds: self.started.elapsed().as_secs(),
            cas: self.cas_usage().await?.into(),
            action_cache: self.action_cache_usage().await?.into(),
            execution: self.execution_status().into(),
            artifacts: self.artifacts_usage().await?.into(),
            ..Default::default()
        })
    }

    async fn cas_usage(&self) -> Result<StorageUsage, ConnectError> {
        let usage = self.components.cas.usage().await.map_err(|error| ConnectError::internal(error.to_string()))?;
        Ok(StorageUsage {
            location: self.components.cas_location.clone(),
            entries: usage.blobs,
            bytes: usage.bytes,
            ..Default::default()
        })
    }

    async fn action_cache_usage(&self) -> Result<StorageUsage, ConnectError> {
        let usage = self
            .components
            .action_cache
            .usage()
            .await
            .map_err(|error| ConnectError::internal(error.to_string()))?;
        Ok(StorageUsage {
            location: self.components.action_cache_location.clone(),
            entries: usage.entries,
            bytes: usage.bytes,
            ..Default::default()
        })
    }

    fn execution_status(&self) -> ExecutionStatus {
        let actions = self.components.scheduler.actions();
        let running = actions
            .iter()
            .filter(|action| action.state == scheduler::ActionState::Running)
            .count();

        ExecutionStatus {
            max_concurrent_actions: saturating_u32(self.components.scheduler.max_concurrent()),
            running_actions: saturating_u32(running),
            queued_actions: saturating_u32(actions.len() - running),
            ..Default::default()
        }
    }

    fn actions(&self) -> ListActionsResponse {
        let now = SystemTime::now();
        let actions = self
            .components
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

    async fn artifacts_usage(&self) -> Result<StorageUsage, ConnectError> {
        let usage = self.components.artifacts.usage().await.map_err(artifact_error)?;
        Ok(StorageUsage {
            location: self.components.artifacts_location.clone(),
            entries: usage.artifacts,
            bytes: usage.bytes,
            ..Default::default()
        })
    }

    async fn import_container_image(
        &self,
        request: ImportContainerImageRequest,
    ) -> Result<ImportContainerImageResponse, ConnectError> {
        let manifest: Digest = request
            .manifest_digest
            .parse()
            .map_err(|error| ConnectError::invalid_argument(format!("manifest_digest: {error}")))?;

        tracing::info!("importing container image {:?} from {manifest}", request.name);
        let artifacts = &self.components.artifacts;
        let info = artifacts
            .import_container_image(&request.name, &manifest)
            .await
            .map_err(artifact_error)?;
        Ok(ImportContainerImageResponse {
            artifact: artifact(info).into(),
            ..Default::default()
        })
    }

    async fn list_artifacts(&self) -> Result<ListArtifactsResponse, ConnectError> {
        let infos = self.components.artifacts.list().await.map_err(artifact_error)?;
        Ok(ListArtifactsResponse {
            artifacts: infos.into_iter().map(artifact).collect(),
            ..Default::default()
        })
    }

    async fn delete_artifact(&self, request: DeleteArtifactRequest) -> Result<DeleteArtifactResponse, ConnectError> {
        tracing::info!("deleting artifact {:?}", request.name);
        self.components
            .artifacts
            .delete(&request.name)
            .await
            .map_err(artifact_error)?;
        Ok(DeleteArtifactResponse::default())
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

    async fn import_container_image(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, ImportContainerImageRequest>,
    ) -> ServiceResult<ImportContainerImageResponse> {
        Response::ok(self.import_container_image(request.to_owned_message()).await?)
    }

    async fn list_artifacts(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListArtifactsRequest>,
    ) -> ServiceResult<ListArtifactsResponse> {
        Response::ok(self.list_artifacts().await?)
    }

    async fn delete_artifact(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, DeleteArtifactRequest>,
    ) -> ServiceResult<DeleteArtifactResponse> {
        Response::ok(self.delete_artifact(request.to_owned_message()).await?)
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
        environment: action.environment,
        queued_millis: millis_since(action.queued_at, now),
        running_millis,
        ..Default::default()
    }
}

fn artifact(info: ArtifactInfo) -> Artifact {
    let kind = match info.kind {
        artifacts::ArtifactKind::ContainerImage => ArtifactKind::ARTIFACT_KIND_CONTAINER_IMAGE,
    };
    Artifact {
        name: info.name,
        kind: kind.into(),
        digest: info.digest.to_string(),
        size_bytes: info.size_bytes,
        imported_at_unix_millis: millis_since(UNIX_EPOCH, info.imported_at),
        ..Default::default()
    }
}

fn artifact_error(error: ArtifactError) -> ConnectError {
    let message = error.to_string();
    match error {
        ArtifactError::NotFound(_) => ConnectError::not_found(message),
        ArtifactError::InvalidName(_) | ArtifactError::WrongKind { .. } | ArtifactError::InvalidImage(_) => {
            ConnectError::invalid_argument(message)
        }
        ArtifactError::MissingBlobs(_) => ConnectError::failed_precondition(message),
        ArtifactError::Io { .. } | ArtifactError::Cas(_) => ConnectError::internal(message),
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
    use std::collections::BTreeMap;
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
            platform: vec![],
        }
    }

    async fn service(executor: Arc<BlockingExecutor>, max_concurrent: usize) -> (AdminService, Arc<Scheduler>) {
        let cas = Arc::new(MemCas::new());
        cas.put_bytes(b"hello").await.unwrap();
        let environments = BTreeMap::from([(scheduler::DEFAULT_ENVIRONMENT.to_string(), executor as Arc<dyn Executor>)]);
        let scheduler = Arc::new(Scheduler::new(environments, max_concurrent));
        let service = AdminService::new(Components {
            cas,
            cas_location: "/cas".to_string(),
            action_cache: Arc::new(MemActionCache::new()),
            action_cache_location: "/ac".to_string(),
            artifacts: Arc::new(FakeArtifactStore::default()),
            artifacts_location: "/artifacts".to_string(),
            scheduler: scheduler.clone(),
        });
        (service, scheduler)
    }

    /// Imports any image, as an artifact of size 1, without looking at it.
    #[derive(Default)]
    struct FakeArtifactStore {
        artifacts: std::sync::Mutex<BTreeMap<String, ArtifactInfo>>,
    }

    #[async_trait::async_trait]
    impl ArtifactStore for FakeArtifactStore {
        async fn import_container_image(&self, name: &str, manifest: &Digest) -> Result<ArtifactInfo, ArtifactError> {
            artifacts::validate_name(name)?;
            let info = ArtifactInfo {
                name: name.to_string(),
                kind: artifacts::ArtifactKind::ContainerImage,
                digest: manifest.clone(),
                size_bytes: 1,
                imported_at: UNIX_EPOCH + Duration::from_secs(1),
            };
            self.artifacts.lock().unwrap().insert(name.to_string(), info.clone());
            Ok(info)
        }

        async fn list(&self) -> Result<Vec<ArtifactInfo>, ArtifactError> {
            Ok(self.artifacts.lock().unwrap().values().cloned().collect())
        }

        async fn delete(&self, name: &str) -> Result<(), ArtifactError> {
            match self.artifacts.lock().unwrap().remove(name) {
                Some(_) => Ok(()),
                None => Err(ArtifactError::NotFound(name.to_string())),
            }
        }

        async fn container_image(&self, name: &str) -> Result<artifacts::ContainerImage, ArtifactError> {
            Err(ArtifactError::NotFound(name.to_string()))
        }

        async fn usage(&self) -> Result<artifacts::Usage, ArtifactError> {
            let artifacts = self.artifacts.lock().unwrap().len() as u64;
            Ok(artifacts::Usage {
                artifacts,
                bytes: artifacts,
            })
        }
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
        assert_eq!(info.environment, scheduler::DEFAULT_ENVIRONMENT);

        release_until_finished(&executor, task).await;
    }

    fn import_request(name: &str, manifest_digest: &str) -> ImportContainerImageRequest {
        ImportContainerImageRequest {
            name: name.to_string(),
            manifest_digest: manifest_digest.to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn imports_lists_and_deletes_artifacts() {
        let (service, _scheduler) = service(Arc::new(BlockingExecutor::default()), 1).await;
        let manifest = Digest::of_bytes(b"manifest");

        let imported = service
            .import_container_image(import_request("image:1", &manifest.to_string()))
            .await
            .unwrap();
        let listed = service.list_artifacts().await.unwrap();
        let status = service.status().await.unwrap();

        let artifact = imported.artifact.as_option().unwrap();
        assert_eq!(artifact.name, "image:1");
        assert_eq!(artifact.kind.as_known(), Some(ArtifactKind::ARTIFACT_KIND_CONTAINER_IMAGE));
        assert_eq!(artifact.digest, manifest.to_string());
        assert_eq!(artifact.imported_at_unix_millis, 1000);
        assert_eq!(listed.artifacts, [artifact.clone()]);
        assert_eq!(status.artifacts.as_option().unwrap().location, "/artifacts");
        assert_eq!(status.artifacts.entries, 1);

        let delete = DeleteArtifactRequest {
            name: "image:1".to_string(),
            ..Default::default()
        };
        service.delete_artifact(delete.clone()).await.unwrap();
        let err = service.delete_artifact(delete).await.unwrap_err();
        assert_eq!(err.code, connectrpc::ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn rejects_invalid_imports() {
        let (service, _scheduler) = service(Arc::new(BlockingExecutor::default()), 1).await;
        let manifest = Digest::of_bytes(b"manifest").to_string();

        let bad_digest = service.import_container_image(import_request("image", "nonsense")).await;
        let bad_name = service.import_container_image(import_request("../image", &manifest)).await;

        assert_eq!(bad_digest.unwrap_err().code, connectrpc::ErrorCode::InvalidArgument);
        assert_eq!(bad_name.unwrap_err().code, connectrpc::ErrorCode::InvalidArgument);
    }
}
