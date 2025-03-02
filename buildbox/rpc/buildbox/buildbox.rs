use common::Error;
use executor::{Executor, SandboxHandle};
use proto::buildbox::{
    Buildbox, FindBlobsRequest, FindBlobsResponse, FindSandboxesRequest, FindSandboxesResponse,
};
use storage::FileStore;
use storage::Store;
use tonic::{Request, Response, Status};

#[derive(Debug)]
pub struct BuildboxService<S, E>
where
    S: Store + 'static,
    E: Executor + 'static,
{
    storage: S,
    executor: E,
}

impl<S, E> BuildboxService<S, E>
where
    S: Store + 'static,
    E: Executor + 'static,
{
    pub fn new(storage: S, executor: E) -> Self {
        Self { storage, executor }
    }
}

#[async_trait::async_trait]
impl<S, E> Buildbox for BuildboxService<S, E>
where
    S: Store + 'static,
    E: Executor + 'static,
{
    async fn find_blobs(
        &self,
        req: Request<FindBlobsRequest>,
    ) -> Result<Response<FindBlobsResponse>, Status> {
        tracing::info!("BuildboxService::find_blobs");

        let blobs = self
            .storage
            .list()
            .map_err(|err| Status::internal(err.to_string()))?;

        Ok(Response::new(FindBlobsResponse { blobs }))
    }

    async fn find_sandboxes(
        &self,
        req: Request<FindSandboxesRequest>,
    ) -> Result<Response<FindSandboxesResponse>, Status> {
        tracing::info!("BuildboxService::find_sandboxes");
        Ok(Response::new(FindSandboxesResponse {
            sandboxes: vec!["one".to_string(), "two".to_string()],
        }))
    }
}
