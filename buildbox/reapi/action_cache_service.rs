//! The ActionCache service: looking up and recording action results.

use proto::bazel::exec::{ActionCache, ActionResult, GetActionResultRequest, UpdateActionResultRequest};
use tonic::{Request, Response, Status};

#[derive(Debug, Default)]
pub struct ActionCacheService {}

#[tonic::async_trait]
impl ActionCache for ActionCacheService {
    async fn get_action_result(
        &self,
        _request: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        Err(Status::not_found("action not found"))
    }

    async fn update_action_result(
        &self,
        _request: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        Err(Status::unimplemented("UpdateActionResult is not supported yet"))
    }
}
