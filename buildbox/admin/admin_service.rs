use proto::buildbox::{Buildbox, FindBlobsRequest, FindBlobsResponse, FindSandboxesRequest, FindSandboxesResponse};
use tonic::{Request, Response, Status};

#[derive(Debug, Default)]
pub struct BuildboxService {}

#[tonic::async_trait]
impl Buildbox for BuildboxService {
    async fn find_blobs(&self, _request: Request<FindBlobsRequest>) -> Result<Response<FindBlobsResponse>, Status> {
        Err(Status::unimplemented("FindBlobs is not supported yet"))
    }

    async fn find_sandboxes(
        &self,
        _request: Request<FindSandboxesRequest>,
    ) -> Result<Response<FindSandboxesResponse>, Status> {
        Err(Status::unimplemented("FindSandboxes is not supported yet"))
    }
}
