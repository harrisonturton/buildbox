//! The ActionCache service: looking up and recording action results.

use crate::{cached_result, status};
use actioncache::ActionCache;
use cas::{Cas, Digest};
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use remote_execution_rust_connect::build::bazel::remote::execution::v2;
use remote_execution_rust_connect::build::bazel::remote::execution::v2::{
    ActionResult, GetActionResultRequest, UpdateActionResultRequest,
};
use std::sync::Arc;

pub struct ActionCacheService {
    cas: Arc<dyn Cas>,
    action_cache: Arc<dyn ActionCache>,
}

impl ActionCacheService {
    pub fn new(cas: Arc<dyn Cas>, action_cache: Arc<dyn ActionCache>) -> ActionCacheService {
        ActionCacheService { cas, action_cache }
    }
}

// Generated traits return `impl Encodable<M>`; handlers narrow that to `M`.
#[allow(refining_impl_trait)]
impl v2::ActionCache for ActionCacheService {
    async fn get_action_result(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetActionResultRequest>,
    ) -> ServiceResult<ActionResult> {
        let request = request.to_owned_message();
        let action = Digest::from_required_proto(request.action_digest.as_option())
            .map_err(|error| status::invalid_digest("action_digest", error))?;

        let result = cached_result::lookup(&*self.cas, &*self.action_cache, &action).await?;
        match result {
            Some(result) => Response::ok(result),
            None => Err(ConnectError::not_found(format!("no cached result for action {action}"))),
        }
    }

    async fn update_action_result(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, UpdateActionResultRequest>,
    ) -> ServiceResult<ActionResult> {
        let request = request.to_owned_message();
        let action = Digest::from_required_proto(request.action_digest.as_option())
            .map_err(|error| status::invalid_digest("action_digest", error))?;
        let Some(result) = request.action_result.into_option() else {
            return Err(ConnectError::invalid_argument("action_result is missing"));
        };

        self.action_cache
            .put(&action, &result)
            .await
            .map_err(status::from_action_cache_error)?;
        Response::ok(result)
    }
}
