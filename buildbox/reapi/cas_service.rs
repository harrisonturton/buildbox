//! The ContentAddressableStorage service: finding, uploading and downloading
//! blobs in bulk.

use crate::status;
use crate::ResponseStream;
use cas::{Cas, Digest};
use remote_execution_proto::build::bazel::remote::execution::v2::content_addressable_storage_server::ContentAddressableStorage;
use remote_execution_proto::build::bazel::remote::execution::v2::{
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest, BatchUpdateBlobsResponse,
    FindMissingBlobsRequest, FindMissingBlobsResponse, GetTreeRequest, GetTreeResponse,
};
use std::sync::Arc;
use tonic::{Request, Response, Status};

pub struct ContentAddressableStorageService {
    cas: Arc<dyn Cas>,
}

impl ContentAddressableStorageService {
    pub fn new(cas: Arc<dyn Cas>) -> ContentAddressableStorageService {
        ContentAddressableStorageService { cas }
    }
}

#[tonic::async_trait]
impl ContentAddressableStorage for ContentAddressableStorageService {
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        let request = request.into_inner();

        let mut digests = Vec::with_capacity(request.blob_digests.len());
        for proto in &request.blob_digests {
            let digest = Digest::from_proto(proto).map_err(|error| status::invalid_digest("blob_digests", error))?;
            digests.push(digest);
        }

        let missing = self.cas.find_missing(&digests).await.map_err(status::from_cas_error)?;
        let missing_blob_digests = missing.iter().map(Digest::to_proto).collect();
        Ok(Response::new(FindMissingBlobsResponse { missing_blob_digests }))
    }

    async fn batch_update_blobs(
        &self,
        _request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        Err(Status::unimplemented("BatchUpdateBlobs is not supported yet"))
    }

    async fn batch_read_blobs(
        &self,
        _request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        Err(Status::unimplemented("BatchReadBlobs is not supported yet"))
    }

    type GetTreeStream = ResponseStream<Result<GetTreeResponse, Status>>;

    async fn get_tree(&self, _request: Request<GetTreeRequest>) -> Result<Response<Self::GetTreeStream>, Status> {
        Err(Status::unimplemented("GetTree is not supported yet"))
    }
}
