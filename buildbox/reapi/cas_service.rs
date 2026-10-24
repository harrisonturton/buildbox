//! The ContentAddressableStorage service: finding, uploading and downloading
//! blobs in bulk.

use crate::{status, MAX_BATCH_TOTAL_SIZE_BYTES};
use buffa::Message;
use cas::{Cas, CasError, Digest};
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream};
use remote_execution_rust_connect::build::bazel::remote::execution::v2::batch_read_blobs_response;
use remote_execution_rust_connect::build::bazel::remote::execution::v2::batch_update_blobs_request;
use remote_execution_rust_connect::build::bazel::remote::execution::v2::batch_update_blobs_response;
use remote_execution_rust_connect::build::bazel::remote::execution::v2::{
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest, BatchUpdateBlobsResponse,
    ContentAddressableStorage, Directory, FindMissingBlobsRequest, FindMissingBlobsResponse, GetTreeRequest,
    GetTreeResponse,
};
use remote_execution_rust_connect::google::rpc;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

/// The most directories sent in one GetTree response, to bound message size.
const GET_TREE_PAGE_SIZE: usize = 1000;

pub struct ContentAddressableStorageService {
    cas: Arc<dyn Cas>,
}

impl ContentAddressableStorageService {
    pub fn new(cas: Arc<dyn Cas>) -> ContentAddressableStorageService {
        ContentAddressableStorageService { cas }
    }

    async fn find_missing(&self, request: FindMissingBlobsRequest) -> Result<FindMissingBlobsResponse, ConnectError> {
        let digests = parse_digests(&request.blob_digests, "blob_digests")?;

        let missing = self.cas.find_missing(&digests).await.map_err(status::from_cas_error)?;
        Ok(FindMissingBlobsResponse {
            missing_blob_digests: missing.iter().map(Digest::to_proto).collect(),
            ..Default::default()
        })
    }

    async fn update_blobs(&self, request: BatchUpdateBlobsRequest) -> BatchUpdateBlobsResponse {
        let mut responses = Vec::with_capacity(request.requests.len());
        for blob in &request.requests {
            let status = self.update_blob(blob).await;
            responses.push(batch_update_blobs_response::Response {
                digest: blob.digest.clone(),
                status: status.into(),
                ..Default::default()
            });
        }
        BatchUpdateBlobsResponse {
            responses,
            ..Default::default()
        }
    }

    async fn update_blob(&self, request: &batch_update_blobs_request::Request) -> rpc::Status {
        let digest = match Digest::from_required_proto(request.digest.as_option()) {
            Ok(digest) => digest,
            Err(error) => return status::to_rpc_status(status::invalid_digest("digest", error)),
        };

        match self.write_blob(&digest, &request.data).await {
            Ok(()) => rpc::Status::default(),
            Err(error) => status::to_rpc_status(status::from_cas_error(error)),
        }
    }

    async fn write_blob(&self, digest: &Digest, data: &[u8]) -> Result<(), CasError> {
        let mut writer = self.cas.write(digest).await?;
        writer.write(data).await?;
        writer.commit().await
    }

    async fn read_blobs(&self, request: BatchReadBlobsRequest) -> Result<BatchReadBlobsResponse, ConnectError> {
        let digests = parse_digests(&request.digests, "digests")?;

        let total_size: u64 = digests.iter().map(Digest::size_bytes).sum();
        if total_size > MAX_BATCH_TOTAL_SIZE_BYTES {
            let message = format!("requested {total_size} bytes, more than the limit of {MAX_BATCH_TOTAL_SIZE_BYTES}");
            return Err(ConnectError::invalid_argument(message));
        }

        let mut responses = Vec::with_capacity(digests.len());
        for digest in digests {
            responses.push(self.read_blob(digest).await);
        }
        Ok(BatchReadBlobsResponse {
            responses,
            ..Default::default()
        })
    }

    async fn read_blob(&self, digest: Digest) -> batch_read_blobs_response::Response {
        let (data, status) = match self.cas.read_bytes(&digest).await {
            Ok(data) => (data, rpc::Status::default()),
            Err(error) => (vec![], status::to_rpc_status(status::from_cas_error(error))),
        };
        batch_read_blobs_response::Response {
            digest: digest.to_proto().into(),
            data,
            status: status.into(),
            ..Default::default()
        }
    }

    /// Every `Directory` in a tree, in pages. Parts of the tree that aren't
    /// in the CAS are left out, as the protocol specifies.
    async fn tree_pages(&self, request: GetTreeRequest) -> Result<Vec<GetTreeResponse>, ConnectError> {
        let root = Digest::from_required_proto(request.root_digest.as_option())
            .map_err(|error| status::invalid_digest("root_digest", error))?;

        let directories = collect_tree(&*self.cas, root).await?;
        let pages = directories
            .chunks(GET_TREE_PAGE_SIZE)
            .map(|page| GetTreeResponse {
                directories: page.to_vec(),
                ..Default::default()
            })
            .collect();
        Ok(pages)
    }
}

// Generated traits return `impl Encodable<M>`; handlers narrow that to `M`.
#[allow(refining_impl_trait)]
impl ContentAddressableStorage for ContentAddressableStorageService {
    async fn find_missing_blobs(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, FindMissingBlobsRequest>,
    ) -> ServiceResult<FindMissingBlobsResponse> {
        Response::ok(self.find_missing(request.to_owned_message()).await?)
    }

    async fn batch_update_blobs(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, BatchUpdateBlobsRequest>,
    ) -> ServiceResult<BatchUpdateBlobsResponse> {
        Response::ok(self.update_blobs(request.to_owned_message()).await)
    }

    async fn batch_read_blobs(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, BatchReadBlobsRequest>,
    ) -> ServiceResult<BatchReadBlobsResponse> {
        Response::ok(self.read_blobs(request.to_owned_message()).await?)
    }

    async fn get_tree(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetTreeRequest>,
    ) -> ServiceResult<ServiceStream<GetTreeResponse>> {
        let pages = self.tree_pages(request.to_owned_message()).await?;
        let stream: ServiceStream<GetTreeResponse> = Box::pin(tokio_stream::iter(pages.into_iter().map(Ok)));
        Response::ok(stream)
    }
}

/// Read every `Directory` in the tree rooted at `root`, breadth first.
async fn collect_tree(cas: &dyn Cas, root: Digest) -> Result<Vec<Directory>, ConnectError> {
    let mut directories = vec![];
    let mut seen = HashSet::new();
    let mut pending = VecDeque::from([root]);

    while let Some(digest) = pending.pop_front() {
        if !seen.insert(digest.clone()) {
            continue;
        }
        let Some(directory) = read_directory(cas, &digest).await? else {
            continue;
        };

        for child in &directory.directories {
            let child = Digest::from_required_proto(child.digest.as_option())
                .map_err(|error| status::invalid_digest("directory digest", error))?;
            pending.push_back(child);
        }
        directories.push(directory);
    }
    Ok(directories)
}

/// Read a `Directory`, or `None` if it isn't in the CAS.
async fn read_directory(cas: &dyn Cas, digest: &Digest) -> Result<Option<Directory>, ConnectError> {
    let data = match cas.read_bytes(digest).await {
        Ok(data) => data,
        Err(CasError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(status::from_cas_error(error)),
    };

    let directory = Directory::decode_from_slice(&data)
        .map_err(|error| ConnectError::invalid_argument(format!("blob {digest} isn't a Directory: {error}")))?;
    Ok(Some(directory))
}

fn parse_digests(
    protos: &[remote_execution_rust_connect::build::bazel::remote::execution::v2::Digest],
    field: &str,
) -> Result<Vec<Digest>, ConnectError> {
    let mut digests = Vec::with_capacity(protos.len());
    for proto in protos {
        let digest = Digest::from_proto(proto).map_err(|error| status::invalid_digest(field, error))?;
        digests.push(digest);
    }
    Ok(digests)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cas_mem::MemCas;
    use connectrpc::ErrorCode;
    use remote_execution_rust_connect::build::bazel::remote::execution::v2::DirectoryNode;

    fn service() -> (ContentAddressableStorageService, Arc<MemCas>) {
        let cas = Arc::new(MemCas::new());
        (ContentAddressableStorageService::new(cas.clone()), cas)
    }

    fn update_request(digest: &Digest, data: &[u8]) -> batch_update_blobs_request::Request {
        batch_update_blobs_request::Request {
            digest: digest.to_proto().into(),
            data: data.to_vec(),
            ..Default::default()
        }
    }

    fn code(error: ErrorCode) -> i32 {
        error.grpc_code() as i32
    }

    #[tokio::test]
    async fn batch_update_stores_blobs_and_reports_each_status() {
        let (service, cas) = service();
        let good = Digest::of_bytes(b"good");
        let wrong = Digest::of_bytes(b"claimed");
        let request = BatchUpdateBlobsRequest {
            requests: vec![update_request(&good, b"good"), update_request(&wrong, b"actual")],
            ..Default::default()
        };

        let response = service.update_blobs(request).await;

        let codes: Vec<i32> = response.responses.iter().map(|r| r.status.code).collect();
        assert_eq!(codes, [0, code(ErrorCode::InvalidArgument)]);
        assert_eq!(cas.read_bytes(&good).await.unwrap(), b"good");
        assert_eq!(cas.find_missing(&[wrong.clone()]).await.unwrap(), vec![wrong]);
    }

    #[tokio::test]
    async fn batch_read_returns_data_and_not_found() {
        let (service, cas) = service();
        let present = cas.put_bytes(b"present").await.unwrap();
        let absent = Digest::of_bytes(b"absent");
        let request = BatchReadBlobsRequest {
            digests: vec![present.to_proto(), absent.to_proto()],
            ..Default::default()
        };

        let response = service.read_blobs(request).await.unwrap();

        assert_eq!(response.responses[0].data, b"present");
        assert_eq!(response.responses[0].status.code, 0);
        assert_eq!(response.responses[1].status.code, code(ErrorCode::NotFound));
    }

    #[tokio::test]
    async fn batch_read_rejects_oversized_requests() {
        let (service, _cas) = service();
        let huge = Digest::new(Digest::empty().hash(), MAX_BATCH_TOTAL_SIZE_BYTES + 1).unwrap();
        let request = BatchReadBlobsRequest {
            digests: vec![huge.to_proto()],
            ..Default::default()
        };

        let err = service.read_blobs(request).await.unwrap_err();

        assert_eq!(err.code, ErrorCode::InvalidArgument);
    }

    #[tokio::test]
    async fn get_tree_returns_present_directories() {
        let (service, cas) = service();
        let leaf = Directory::default();
        let leaf_digest = cas.put_bytes(&leaf.encode_to_vec()).await.unwrap();
        let missing = Digest::of_bytes(b"missing directory");
        let root = Directory {
            directories: vec![
                DirectoryNode {
                    name: "leaf".to_string(),
                    digest: leaf_digest.to_proto().into(),
                    ..Default::default()
                },
                DirectoryNode {
                    name: "missing".to_string(),
                    digest: missing.to_proto().into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let root_digest = cas.put_bytes(&root.encode_to_vec()).await.unwrap();
        let request = GetTreeRequest {
            root_digest: root_digest.to_proto().into(),
            ..Default::default()
        };

        let pages = service.tree_pages(request).await.unwrap();

        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].directories, vec![root, leaf]);
    }
}
