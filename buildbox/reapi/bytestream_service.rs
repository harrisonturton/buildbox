//! The ByteStream service, which REAPI clients use to upload and download
//! blobs too large for the batch CAS calls.

use crate::status;
use cas::{BlobReader, BlobWriter, Cas, CasError, Digest};
use proto::google::bytestream::{
    ByteStream, QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use std::str::FromStr;
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};

/// The size of each chunk sent when reading a blob.
const READ_CHUNK_SIZE: usize = 64 * 1024;

/// Reads and writes CAS blobs as byte streams.
pub struct ByteStreamService {
    cas: Arc<dyn Cas>,
}

impl ByteStreamService {
    pub fn new(cas: Arc<dyn Cas>) -> ByteStreamService {
        ByteStreamService { cas }
    }
}

#[tonic::async_trait]
impl ByteStream for ByteStreamService {
    type ReadStream = ReceiverStream<Result<ReadResponse, Status>>;

    async fn read(&self, request: Request<ReadRequest>) -> Result<Response<Self::ReadStream>, Status> {
        let request = request.into_inner();
        let digest = parse_read_resource(&request.resource_name)?;
        if request.read_offset != 0 || request.read_limit != 0 {
            return Err(Status::unimplemented("partial reads are not supported"));
        }

        let reader = self.cas.read(&digest).await.map_err(status::from_cas_error)?;
        let (sender, receiver) = mpsc::channel(4);
        tokio::spawn(send_chunks(reader, digest, sender));
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    async fn write(&self, request: Request<Streaming<WriteRequest>>) -> Result<Response<WriteResponse>, Status> {
        let mut stream = request.into_inner();
        let Some(first) = stream.next().await else {
            return Err(Status::invalid_argument("write stream is empty"));
        };
        let first = first?;

        let digest = parse_write_resource(&first.resource_name)?;
        let mut upload = Upload::start(&*self.cas, &digest).await?;
        let mut finished = upload.append(first).await?;

        while !finished {
            let Some(request) = stream.next().await else {
                return Err(Status::invalid_argument("write stream ended before finish_write"));
            };
            finished = upload.append(request?).await?;
        }

        let committed_size = upload.finish().await?;
        Ok(Response::new(WriteResponse { committed_size }))
    }

    async fn query_write_status(
        &self,
        _request: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        Err(Status::unimplemented("resumable writes are not supported"))
    }
}

/// Stream a blob to a client in chunks.
async fn send_chunks(mut reader: BlobReader, digest: Digest, sender: mpsc::Sender<Result<ReadResponse, Status>>) {
    let mut buf = vec![0; READ_CHUNK_SIZE];
    loop {
        let message = match reader.read(&mut buf).await {
            Ok(0) => return,
            Ok(read) => Ok(ReadResponse {
                data: buf[..read].to_vec(),
            }),
            Err(error) => Err(Status::internal(format!("failed to read blob {digest}: {error}"))),
        };

        let failed = message.is_err();
        // A send error means the client went away, so there's nobody to tell.
        if sender.send(message).await.is_err() || failed {
            return;
        }
    }
}

/// A write in progress. The data is written to the CAS as it arrives.
struct Upload {
    digest: Digest,
    writer: Box<dyn BlobWriter>,
    received: u64,
}

impl Upload {
    async fn start(cas: &dyn Cas, digest: &Digest) -> Result<Upload, Status> {
        let writer = cas.write(digest).await.map_err(status::from_cas_error)?;
        Ok(Upload {
            digest: digest.clone(),
            writer,
            received: 0,
        })
    }

    /// Check one request of the stream and write its data. Returns whether it
    /// was the last.
    async fn append(&mut self, request: WriteRequest) -> Result<bool, Status> {
        if request.write_offset != self.received as i64 {
            let message = format!("write_offset is {}, expected {}", request.write_offset, self.received);
            return Err(Status::invalid_argument(message));
        }

        self.received += request.data.len() as u64;
        if self.received > self.digest.size_bytes() {
            let message = format!("received more than the {} bytes of blob {}", self.digest.size_bytes(), self.digest);
            return Err(Status::invalid_argument(message));
        }

        self.writer.write(&request.data).await.map_err(status::from_cas_error)?;
        Ok(request.finish_write)
    }

    /// Verify and commit the blob, returning its size.
    async fn finish(self) -> Result<i64, Status> {
        match self.writer.commit().await {
            Ok(()) => Ok(self.received as i64),
            Err(error @ CasError::DigestMismatch { .. }) => Err(Status::invalid_argument(error.to_string())),
            Err(error) => Err(status::from_cas_error(error)),
        }
    }
}

/// Parse a read resource name, `{instance_name}/blobs/{hash}/{size}`, where
/// the instance name may be empty or contain slashes.
fn parse_read_resource(resource_name: &str) -> Result<Digest, Status> {
    let parts: Vec<&str> = resource_name.split('/').collect();
    let Some(blobs) = parts.iter().position(|part| *part == "blobs") else {
        return Err(invalid_resource(resource_name));
    };
    parse_digest_parts(&parts[blobs + 1..], resource_name)
}

/// Parse a write resource name,
/// `{instance_name}/uploads/{uuid}/blobs/{hash}/{size}{/optional_metadata}`,
/// where the instance name may be empty or contain slashes.
fn parse_write_resource(resource_name: &str) -> Result<Digest, Status> {
    let parts: Vec<&str> = resource_name.split('/').collect();
    let Some(uploads) = parts.iter().position(|part| *part == "uploads") else {
        return Err(invalid_resource(resource_name));
    };
    if parts.get(uploads + 2) != Some(&"blobs") {
        return Err(invalid_resource(resource_name));
    }
    parse_digest_parts(&parts[uploads + 3..], resource_name)
}

/// Parse the `{hash}/{size}` at the start of `parts`.
fn parse_digest_parts(parts: &[&str], resource_name: &str) -> Result<Digest, Status> {
    let [hash, size, ..] = parts else {
        return Err(invalid_resource(resource_name));
    };
    let Ok(size) = u64::from_str(size) else {
        return Err(invalid_resource(resource_name));
    };
    Digest::new(hash, size).map_err(|error| Status::invalid_argument(format!("{resource_name:?}: {error}")))
}

fn invalid_resource(resource_name: &str) -> Status {
    Status::invalid_argument(format!("invalid resource name {resource_name:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn parses_read_resources_with_and_without_instance_names() {
        let expected = Digest::new(HASH, 5).unwrap();
        for name in [
            format!("blobs/{HASH}/5"),
            format!("my/instance/blobs/{HASH}/5"),
        ] {
            assert_eq!(parse_read_resource(&name).unwrap(), expected, "{name}");
        }
    }

    #[test]
    fn parses_write_resources_with_and_without_instance_names() {
        let expected = Digest::new(HASH, 5).unwrap();
        for name in [
            format!("uploads/some-uuid/blobs/{HASH}/5"),
            format!("instance/uploads/some-uuid/blobs/{HASH}/5"),
            format!("uploads/some-uuid/blobs/{HASH}/5/extra/metadata"),
        ] {
            assert_eq!(parse_write_resource(&name).unwrap(), expected, "{name}");
        }
    }

    #[test]
    fn rejects_malformed_read_resources() {
        for name in [
            String::new(),
            format!("blobs/{HASH}"),
            format!("blobs/{HASH}/not-a-size"),
            "blobs/../../etc/passwd/5".to_string(),
            format!("compressed-blobs/zstd/{HASH}/5"),
        ] {
            assert!(parse_read_resource(&name).is_err(), "{name}");
        }
    }

    #[test]
    fn rejects_malformed_write_resources() {
        for name in [
            String::new(),
            format!("blobs/{HASH}/5"),
            format!("uploads/some-uuid/blobs/{HASH}"),
            format!("uploads/some-uuid/blobs/../5"),
            format!("uploads/some-uuid/compressed-blobs/zstd/{HASH}/5"),
        ] {
            assert!(parse_write_resource(&name).is_err(), "{name}");
        }
    }
}
