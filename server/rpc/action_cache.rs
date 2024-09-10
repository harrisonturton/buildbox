use crate::proto::remote_execution::{
    ActionCache, ActionResult, GetActionResultRequest, UpdateActionResultRequest,
};
use tonic::{Request, Response, Status};

#[derive(Default, Debug)]
pub struct ActionCacheService {}

#[tonic::async_trait]
impl ActionCache for ActionCacheService {
    async fn get_action_result(
        &self,
        _req: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        tracing::info!("ActionCache::get_action_result");
        // Err(Status::internal("not implemented"))
        Ok(Response::new(ActionResult::default()))
    }

    async fn update_action_result(
        &self,
        _req: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        tracing::info!("ActionCache::update_action_result");
        Ok(Response::new(ActionResult::default()))
    }
}
