use common::Error;
pub use proto::bazel::exec::{
    Action, ActionCache, ActionResult, Command, Digest, GetActionResultRequest, OutputFile,
    UpdateActionResultRequest,
};
use storage::Store;
use tonic::{Request, Response, Status};

#[derive(Debug)]
pub struct ActionCacheService<S>
where
    S: Store + 'static,
{
    store: S,
}

impl<S> ActionCacheService<S>
where
    S: Store + 'static,
{
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl<S> ActionCache for ActionCacheService<S>
where
    S: Store + 'static,
{
    async fn get_action_result(
        &self,
        req: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let req = req.into_inner();
        let hash = req.action_digest.clone().unwrap().hash;
        tracing::info!("ActionCache::get_action_result {req:?}");
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
