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
        req: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        let req = req.into_inner();
        tracing::info!("ContentAddressableStorage::find_missing_blobs");

        for digest in &req.blob_digests {
          let hash = &digest.hash;
          tracing::info!("find blob: {hash}");
        }

        // let req = req.into_inner();
        // let hash = req.action_digest.unwrap().hash;
        // tracing::info!("ActionCache::get_action_result {hash}");

        // Ok(Response::new(FindMissingBlobsResponse::default()))
        Ok(Response::new(FindMissingBlobsResponse {
          missing_blob_digests: req.blob_digests,
        }))
    }

    async fn batch_update_blobs(
        &self,
        req: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        let req = req.into_inner();
        tracing::info!("ContentAddressableStorage::batch_update_blobs {req:?}");
        Ok(Response::new(BatchUpdateBlobsResponse::default()))
    }

    async fn batch_read_blobs(
        &self,
        req: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        let req = req.into_inner();
        tracing::info!("ContentAddressableStorage::batch_read_blobs {req:?}");
        Ok(Response::new(BatchReadBlobsResponse::default()))
    }

    type GetTreeStream = ResponseStream<Result<GetTreeResponse, Status>>;

    async fn get_tree(
        &self,
        req: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        let req = req.into_inner();
        tracing::info!("ContentAddressableStorage::get_tree {req:?}");
        todo!()
    }
}
