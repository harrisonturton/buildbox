//! The ContentAddressableStorage service: finding, uploading and downloading
//! blobs in bulk.

use crate::{status, ResponseStream, MAX_BATCH_TOTAL_SIZE_BYTES};
use cas::{Cas, CasError, Digest};
use prost::Message;
use remote_execution_proto::build::bazel::remote::execution::v2::batch_read_blobs_response;
use remote_execution_proto::build::bazel::remote::execution::v2::batch_update_blobs_request;
use remote_execution_proto::build::bazel::remote::execution::v2::batch_update_blobs_response;
use remote_execution_proto::build::bazel::remote::execution::v2::content_addressable_storage_server::ContentAddressableStorage;
use remote_execution_proto::build::bazel::remote::execution::v2::{
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest, BatchUpdateBlobsResponse, Directory,
    FindMissingBlobsRequest, FindMissingBlobsResponse, GetTreeRequest, GetTreeResponse,
};
use status_proto::google::rpc;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use tonic::{Request, Response, Status};

/// The most directories sent in one GetTree response, to bound message size.
const GET_TREE_PAGE_SIZE: usize = 1000;

pub struct ContentAddressableStorageService {
    cas: Arc<dyn Cas>,
}

impl ContentAddressableStorageService {
    pub fn new(cas: Arc<dyn Cas>) -> ContentAddressableStorageService {
        ContentAddressableStorageService { cas }
    }

    async fn update_blob(&self, request: &batch_update_blobs_request::Request) -> rpc::Status {
        let digest = match Digest::from_required_proto(request.digest.as_ref()) {
            Ok(digest) => digest,
            Err(error) => return rpc_status(status::invalid_digest("digest", error)),
        };

        let result = self.write_blob(&digest, &request.data).await;
        match result {
            Ok(()) => rpc::Status::default(),
            Err(error @ CasError::DigestMismatch { .. }) => rpc_status(Status::invalid_argument(error.to_string())),
            Err(error) => rpc_status(status::from_cas_error(error)),
        }
    }

    async fn write_blob(&self, digest: &Digest, data: &[u8]) -> Result<(), CasError> {
        let mut writer = self.cas.write(digest).await?;
        writer.write(data).await?;
        writer.commit().await
    }

    async fn read_blob(&self, digest: Digest) -> batch_read_blobs_response::Response {
        let (data, status) = match self.cas.read_bytes(&digest).await {
            Ok(data) => (data, rpc::Status::default()),
            Err(error) => (vec![], rpc_status(status::from_cas_error(error))),
        };
        batch_read_blobs_response::Response {
            digest: Some(digest.to_proto()),
            data,
            status: Some(status),
        }
    }
}

#[tonic::async_trait]
impl ContentAddressableStorage for ContentAddressableStorageService {
    async fn find_missing_blobs(
        &self,
        request: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        let request = request.into_inner();
        let digests = parse_digests(&request.blob_digests, "blob_digests")?;

        let missing = self.cas.find_missing(&digests).await.map_err(status::from_cas_error)?;
        let missing_blob_digests = missing.iter().map(Digest::to_proto).collect();
        Ok(Response::new(FindMissingBlobsResponse { missing_blob_digests }))
    }

    async fn batch_update_blobs(
        &self,
        request: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        let request = request.into_inner();

        let mut responses = Vec::with_capacity(request.requests.len());
        for blob in &request.requests {
            let status = self.update_blob(blob).await;
            responses.push(batch_update_blobs_response::Response {
                digest: blob.digest.clone(),
                status: Some(status),
            });
        }
        Ok(Response::new(BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(
        &self,
        request: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        let request = request.into_inner();
        let digests = parse_digests(&request.digests, "digests")?;

        let total_size: u64 = digests.iter().map(Digest::size_bytes).sum();
        if total_size > MAX_BATCH_TOTAL_SIZE_BYTES {
            let message = format!("requested {total_size} bytes, more than the limit of {MAX_BATCH_TOTAL_SIZE_BYTES}");
            return Err(Status::invalid_argument(message));
        }

        let mut responses = Vec::with_capacity(digests.len());
        for digest in digests {
            responses.push(self.read_blob(digest).await);
        }
        Ok(Response::new(BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream = ResponseStream<Result<GetTreeResponse, Status>>;

    /// Return every `Directory` in a tree, in pages. Parts of the tree that
    /// aren't in the CAS are left out, as the protocol specifies.
    async fn get_tree(&self, request: Request<GetTreeRequest>) -> Result<Response<Self::GetTreeStream>, Status> {
        let request = request.into_inner();
        let root = Digest::from_required_proto(request.root_digest.as_ref())
            .map_err(|error| status::invalid_digest("root_digest", error))?;

        let directories = collect_tree(&*self.cas, root).await?;
        let pages: Vec<Result<GetTreeResponse, Status>> = directories
            .chunks(GET_TREE_PAGE_SIZE)
            .map(|page| {
                Ok(GetTreeResponse {
                    directories: page.to_vec(),
                    next_page_token: String::new(),
                })
            })
            .collect();
        Ok(Response::new(Box::pin(tokio_stream::iter(pages))))
    }
}

/// Read every `Directory` in the tree rooted at `root`, breadth first.
async fn collect_tree(cas: &dyn Cas, root: Digest) -> Result<Vec<Directory>, Status> {
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
            let child = Digest::from_required_proto(child.digest.as_ref())
                .map_err(|error| status::invalid_digest("directory digest", error))?;
            pending.push_back(child);
        }
        directories.push(directory);
    }
    Ok(directories)
}

/// Read a `Directory`, or `None` if it isn't in the CAS.
async fn read_directory(cas: &dyn Cas, digest: &Digest) -> Result<Option<Directory>, Status> {
    let data = match cas.read_bytes(digest).await {
        Ok(data) => data,
        Err(CasError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(status::from_cas_error(error)),
    };

    let directory = Directory::decode(data.as_slice())
        .map_err(|error| Status::invalid_argument(format!("blob {digest} isn't a Directory: {error}")))?;
    Ok(Some(directory))
}

fn parse_digests(
    protos: &[remote_execution_proto::build::bazel::remote::execution::v2::Digest],
    field: &str,
) -> Result<Vec<Digest>, Status> {
    let mut digests = Vec::with_capacity(protos.len());
    for proto in protos {
        let digest = Digest::from_proto(proto).map_err(|error| status::invalid_digest(field, error))?;
        digests.push(digest);
    }
    Ok(digests)
}

fn rpc_status(status: Status) -> rpc::Status {
    rpc::Status {
        code: status.code() as i32,
        message: status.message().to_string(),
        details: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cas_mem::MemCas;
    use remote_execution_proto::build::bazel::remote::execution::v2::DirectoryNode;
    use tokio_stream::StreamExt;
    use tonic::Code;

    fn service() -> (ContentAddressableStorageService, Arc<MemCas>) {
        let cas = Arc::new(MemCas::new());
        (ContentAddressableStorageService::new(cas.clone()), cas)
    }

    fn update_request(digest: &Digest, data: &[u8]) -> batch_update_blobs_request::Request {
        batch_update_blobs_request::Request {
            digest: Some(digest.to_proto()),
            data: data.to_vec(),
        }
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

        let response = service.batch_update_blobs(Request::new(request)).await.unwrap().into_inner();

        let codes: Vec<i32> = response.responses.iter().map(|r| r.status.as_ref().unwrap().code).collect();
        assert_eq!(codes, [Code::Ok as i32, Code::InvalidArgument as i32]);
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

        let response = service.batch_read_blobs(Request::new(request)).await.unwrap().into_inner();

        assert_eq!(response.responses[0].data, b"present");
        assert_eq!(response.responses[0].status.as_ref().unwrap().code, Code::Ok as i32);
        assert_eq!(response.responses[1].status.as_ref().unwrap().code, Code::NotFound as i32);
    }

    #[tokio::test]
    async fn batch_read_rejects_oversized_requests() {
        let (service, _cas) = service();
        let huge = Digest::new(Digest::empty().hash(), MAX_BATCH_TOTAL_SIZE_BYTES + 1).unwrap();
        let request = BatchReadBlobsRequest {
            digests: vec![huge.to_proto()],
            ..Default::default()
        };

        let err = service.batch_read_blobs(Request::new(request)).await.unwrap_err();

        assert_eq!(err.code(), Code::InvalidArgument);
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
                    digest: Some(leaf_digest.to_proto()),
                },
                DirectoryNode {
                    name: "missing".to_string(),
                    digest: Some(missing.to_proto()),
                },
            ],
            ..Default::default()
        };
        let root_digest = cas.put_bytes(&root.encode_to_vec()).await.unwrap();
        let request = GetTreeRequest {
            root_digest: Some(root_digest.to_proto()),
            ..Default::default()
        };

        let mut stream = service.get_tree(Request::new(request)).await.unwrap().into_inner();

        let page = stream.next().await.unwrap().unwrap();
        assert_eq!(page.directories, vec![root, leaf]);
        assert!(stream.next().await.is_none());
    }
}
