//! The executor of a remote execution service.

use crate::Endpoint;
use async_trait::async_trait;
use buffa::Message;
use cas::Digest;
use grpcclient::Channel;
use executor::{Action, ExecuteError, Executor};
use remote_execution_rust_connect::build::bazel::remote::execution::v2::{
    ActionResult, ExecuteRequest, ExecuteResponse, ExecutionClient,
};
use remote_execution_rust_connect::google::longrunning::{Operation, operation};
use remote_execution_rust_connect::google::rpc::{PreconditionFailure, Status};

/// The gRPC code for a failed precondition, which the protocol uses for
/// missing inputs.
const FAILED_PRECONDITION: i32 = 9;

/// The gRPC code for an exceeded deadline, which the protocol uses for
/// actions that time out.
const DEADLINE_EXCEEDED: i32 = 4;

/// Runs actions on a remote service. Their inputs must already be in its
/// CAS; the results' outputs are left there.
pub struct RemoteExecutor {
    endpoint: Endpoint,
    client: ExecutionClient<Channel>,
}

impl RemoteExecutor {
    pub fn new(endpoint: Endpoint) -> RemoteExecutor {
        let (http, config) = endpoint.client();
        RemoteExecutor {
            client: ExecutionClient::new(http, config),
            endpoint,
        }
    }

    fn error(&self, error: impl std::fmt::Display) -> ExecuteError {
        ExecuteError::Internal(format!("remote execution at {}: {error}", self.endpoint.uri))
    }

    /// Wait for the operation an `Execute` call started to finish.
    async fn finished_operation(&self, action: &Action) -> Result<Operation, ExecuteError> {
        let request = ExecuteRequest {
            instance_name: self.endpoint.instance_name.clone(),
            // The caller has looked in the action cache already.
            skip_cache_lookup: true,
            action_digest: action.digest.to_proto().into(),
            ..Default::default()
        };
        let mut stream = self.client.execute(request).await.map_err(|error| self.error(error))?;
        while let Some(message) = stream.message().await.map_err(|error| self.error(error))? {
            let operation = message.to_owned_message();
            if operation.done {
                return Ok(operation);
            }
        }
        Err(self.error("the operation ended without finishing"))
    }

    fn response(&self, operation: Operation, action: &Action) -> Result<ActionResult, ExecuteError> {
        let response = match operation.result {
            Some(operation::Result::Response(any)) => ExecuteResponse::decode_from_slice(&any.value).map_err(|error| self.error(error))?,
            Some(operation::Result::Error(status)) => return Err(self.status_error(&status, action)),
            None => return Err(self.error("the operation finished without a result")),
        };
        if let Some(status) = response.status.as_option().filter(|status| status.code != 0) {
            return Err(self.status_error(status, action));
        }
        Ok(response.result.as_option().cloned().unwrap_or_default())
    }

    fn status_error(&self, status: &Status, action: &Action) -> ExecuteError {
        match status.code {
            FAILED_PRECONDITION => ExecuteError::MissingInputs(missing_blobs(status)),
            DEADLINE_EXCEEDED => ExecuteError::Timeout(action.timeout.unwrap_or_default()),
            code => self.error(format!("status {code}: {}", status.message)),
        }
    }
}

/// The blobs a failed precondition says are missing.
fn missing_blobs(status: &Status) -> Vec<Digest> {
    let mut missing = Vec::new();
    for detail in &status.details {
        let Ok(failure) = PreconditionFailure::decode_from_slice(&detail.value) else {
            continue;
        };
        for violation in &failure.violations {
            let Some(rest) = violation.subject.strip_prefix("blobs/") else {
                continue;
            };
            let Some((hash, size)) = rest.split_once('/') else {
                continue;
            };
            if let Some(digest) = size.parse().ok().and_then(|size| Digest::new(hash, size).ok()) {
                missing.push(digest);
            }
        }
    }
    missing
}

#[async_trait]
impl Executor for RemoteExecutor {
    async fn execute(&self, action: &Action) -> Result<ActionResult, ExecuteError> {
        let operation = self.finished_operation(action).await?;
        self.response(operation, action)
    }
}
