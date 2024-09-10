use super::ResponseStream;
use crate::proto::remote_execution::{
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, ContentAddressableStorage, FindMissingBlobsRequest,
    FindMissingBlobsResponse, GetTreeRequest, GetTreeResponse,
};
use tonic::{Request, Response, Status};

#[derive(Default, Debug)]
pub struct ContentAddressableStorageService {}

#[tonic::async_trait]
impl ContentAddressableStorage for ContentAddressableStorageService {
    async fn find_missing_blobs(
        &self,
        _req: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        tracing::info!("ContentAddressableStorage::find_missing_blobs");
        Ok(Response::new(FindMissingBlobsResponse::default()))
    }

    async fn batch_update_blobs(
        &self,
        _req: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        tracing::info!("ContentAddressableStorage::batch_update_blobs");
        Ok(Response::new(BatchUpdateBlobsResponse::default()))
    }

    async fn batch_read_blobs(
        &self,
        _req: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        tracing::info!("ContentAddressableStorage::batch_read_blobs");
        Ok(Response::new(BatchReadBlobsResponse::default()))
    }

    type GetTreeStream = ResponseStream<Result<GetTreeResponse, Status>>;

    async fn get_tree(
        &self,
        _req: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        tracing::info!("ContentAddressableStorage::get_tree");
        todo!()
    }
}
