use proto::bazel::asset::{
    Fetch, FetchBlobRequest, FetchBlobResponse, FetchDirectoryRequest, FetchDirectoryResponse,
};
use tonic::{Request, Response, Status};

#[derive(Default, Debug)]
pub struct FetchService {}

#[async_trait::async_trait]
impl Fetch for FetchService {
    async fn fetch_blob(
        &self,
        _req: Request<FetchBlobRequest>,
    ) -> Result<Response<FetchBlobResponse>, Status> {
        tracing::info!("FetchService::fetch_blob");
        Ok(Response::new(FetchBlobResponse::default()))
    }

    async fn fetch_directory(
        &self,
        _req: Request<FetchDirectoryRequest>,
    ) -> Result<Response<FetchDirectoryResponse>, Status> {
        tracing::info!("FetchService::fetch_directory");
        Ok(Response::new(FetchDirectoryResponse::default()))
    }
}
