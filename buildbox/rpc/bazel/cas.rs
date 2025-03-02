use super::ResponseStream;
use proto::bazel::exec::{
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, ContentAddressableStorage, FindMissingBlobsRequest,
    FindMissingBlobsResponse, GetTreeRequest, GetTreeResponse,
};
use storage::Store;
use tonic::{Request, Response, Status};

/// The CAS (content-addressable storage) is used to store the inputs to and
/// outputs from the execution service. Each piece of content is addressed by
/// the digest of its binary data.
///
/// Most of the binary data stored in the CAS is opaque to the execution engine,
/// and is only used as a communication medium. In order to build an `Action`,
/// however, the client will need to also upload  the `Command` and input root
/// `Directory` for the `Action`.
///
/// The `Command` and `Directory` messages must be marshalled to wire format and
/// then uploaded under the hash as with any other piece of content. In
/// practice, the input root directory is likely to refer to other directories
/// in it's hierarchy, which must also each by uploaded on their own.
///
/// For small uploads the client should group them together and call
/// `batchUpdateBlobs`.
///
/// For larger uploads, the client must use the `Write` method of the
/// `ByteStream` API.
#[derive(Debug)]
pub struct ContentAddressableStorageService<S> {
    storage: S,
}

impl<S> ContentAddressableStorageService<S>
where
    S: Store + 'static,
{
    /// Create new instance of [`ContentAddressableStorageService`].
    #[must_use]
    pub fn new(storage: S) -> Self {
        Self { storage }
    }
}

#[async_trait::async_trait]
impl<S> ContentAddressableStorage for ContentAddressableStorageService<S>
where
    S: Store + 'static,
{
    /// Determine whether a blob is in the CAS.
    ///
    /// Clients can use this API before uploading blobs to determine which ones
    /// are already present in the CAS and do not need to be uploaded again.
    async fn find_missing_blobs(
        &self,
        req: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        let req = req.into_inner();
        let mut missing = vec![];

        for digest in &req.blob_digests {
            let hash = &digest.hash;

            let exists = match self.storage.contains(&hash) {
                Ok(exists) => exists,
                Err(err) => return Err(Status::internal(err.to_string())),
            };

            // Empty file digest
            if hash == "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855" {
                continue;
            }

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
