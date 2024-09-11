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
        req: Request<ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        let req = req.into_inner();
        tracing::info!("Execution::execute {req:?}");
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
