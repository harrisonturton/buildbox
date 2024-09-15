use super::ResponseStream;
use crate::{
    blob::LocalBlobStore,
    proto::remote_execution::{
        BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
        BatchUpdateBlobsResponse, ContentAddressableStorage, FindMissingBlobsRequest,
        FindMissingBlobsResponse, GetTreeRequest, GetTreeResponse,
    },
};
use tonic::{Request, Response, Status};

#[derive(Debug)]
pub struct ContentAddressableStorageService {
    store: LocalBlobStore,
}

impl ContentAddressableStorageService {
    /// Create new instance of [`ContentAddressableStorageService`].
    #[must_use]
    pub fn new(store: LocalBlobStore) -> Self {
        Self { store }
    }
}

#[tonic::async_trait]
impl ContentAddressableStorage for ContentAddressableStorageService {
    async fn find_missing_blobs(
        &self,
        req: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        let req = req.into_inner();
        let mut missing = vec![];

        for digest in &req.blob_digests {
            let hash = &digest.hash;

            let exists = match self.store.contains(hash) {
                Ok(exists) => exists,
                Err(err) => return Err(Status::internal(err.to_string())),
            };

            if !exists {
                tracing::info!("ContentAddressableStorage::find_missing_blobs missing hash={hash}");
                missing.push(digest.clone());
            } else {
                tracing::info!("ContentAddressableStorage::find_missing_blobs found hash={hash}");
            }
        }

        Ok(Response::new(FindMissingBlobsResponse {
            missing_blob_digests: missing,
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
