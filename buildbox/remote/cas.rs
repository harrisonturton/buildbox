//! The CAS of a remote execution service.

use crate::Endpoint;
use async_trait::async_trait;
use bytestream_rust_connect::google::bytestream::{ByteStreamClient, ReadRequest, WriteRequest};
use cas::{BlobReader, BlobWriter, Cas, CasError, Digest, Usage};
use connectrpc::client::HttpClient;
use remote_execution_rust_connect::build::bazel::remote::execution::v2::{
    BatchUpdateBlobsRequest, ContentAddressableStorageClient, FindMissingBlobsRequest, batch_update_blobs_request,
};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Blobs up to this size are uploaded in one batch request; larger ones are
/// streamed.
const MAX_BATCH_BYTES: usize = 1 << 20;

/// How much of a blob each streamed message carries.
const CHUNK_SIZE: usize = 1 << 20;

/// How many digests one `FindMissingBlobs` request asks about.
const FIND_MISSING_BATCH: usize = 10_000;

/// A remote service's CAS.
#[derive(Clone)]
pub struct RemoteCas {
    endpoint: Endpoint,
    cas: ContentAddressableStorageClient<HttpClient>,
    bytestream: ByteStreamClient<HttpClient>,
}

impl RemoteCas {
    pub fn new(endpoint: Endpoint) -> RemoteCas {
        let (http, config) = endpoint.client();
        RemoteCas {
            cas: ContentAddressableStorageClient::new(http.clone(), config.clone()),
            bytestream: ByteStreamClient::new(http, config),
            endpoint,
        }
    }

    fn error(&self, what: &str, error: impl std::fmt::Display) -> CasError {
        CasError::io(format!("{what} at {}", self.endpoint.uri), std::io::Error::other(error.to_string()))
    }

    /// Read a blob by streaming it, calling `chunk` with each piece.
    async fn read_chunks(&self, digest: &Digest, mut chunk: impl AsyncChunk) -> Result<(), CasError> {
        let request = ReadRequest {
            resource_name: self.endpoint.resource(&format!("blobs/{}/{}", digest.hash(), digest.size_bytes())),
            ..Default::default()
        };
        let mut stream = self.bytestream.read(request).await.map_err(|error| read_error(self, digest, error))?;
        while let Some(message) = stream.message().await.map_err(|error| read_error(self, digest, error))? {
            chunk.write(&message.to_owned_message().data).await?;
        }
        Ok(())
    }

    async fn upload(&self, digest: &Digest, data: Vec<u8>) -> Result<(), CasError> {
        if data.len() <= MAX_BATCH_BYTES {
            return self.batch_upload(digest, data).await;
        }
        let resource_name = self.endpoint.resource(&format!("uploads/{}/blobs/{}/{}", upload_id(), digest.hash(), digest.size_bytes()));
        let mut requests = Vec::new();
        let mut offset = 0;
        for piece in data.chunks(CHUNK_SIZE) {
            requests.push(WriteRequest {
                resource_name: if offset == 0 { resource_name.clone() } else { String::new() },
                write_offset: offset as i64,
                finish_write: offset + piece.len() == data.len(),
                data: piece.to_vec().into(),
                ..Default::default()
            });
            offset += piece.len();
        }
        let stream = futures::stream::iter(requests);
        self.bytestream.write(stream).await.map_err(|error| self.error(&format!("uploading {digest}"), error))?;
        Ok(())
    }

    async fn batch_upload(&self, digest: &Digest, data: Vec<u8>) -> Result<(), CasError> {
        let request = BatchUpdateBlobsRequest {
            instance_name: self.endpoint.instance_name.clone(),
            requests: vec![batch_update_blobs_request::Request {
                digest: digest.to_proto().into(),
                data: data.into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let response = self.cas.batch_update_blobs(request).await.map_err(|error| self.error(&format!("uploading {digest}"), error))?;
        for response in response.into_owned().responses {
            let code = response.status.as_option().map(|status| status.code).unwrap_or(0);
            if code != 0 {
                let message = response.status.as_option().map(|status| status.message.clone()).unwrap_or_default();
                return Err(self.error(&format!("uploading {digest}"), format!("status {code}: {message}")));
            }
        }
        Ok(())
    }
}

fn read_error(cas: &RemoteCas, digest: &Digest, error: connectrpc::ConnectError) -> CasError {
    if error.code == connectrpc::ErrorCode::NotFound {
        return CasError::NotFound(digest.clone());
    }
    cas.error(&format!("reading {digest}"), error)
}

/// Where the chunks of a blob being read go.
trait AsyncChunk: Send {
    fn write(&mut self, data: &[u8]) -> impl std::future::Future<Output = Result<(), CasError>> + Send;
}

impl AsyncChunk for &mut Vec<u8> {
    async fn write(&mut self, data: &[u8]) -> Result<(), CasError> {
        self.extend_from_slice(data);
        Ok(())
    }
}

/// Writes the chunks of a blob to a file.
struct FileChunks<'a> {
    file: tokio::fs::File,
    path: &'a Path,
}

impl AsyncChunk for FileChunks<'_> {
    async fn write(&mut self, data: &[u8]) -> Result<(), CasError> {
        self.file
            .write_all(data)
            .await
            .map_err(|error| CasError::io(format!("failed to write {}", self.path.display()), error))
    }
}

#[async_trait]
impl Cas for RemoteCas {
    async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, CasError> {
        let mut missing = Vec::new();
        for batch in digests.chunks(FIND_MISSING_BATCH) {
            let request = FindMissingBlobsRequest {
                instance_name: self.endpoint.instance_name.clone(),
                blob_digests: batch.iter().map(Digest::to_proto).collect(),
                ..Default::default()
            };
            let response = self.cas.find_missing_blobs(request).await.map_err(|error| self.error("finding missing blobs", error))?;
            for proto in &response.into_owned().missing_blob_digests {
                missing.push(Digest::from_proto(proto).map_err(|error| self.error("finding missing blobs", error))?);
            }
        }
        // The empty blob is always present.
        missing.retain(|digest| digest.size_bytes() > 0);
        Ok(missing)
    }

    async fn read(&self, digest: &Digest) -> Result<BlobReader, CasError> {
        let mut data = Vec::with_capacity(digest.size_bytes().min(1 << 20) as usize);
        if digest.size_bytes() > 0 {
            self.read_chunks(digest, &mut data).await?;
        }
        Ok(Box::pin(std::io::Cursor::new(data)))
    }

    async fn write(&self, expected: &Digest) -> Result<Box<dyn BlobWriter>, CasError> {
        Ok(Box::new(RemoteWriter {
            cas: self.clone(),
            expected: expected.clone(),
            data: Vec::new(),
        }))
    }

    async fn put_file(&self, path: &Path) -> Result<Digest, CasError> {
        let mut file = tokio::fs::File::open(path).await.map_err(|error| CasError::io(format!("failed to open {}", path.display()), error))?;
        let mut data = Vec::new();
        file.read_to_end(&mut data).await.map_err(|error| CasError::io(format!("failed to read {}", path.display()), error))?;
        let digest = Digest::of_bytes(&data);
        self.upload(&digest, data).await?;
        Ok(digest)
    }

    async fn materialize(&self, digest: &Digest, dest: &Path, executable: bool) -> Result<(), CasError> {
        let file = tokio::fs::File::create(dest).await.map_err(|error| CasError::io(format!("failed to create {}", dest.display()), error))?;
        let mut chunks = FileChunks { file, path: dest };
        if digest.size_bytes() > 0 {
            self.read_chunks(digest, &mut chunks).await?;
        }
        chunks.file.flush().await.map_err(|error| CasError::io(format!("failed to write {}", dest.display()), error))?;
        let mode = if executable { 0o555 } else { 0o444 };
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode))
            .await
            .map_err(|error| CasError::io(format!("failed to set permissions of {}", dest.display()), error))
    }

    async fn usage(&self) -> Result<Usage, CasError> {
        Ok(Usage::default())
    }
}

impl<'a> AsyncChunk for &mut FileChunks<'a> {
    async fn write(&mut self, data: &[u8]) -> Result<(), CasError> {
        (**self).write(data).await
    }
}

/// Collects a blob's data, and uploads it on commit.
struct RemoteWriter {
    cas: RemoteCas,
    expected: Digest,
    data: Vec<u8>,
}

#[async_trait]
impl BlobWriter for RemoteWriter {
    async fn write(&mut self, data: &[u8]) -> Result<(), CasError> {
        self.data.extend_from_slice(data);
        Ok(())
    }

    async fn commit(self: Box<Self>) -> Result<(), CasError> {
        let actual = Digest::of_bytes(&self.data);
        if actual != self.expected {
            return Err(CasError::DigestMismatch {
                expected: self.expected,
                actual,
            });
        }
        self.cas.upload(&self.expected, self.data).await
    }
}

/// A unique ID for an upload, as ByteStream resource names need.
fn upload_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("bb-{}-{count}", std::process::id())
}
