use super::ResponseStream;
use crate::proto::remote_execution::{ExecuteRequest, Execution, WaitExecutionRequest};
use operations_proto::google::longrunning::Operation;
use tonic::{Request, Response, Status};

#[derive(Default, Debug)]
pub struct ExecutionService {}

#[tonic::async_trait]
impl Execution for ExecutionService {
    type ExecuteStream = ResponseStream<Result<Operation, Status>>;

    async fn execute(
        &self,
        _req: Request<ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        tracing::info!("Execution::execute");
        todo!()
    }

    type WaitExecutionStream = ResponseStream<Result<Operation, Status>>;

    async fn wait_execution(
        &self,
        _req: Request<WaitExecutionRequest>,
    ) -> Result<Response<Self::WaitExecutionStream>, Status> {
        tracing::info!("Execution::wait_execution");
        todo!()
    }
}
