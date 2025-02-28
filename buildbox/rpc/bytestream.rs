//! The [`crate::rpc::bytestream`] RPC service acts like a generic,
//! content-addressable blob storage server.
//!
//! While not technically a "named" part of the Bazel remote APIs, the other RPC
//! services assume that a `bytestream` service is available. The bytestream
//! service is used by the remote execution and caching services to store
//! inputs, action digests, output artifacts, etc.

use super::ResponseStream;
use common::Error;
use proto::google::bytestream::{
    ByteStream, QueryWriteStatusRequest, QueryWriteStatusResponse, ReadRequest, ReadResponse,
    WriteRequest, WriteResponse,
};
use tokio::sync::mpsc;
use std::{
    io::{Cursor, Read},
    str::FromStr,
};
use storage::Storage;
use tokio_stream::{wrappers::ReceiverStream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

/// Effectively the read/write input to the CAS for large byte payloads.
#[derive(Debug)]
pub struct ByteStreamService {
    storage: Storage,
}

impl ByteStreamService {
    /// Create a new [`ByteStreamService`] instance.
    #[must_use]
    pub fn new(storage: Storage) -> Self {
        Self { storage }
    }
}

#[async_trait::async_trait]
impl ByteStream for ByteStreamService {
    type ReadStream = ReceiverStream<Result<ReadResponse, Status>>;

    // ReadRequest { resource_name: "blobs/eaa7d9cde97fef21303b4e1e18e3e6cb7e68c92313b24bbc0335be9fe37af464/30", read_offset: 0, read_limit: 0 }

    async fn read(&self, req: Request<ReadRequest>) -> Result<Response<Self::ReadStream>, Status> {
        let req = req.into_inner();
        tracing::info!("ByteStream::read {req:?}");

        let parts = req.resource_name.split("/").collect::<Vec<&str>>();
        let hash = &parts[1];

        let mut reader = self
            .storage
            .read(hash)
            .map_err(|err| Status::internal(err.to_string()))?;

        let (tx, rx) = mpsc::channel(1024);

        tokio::spawn(async move {
            loop {
                let mut data = vec![0; 1024];
                let read = reader
                    .read(&mut data)
                    .map_err(|err| Status::internal("failed to read"))
                    .unwrap();

                if read == 0 {
                    break;
                }

                let res = ReadResponse {
                    data: data[0..read].to_vec(),
                };

                tx.send(Ok(res))
                    .await
                    .map_err(|err| Status::internal("failed to send execute response"))
                    .unwrap();
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
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

                if resource_name.uuid
                    == "hash=d0b06d284064528c34e0177821b739e116582907f4de5e0886cde3476483367a"
                {
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
        self.storage
            .write_with_name(&name.hash, reader)
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
