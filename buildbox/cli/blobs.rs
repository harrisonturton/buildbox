//! Uploading blobs to a server's CAS.

use bytestream_rust_connect::google::bytestream::{ByteStreamClient, WriteRequest};
use cas::Digest;
use connectrpc::client::{ClientConfig, HttpClient};
use remote_execution_rust_connect::build::bazel::remote::execution::v2::{
    ContentAddressableStorageClient, FindMissingBlobsRequest,
};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

/// How much of a blob to send in each message. Well under the server's
/// message size limit.
const CHUNK_SIZE: usize = 1024 * 1024;

/// Uploads, and checks for, blobs on a server.
pub struct Blobs {
    cas: ContentAddressableStorageClient<HttpClient>,
    bytestream: ByteStreamClient<HttpClient>,
    url: String,
}

impl Blobs {
    pub fn new(uri: http::Uri, url: String) -> Blobs {
        Blobs {
            cas: ContentAddressableStorageClient::new(HttpClient::plaintext(), ClientConfig::new(uri.clone())),
            bytestream: ByteStreamClient::new(HttpClient::plaintext(), ClientConfig::new(uri)),
            url,
        }
    }

    /// The blobs in `digests` the server doesn't have.
    pub async fn find_missing(&self, digests: &[Digest]) -> Result<Vec<Digest>, Box<dyn Error>> {
        let request = FindMissingBlobsRequest {
            blob_digests: digests.iter().map(Digest::to_proto).collect(),
            ..Default::default()
        };
        let response = self.cas.find_missing_blobs(request).await;
        let response = response.map_err(|error| format!("request to {} failed: {error}", self.url))?;

        let mut missing = vec![];
        for proto in &response.into_owned().missing_blob_digests {
            missing.push(Digest::from_proto(proto)?);
        }
        Ok(missing)
    }

    /// Upload the file at `path`, whose contents have digest `digest`.
    pub async fn upload_file(&self, path: &Path, digest: &Digest) -> Result<(), Box<dyn Error>> {
        let (requests, receiver) = mpsc::channel(4);
        let (read_result, read_error) = oneshot::channel();
        tokio::spawn(send_chunks(path.to_path_buf(), digest.clone(), requests, read_result));

        let response = self.bytestream.write(ReceiverStream::new(receiver)).await;
        // A failure reading the file ends the upload early, so report it
        // rather than the server's complaint about a short upload.
        if let Ok(Err(error)) = read_error.await {
            return Err(format!("failed to read {}: {error}", path.display()).into());
        }

        let response = response.map_err(|error| format!("failed to upload {}: {error}", path.display()))?;
        let committed = response.into_owned().committed_size;
        if u64::try_from(committed) != Ok(digest.size_bytes()) {
            let size = digest.size_bytes();
            return Err(format!("uploading {} stored {committed} of {size} bytes", path.display()).into());
        }
        Ok(())
    }
}

/// Read the file at `path` in chunks, sending each as a write request.
/// Reports whether the whole file was read through `result`.
async fn send_chunks(
    path: PathBuf,
    digest: Digest,
    requests: mpsc::Sender<WriteRequest>,
    result: oneshot::Sender<std::io::Result<()>>,
) {
    let read = read_chunks(&path, &digest, &requests).await;
    let _ = result.send(read);
}

async fn read_chunks(path: &Path, digest: &Digest, requests: &mpsc::Sender<WriteRequest>) -> std::io::Result<()> {
    let mut file = tokio::fs::File::open(path).await?;
    let size = digest.size_bytes();
    let resource_name = format!("uploads/{}/blobs/{digest}", upload_id());

    let mut offset = 0;
    loop {
        let mut data = vec![0; CHUNK_SIZE.min((size - offset) as usize)];
        file.read_exact(&mut data).await?;
        let request = WriteRequest {
            resource_name: if offset == 0 { resource_name.clone() } else { String::new() },
            write_offset: offset as i64,
            finish_write: offset + data.len() as u64 == size,
            data,
            ..Default::default()
        };
        offset += request.data.len() as u64;
        let finished = request.finish_write;

        if requests.send(request).await.is_err() {
            // The upload was abandoned; its error is reported instead.
            return Ok(());
        }
        if finished {
            return Ok(());
        }
    }
}

/// A unique ID for an upload, as ByteStream resource names need.
fn upload_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("bb-{}-{count}", std::process::id())
}
