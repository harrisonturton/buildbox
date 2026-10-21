//! The Execution service, which runs actions.

use crate::status;
use crate::ResponseStream;
use cas::{Cas, Digest};
use executor::Executor;
use proto::bazel::exec::{Action, Command, ExecuteRequest, ExecuteResponse, Execution, WaitExecutionRequest};
use proto::google::longrunning::{operation, Operation};
use proto::google::protobuf::Any;
use proto::google::rpc;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

pub struct ExecutionService {
    cas: Arc<dyn Cas>,
    executor: Arc<dyn Executor>,
}

impl ExecutionService {
    pub fn new(cas: Arc<dyn Cas>, executor: Arc<dyn Executor>) -> ExecutionService {
        ExecutionService { cas, executor }
    }

    async fn run(&self, request: &ExecuteRequest) -> Result<ExecuteResponse, Status> {
        let action_digest = Digest::from_required_proto(request.action_digest.as_ref())
            .map_err(|error| status::invalid_digest("action_digest", error))?;
        let action: Action = crate::read_message(&*self.cas, &action_digest).await?;

        let command_digest = Digest::from_required_proto(action.command_digest.as_ref())
            .map_err(|error| status::invalid_digest("command_digest", error))?;
        let command: Command = crate::read_message(&*self.cas, &command_digest).await?;

        let input_root = Digest::from_required_proto(action.input_root_digest.as_ref())
            .map_err(|error| status::invalid_digest("input_root_digest", error))?;

        let timeout = match &action.timeout {
            Some(timeout) => Some(parse_timeout(timeout.seconds, timeout.nanos)?),
            None => None,
        };

        let executable = executor::Action {
            digest: action_digest,
            command,
            input_root,
            timeout,
        };
        let result = self
            .executor
            .execute(&executable)
            .await
            .map_err(status::from_execute_error)?;

        Ok(ExecuteResponse {
            result: Some(result),
            status: Some(rpc::Status::default()),
            ..Default::default()
        })
    }
}

#[tonic::async_trait]
impl Execution for ExecutionService {
    type ExecuteStream = ReceiverStream<Result<Operation, Status>>;

    async fn execute(&self, request: Request<ExecuteRequest>) -> Result<Response<Self::ExecuteStream>, Status> {
        let request = request.into_inner();
        let response = self.run(&request).await?;

        let response = Any {
            type_url: "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse".to_string(),
            value: prost::Message::encode_to_vec(&response),
        };
        let operation = Operation {
            // TODO: Track operations so they can be waited on.
            name: "operations/fake-id".to_string(),
            metadata: None,
            done: true,
            result: Some(operation::Result::Response(response)),
        };

        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(Ok(operation))
            .await
            .map_err(|_| Status::internal("failed to send execute response"))?;
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    type WaitExecutionStream = ResponseStream<Result<Operation, Status>>;

    async fn wait_execution(
        &self,
        _request: Request<WaitExecutionRequest>,
    ) -> Result<Response<Self::WaitExecutionStream>, Status> {
        Err(Status::unimplemented("WaitExecution is not supported yet"))
    }
}

fn parse_timeout(seconds: i64, nanos: i32) -> Result<Duration, Status> {
    let (Ok(seconds), Ok(nanos)) = (u64::try_from(seconds), u32::try_from(nanos)) else {
        return Err(Status::invalid_argument("timeout must not be negative"));
    };
    Ok(Duration::new(seconds, nanos))
}
