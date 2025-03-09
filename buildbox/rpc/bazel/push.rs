use proto::bazel::asset::{
    Push, PushBlobRequest, PushBlobResponse, PushDirectoryRequest, PushDirectoryResponse,
};
use tonic::{Request, Response, Status};

#[derive(Default, Debug)]
pub struct PushService {}

#[async_trait::async_trait]
impl Push for PushService {
    async fn push_blob(
        &self,
        _req: Request<PushBlobRequest>,
    ) -> Result<Response<PushBlobResponse>, Status> {
        // tracing::info!("PushService::push_blob");
        // Ok(Response::new(PushBlobResponse::default()))
        tracing::error!("unimplemented endpoint push_blob invoked, stopping");
        std::process::exit(1)
    }

    async fn push_directory(
        &self,
        _req: Request<PushDirectoryRequest>,
    ) -> Result<Response<PushDirectoryResponse>, Status> {
        // tracing::info!("PushService::push_directory");
        // Ok(Response::new(PushDirectoryResponse::default()))
        tracing::error!("unimplemented endpoint push_directory invoked, stopping");
        std::process::exit(1)
    }
}
