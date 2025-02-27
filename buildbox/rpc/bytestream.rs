//! The [`crate::rpc::bytestream`] RPC service acts like a generic,
//! content-addressable blob storage server.
//! 
//! While not technically a "named" part of the Bazel remote APIs, the other RPC
//! services assume that a `bytestream` service is available. The bytestream
//! service is used by the remote execution and caching services to store
//! inputs, action digests, output artifacts, etc.

use super::ResponseStream;
use internal::{blob::LocalBlobStore, Error};
use bytestream_proto::google::bytestream::byte_stream_server::ByteStream;
use bytestream_proto::google::bytestream::{
    QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse, WriteRequest,
    WriteResponse,
};
use std::str::FromStr;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};

/// Effectively the read/write input to the CAS for large byte payloads.
#[derive(Debug)]
pub struct ByteStreamService {
    store: LocalBlobStore,
}

impl ByteStreamService {
    /// Create a new [`ByteStreamService`] instance.
    #[must_use]
    pub fn new(store: LocalBlobStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl ByteStream for ByteStreamService {
    type ReadStream = ResponseStream<Result<ReadResponse, Status>>;

    async fn read(&self, req: Request<ReadRequest>) -> Result<Response<Self::ReadStream>, Status> {
        let req = req.into_inner();
        tracing::info!("ByteStream::read {req:?}");
        Err(Status::internal("not implemented"))
    }


    async fn write(
        &self,
        req: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let mut stream = req.into_inner();

        let mut name = None;
        let mut data: Vec<u8> = vec![];
        let mut received = 0;

        while let Some(Ok(req)) = stream.next().await {
            if &req.resource_name != "" {
                let resource_name = ResourceName::parse(&req.resource_name)
                    .map_err(|err| Status::invalid_argument(err.to_string()))?;
                tracing::info!("ByteStream::write hash={:?}", resource_name.hash);
                name = Some(resource_name.clone());

                if resource_name.uuid == "hash=d0b06d284064528c34e0177821b739e116582907f4de5e0886cde3476483367a" {
                    tracing::warn!("GOT {req:?}");
                }
            }

            data.extend(req.data.iter());

            if req.finish_write {
                break;
            }
        }

        let name = name.ok_or_else(|| Status::invalid_argument("did not receive resource name"))?;

        if data.len() != name.size as usize {
            let err = format!("Expected {} bytes but received {}", name.size, data.len());
            tracing::error!("{err}");
            return Err(Status::invalid_argument(&err));
        }

        tracing::info!("Writing {}", name.hash);
        let mut reader = std::io::Cursor::new(&data);
        self.store
            .write(&name.hash, reader)
            .map_err(|err| {
                tracing::error!("Failed to write file: {err}");
                Status::internal(err.to_string())
            })?;

        Ok(Response::new(WriteResponse {
            committed_size: data.len() as i64,
        }))
    }

    async fn query_write_status(
        &self,
        req: Request<QueryWriteStatusRequest>,
    ) -> Result<Response<QueryWriteStatusResponse>, Status> {
        let req = req.into_inner();
        tracing::info!("ByteStream::query_write_status {req:?}");
        Err(Status::internal("not implemented"))
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ResourceName {
    pub uuid: String,
    pub hash: String,
    pub size: u64,
}

impl ResourceName {
    // `resource_name` is `{instance_name}/uploads/{uuid}/blobs/{hash}/{size}`,
    // where `{instance_name}` is optional, and there can be trailing URL
    // components after {size}.
    pub fn parse(resource_name: &str) -> Result<Self, Error> {
        let parts = resource_name.split("/").collect::<Vec<_>>();

        if parts.len() < 5 {
            return Err(Error::invalid(&format!(
                "must have more parts: {resource_name}"
            )));
        }

        if parts[0] != "uploads" {
            return Err(Error::invalid("resource name does not begin with uploads"));
        }

        let size =
            u64::from_str(parts[4]).map_err(|_| Error::invalid("invalid size in resource name"))?;

        Ok(ResourceName {
            uuid: parts[1].to_string(),
            hash: parts[3].to_string(),
            size,
        })
    }
}