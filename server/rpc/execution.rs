use super::ResponseStream;
use crate::{
    blob::LocalBlobStore,
    proto::remote_execution::{
        Action, ActionResult, Command, ExecuteRequest, ExecuteResponse, Execution,
        WaitExecutionRequest,
    },
};
use std::collections::HashMap;
use any_proto::google::protobuf::Any;
use bytes::BytesMut;
use status_proto::google::rpc;
use operations_proto::google::longrunning::{operation, Operation};
use prost::Message;
use std::io::Read;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

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
    type ExecuteStream = ReceiverStream<Result<Operation, Status>>;

    async fn execute(
        &self,
        req: Request<ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let req = req.into_inner();
        tracing::info!("Execution::execute {req:?}");

        let action_digest = req
            .action_digest
            .ok_or_else(|| Status::invalid_argument("no action digest provided"))?;

        let mut data = vec![];
        let mut reader = self
            .store
            .read(&action_digest.hash)
            .map_err(|err| Status::internal(err.to_string()))?;
        reader.read_to_end(&mut data).unwrap();
        let mut bytes = BytesMut::from(&data[..]);
        let action = Action::decode(&mut bytes).unwrap();
        tracing::info!("Received action: {action:?}");

        let command_digest = action
            .command_digest
            .ok_or_else(|| Status::invalid_argument("no command digest provided"))?;

        let mut data = vec![];
        let mut reader = self
            .store
            .read(&command_digest.hash)
            .map_err(|err| Status::internal(err.to_string()))?;
        reader.read_to_end(&mut data).unwrap();
        let mut bytes = BytesMut::from(&data[..]);
        let command = Command::decode(&mut bytes).unwrap();
        tracing::info!("Received command: {command:?}");

        let (tx, rx) = mpsc::channel(4);

        tokio::spawn(async move {
            let action_res = ActionResult {
                output_files: vec![],
                output_file_symlinks: vec![],
                output_symlinks: vec![],
                output_directories: vec![],
                output_directory_symlinks: vec![],
                exit_code: 0,
                stdout_raw: vec![],
                stdout_digest: None,
                stderr_raw: vec![],
                stderr_digest: None,
                execution_metadata: None,
            };

            let status = rpc::Status {
                code: 0,
                message: "error message".to_string(),
                details: vec![],
            };

            let res = ExecuteResponse {
                result: Some(action_res),
                cached_result: false,
                status: Some(status),
                server_logs: HashMap::new(),
                message: "Optional message".to_string(),
            };

            let any = prost_types::Any::from_msg(&res).unwrap();

            let proto_any = Any {
                type_url: any.type_url,
                value: any.value,
            };

            let op = Operation {
                name: "test".to_string(),
                metadata: None,
                done: true,
                result: Some(operation::Result::Response(proto_any)),
            };

            tx.send(Ok(op)).await.expect("failed to send operation");
        });

        Ok(Response::new(ReceiverStream::new(rx)))
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
