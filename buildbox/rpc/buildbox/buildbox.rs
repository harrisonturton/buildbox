use common::Error;
use proto::buildbox::{
    Buildbox, FindBlobsRequest, FindBlobsResponse, FindSandboxesRequest, FindSandboxesResponse,
};
use sandbox::Sandbox;
use storage::Storage;
use tonic::{Request, Response, Status};

#[derive(Debug)]
pub struct BuildboxService {
    storage: Storage,
    sandbox: Sandbox,
}

impl BuildboxService {
    pub fn new(storage: Storage, sandbox: Sandbox) -> Self {
        Self { storage, sandbox }
    }
}

#[async_trait::async_trait]
impl Buildbox for BuildboxService {
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
