//! The ActionCache service: looking up and recording action results.

use crate::{cached_result, status};
use actioncache::ActionCache;
use cas::{Cas, Digest};
use remote_execution_proto::build::bazel::remote::execution::v2::action_cache_server;
use remote_execution_proto::build::bazel::remote::execution::v2::{
    ActionResult, GetActionResultRequest, UpdateActionResultRequest,
};
use std::sync::Arc;
use tonic::{Request, Response, Status};

pub struct ActionCacheService {
    cas: Arc<dyn Cas>,
    action_cache: Arc<dyn ActionCache>,
}

impl ActionCacheService {
    pub fn new(cas: Arc<dyn Cas>, action_cache: Arc<dyn ActionCache>) -> ActionCacheService {
        ActionCacheService { cas, action_cache }
    }
}

#[tonic::async_trait]
impl action_cache_server::ActionCache for ActionCacheService {
    async fn get_action_result(
        &self,
        request: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let request = request.into_inner();
        let action = Digest::from_required_proto(request.action_digest.as_ref())
            .map_err(|error| status::invalid_digest("action_digest", error))?;

        let result = cached_result::lookup(&*self.cas, &*self.action_cache, &action).await?;
        match result {
            Some(result) => Ok(Response::new(result)),
            None => Err(Status::not_found(format!("no cached result for action {action}"))),
        }
    }

    async fn update_action_result(
        &self,
        request: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        let request = request.into_inner();
        let action = Digest::from_required_proto(request.action_digest.as_ref())
            .map_err(|error| status::invalid_digest("action_digest", error))?;
        let Some(result) = request.action_result else {
            return Err(Status::invalid_argument("action_result is missing"));
        };

        self.action_cache
            .put(&action, &result)
            .await
            .map_err(status::from_action_cache_error)?;
        Ok(Response::new(result))
    }
}
