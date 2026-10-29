//! The action cache of a remote execution service.

use crate::Endpoint;
use actioncache::{ActionCache, ActionCacheError, Usage};
use async_trait::async_trait;
use cas::Digest;
use grpcclient::Channel;
use remote_execution_rust_connect::build::bazel::remote::execution::v2::{
    ActionCacheClient, ActionResult, GetActionResultRequest, UpdateActionResultRequest,
};

/// A remote service's action cache.
pub struct RemoteActionCache {
    endpoint: Endpoint,
    client: ActionCacheClient<Channel>,
}

impl RemoteActionCache {
    pub fn new(endpoint: Endpoint) -> RemoteActionCache {
        let (http, config) = endpoint.client();
        RemoteActionCache {
            client: ActionCacheClient::new(http, config),
            endpoint,
        }
    }

    fn error(&self, what: &str, error: impl std::fmt::Display) -> ActionCacheError {
        ActionCacheError::io(format!("{what} at {}", self.endpoint.uri), std::io::Error::other(error.to_string()))
    }
}

#[async_trait]
impl ActionCache for RemoteActionCache {
    async fn get(&self, action: &Digest) -> Result<Option<ActionResult>, ActionCacheError> {
        let request = GetActionResultRequest {
            instance_name: self.endpoint.instance_name.clone(),
            action_digest: action.to_proto().into(),
            ..Default::default()
        };
        match self.client.get_action_result(request).await {
            Ok(response) => Ok(Some(response.into_owned())),
            Err(error) if error.code == connectrpc::ErrorCode::NotFound => Ok(None),
            Err(error) => Err(self.error(&format!("looking up {action}"), error)),
        }
    }

    async fn put(&self, action: &Digest, result: &ActionResult) -> Result<(), ActionCacheError> {
        let request = UpdateActionResultRequest {
            instance_name: self.endpoint.instance_name.clone(),
            action_digest: action.to_proto().into(),
            action_result: result.clone().into(),
            ..Default::default()
        };
        self.client
            .update_action_result(request)
            .await
            .map_err(|error| self.error(&format!("storing the result of {action}"), error))?;
        Ok(())
    }

    async fn usage(&self) -> Result<Usage, ActionCacheError> {
        Ok(Usage::default())
    }
}
