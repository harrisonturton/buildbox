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
        req: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let req = req.into_inner();
        let hash = req.action_digest.unwrap().hash;
        tracing::info!("ActionCache::get_action_result {hash}");
        Err(Status::not_found("action not found"))
    }

    async fn update_action_result(
        &self,
        req: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let req = req.into_inner();
        let hash = req.action_digest.unwrap().hash;
        tracing::info!("ActionCache::update_action_result {hash}");
        Ok(Response::new(ActionResult::default()))
    }
}
