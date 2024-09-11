use bytestream_proto::google::bytestream::byte_stream_server::ByteStream;
use bytestream_proto::google::bytestream::{ReadRequest, ReadResponse, WriteRequest, WriteResponse, QueryWriteStatusRequest, QueryWriteStatusResponse};
use super::ResponseStream;
use tonic::{Streaming, Request, Response, Status};

#[derive(Default, Debug)]
pub struct ByteStreamService{}

#[tonic::async_trait]
impl ByteStream for ByteStreamService {
    type ReadStream = ResponseStream<Result<ReadResponse, Status>>;

    async fn read(
        &self,
        req: Request<ReadRequest>,
    ) -> Result<Response<Self::ReadStream>, Status> {
        let req = req.into_inner();
        tracing::info!("ByteStream::read {req:?}");
        Err(Status::internal("not implemented"))
    }

    async fn write(
        &self,
        req: Request<Streaming<WriteRequest>>,
    ) -> Result<Response<WriteResponse>, Status> {
        let req = req.into_inner();
        tracing::info!("ByteStream::write {req:?}");
        Err(Status::internal("not implemented"))
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