//! The CAS of a remote execution service.

use crate::Endpoint;
use async_trait::async_trait;
use bytestream_rust_connect::google::bytestream::{ByteStreamClient, ReadRequest, WriteRequest};
use cas::{BlobReader, BlobWriter, Cas, CasError, Digest, Usage};
use grpcclient::Channel;
use remote_execution_rust_connect::build::bazel::remote::execution::v2::{
    BatchUpdateBlobsRequest, ContentAddressableStorageClient, FindMissingBlobsRequest, batch_update_blobs_request,
};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

/// Blobs up to this size are uploaded in one batch request; larger ones are
/// streamed.
const MAX_BATCH_BYTES: usize = 1 << 20;

/// How much of a blob each streamed message carries.
const CHUNK_SIZE: usize = 1 << 20;

/// How many digests one `FindMissingBlobs` request asks about.
const FIND_MISSING_BATCH: usize = 10_000;

/// How many requests to the CAS are in flight at once, so that a build
/// with many actions doesn't overwhelm the connection.
const MAX_REQUESTS: usize = 64;

/// How many times a request the service was unavailable for is tried
/// again, as Bazel's `--remote_retries` does.
const RETRIES: u32 = 5;

/// A remote service's CAS.
#[derive(Clone)]
pub struct RemoteCas {
    endpoint: Endpoint,
    cas: ContentAddressableStorageClient<Channel>,
    bytestream: ByteStreamClient<Channel>,
    requests: Arc<Semaphore>,
}

impl RemoteCas {
    pub fn new(endpoint: Endpoint) -> RemoteCas {
        let (http, config) = endpoint.client();
        RemoteCas {
            cas: ContentAddressableStorageClient::new(http.clone(), config.clone()),
            bytestream: ByteStreamClient::new(http, config),
            endpoint,
            requests: Arc::new(Semaphore::new(MAX_REQUESTS)),
        }
    }

    fn error(&self, what: &str, error: impl std::fmt::Display) -> CasError {
        CasError::io(format!("{what} at {}", self.endpoint.uri), std::io::Error::other(error.to_string()))
    }

    /// Make a request, waiting for a free slot, and trying again with
    /// backoff while the service is unavailable.
    async fn call<T, F, Fut>(&self, mut request: F) -> Result<T, connectrpc::ConnectError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, connectrpc::ConnectError>>,
    {
        let mut delay = Duration::from_millis(100);
        let mut attempt = 0;
        loop {
            let permit = self.requests.acquire().await.map_err(|_| connectrpc::ConnectError::internal("the CAS client is closed"))?;
            let result = request().await;
            drop(permit);
            match result {
                Err(error) if is_retryable(&error) && attempt < RETRIES => {
                    attempt += 1;
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                result => return result,
            }
        }
    }

    /// Read a blob by streaming it, calling `chunk` with each piece.
    async fn read_chunks(&self, digest: &Digest, mut chunk: impl AsyncChunk) -> Result<(), CasError> {
        let request = ReadRequest {
            resource_name: self.endpoint.resource(&format!("blobs/{}/{}", digest.hash(), digest.size_bytes())),
            ..Default::default()
        };
        let mut stream = self.call(|| self.bytestream.read(request.clone())).await.map_err(|error| read_error(self, digest, error))?;
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
        self.call(|| self.bytestream.write(futures::stream::iter(requests.clone())))
            .await
            .map_err(|error| self.error(&format!("uploading {digest}"), error))?;
        Ok(())
    }

    /// Upload a large file by streaming it from disk, a chunk at a time.
    async fn stream_file(&self, digest: &Digest, path: &Path) -> Result<(), CasError> {
        let resource_name = self.endpoint.resource(&format!("uploads/{}/blobs/{}/{}", upload_id(), digest.hash(), digest.size_bytes()));
        let size = digest.size_bytes() as u64;
        let upload = || async {
            let file = tokio::fs::File::open(path).await.map_err(|error| connectrpc::ConnectError::internal(format!("failed to open {}: {error}", path.display())))?;
            self.bytestream.write(file_chunks(file, resource_name.clone(), size)).await
        };
        self.call(upload).await.map_err(|error| self.error(&format!("uploading {digest}"), error))?;
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
        let response = self
            .call(|| self.cas.batch_update_blobs(request.clone()))
            .await
            .map_err(|error| self.error(&format!("uploading {digest}"), error))?;
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
            let response = self
                .call(|| self.cas.find_missing_blobs(request.clone()))
                .await
                .map_err(|error| self.error("finding missing blobs", error))?;
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
        let digest = hash_file(path).await?;
        if digest.size_bytes() as usize <= MAX_BATCH_BYTES {
            let data = tokio::fs::read(path).await.map_err(|error| CasError::io(format!("failed to read {}", path.display()), error))?;
            self.batch_upload(&digest, data).await?;
            return Ok(digest);
        }
        self.stream_file(&digest, path).await?;
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

/// Whether a request failed in a way that trying again may fix.
fn is_retryable(error: &connectrpc::ConnectError) -> bool {
    matches!(error.code, connectrpc::ErrorCode::Unavailable | connectrpc::ErrorCode::ResourceExhausted)
}

/// A file's digest, read on a blocking thread.
async fn hash_file(path: &Path) -> Result<Digest, CasError> {
    let path = path.to_path_buf();
    let hashed = tokio::task::spawn_blocking(move || -> Result<Digest, CasError> {
        let mut file = std::fs::File::open(&path).map_err(|error| CasError::io(format!("failed to open {}", path.display()), error))?;
        let mut hasher = cas::Hasher::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = std::io::Read::read(&mut file, &mut buffer).map_err(|error| CasError::io(format!("failed to read {}", path.display()), error))?;
            if read == 0 {
                return Ok(hasher.finish());
            }
            hasher.update(&buffer[..read]);
        }
    })
    .await;
    hashed.map_err(|error| CasError::io("hashing task failed".to_string(), std::io::Error::other(error.to_string())))?
}

/// The `WriteRequest`s that stream a file, a chunk at a time.
fn file_chunks(file: tokio::fs::File, resource_name: String, size: u64) -> impl futures::Stream<Item = WriteRequest> + Send + 'static {
    futures::stream::unfold((file, 0u64, false), move |(mut file, offset, done)| {
        let resource_name = resource_name.clone();
        async move {
            if done {
                return None;
            }
            let mut data = vec![0u8; CHUNK_SIZE];
            let read = file.read(&mut data).await.ok()?;
            data.truncate(read);
            let next = offset + read as u64;
            let finish_write = next >= size || read == 0;
            let request = WriteRequest {
                resource_name: if offset == 0 { resource_name } else { String::new() },
                write_offset: offset as i64,
                finish_write,
                data: data.into(),
                ..Default::default()
            };
            Some((request, (file, next, finish_write)))
        }
    })
}

/// A unique ID for an upload, as ByteStream resource names need.
fn upload_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("bb-{}-{count}", std::process::id())
}
