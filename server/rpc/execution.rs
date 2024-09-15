use super::ResponseStream;
use crate::{blob::LocalBlobStore, proto::remote_execution::{ExecuteRequest, Execution, Command, Action, WaitExecutionRequest}};
use operations_proto::google::longrunning::Operation;
use tonic::{Request, Response, Status};
use std::io::Read;
use bytes::BytesMut;
use prost::Message;

#[derive(Debug)]
pub struct ExecutionService {
    store: LocalBlobStore,
}

impl ExecutionService {
    /// Create new [`ExecutionService`] instance.
    #[must_use]
    pub fn new(store: LocalBlobStore) -> Self {
        Self { store }
    }
}

#[tonic::async_trait]
impl Execution for ExecutionService {
    type ExecuteStream = ResponseStream<Result<Operation, Status>>;

    async fn execute(
        &self,
        req: Request<ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let req = req.into_inner();
        tracing::info!("Execution::execute {req:?}");

        let action_digest = req.action_digest
            .ok_or_else(|| Status::invalid_argument("no action digest provided"))?;

        let mut data = vec![];
        let mut reader = self.store.read(&action_digest.hash).map_err(|err| Status::internal(err.to_string()))?;
        reader.read_to_end(&mut data).unwrap();
        let mut bytes = BytesMut::from(&data[..]);
        let action = Action::decode(&mut bytes).unwrap();
        tracing::info!("Received action: {action:?}");

        let command_digest = action.command_digest
            .ok_or_else(|| Status::invalid_argument("no command digest provided"))?;

        let mut data = vec![];
        let mut reader = self.store.read(&command_digest.hash).map_err(|err| Status::internal(err.to_string()))?;
        reader.read_to_end(&mut data).unwrap();
        let mut bytes = BytesMut::from(&data[..]);
        let command = Command::decode(&mut bytes).unwrap();
        tracing::info!("Received command: {command:?}");

        Err(Status::internal("not implemented"))
    }

    type WaitExecutionStream = ResponseStream<Result<Operation, Status>>;

    async fn wait_execution(
        &self,
        req: Request<WaitExecutionRequest>,
    ) -> Result<Response<Self::WaitExecutionStream>, Status> {
        let req = req.into_inner();
        tracing::info!("Execution::wait_execution {req:?}");
        Err(Status::internal("not implemented"))
    }
}
