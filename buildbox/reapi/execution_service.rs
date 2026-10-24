//! The Execution service, which runs actions.

use crate::{cached_result, status};
use actioncache::ActionCache;
use buffa_types::google::protobuf::Any;
use cas::{Cas, Digest};
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream};
use executor::{ExecuteError, Executor};
use remote_execution_rust_connect::build::bazel::remote::execution::v2::{
    Action, ActionResult, Command, ExecuteRequest, ExecuteResponse, Execution, WaitExecutionRequest,
};
use remote_execution_rust_connect::google::longrunning::{operation, Operation};
use remote_execution_rust_connect::google::rpc;
use remote_execution_rust_connect::google::rpc::precondition_failure::Violation;
use remote_execution_rust_connect::google::rpc::PreconditionFailure;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::Instrument;

/// The violation type the protocol uses for blobs missing from the CAS.
const MISSING_VIOLATION_TYPE: &str = "MISSING";

pub struct ExecutionService {
    cas: Arc<dyn Cas>,
    action_cache: Arc<dyn ActionCache>,
    executor: Arc<dyn Executor>,
    next_operation_id: AtomicU64,
}

impl ExecutionService {
    pub fn new(cas: Arc<dyn Cas>, action_cache: Arc<dyn ActionCache>, executor: Arc<dyn Executor>) -> ExecutionService {
        ExecutionService {
            cas,
            action_cache,
            executor,
            next_operation_id: AtomicU64::new(0),
        }
    }

    /// Run the requested action, or return its cached result.
    ///
    /// Problems with the request itself are returned as errors. Problems
    /// running the action are reported in the response's status.
    async fn run(&self, request: &ExecuteRequest) -> Result<ExecuteResponse, ConnectError> {
        let action_digest = Digest::from_required_proto(request.action_digest.as_option())
            .map_err(|error| status::invalid_digest("action_digest", error))?;
        telemetry::set_attribute("action.digest", action_digest.to_string());

        if !request.skip_cache_lookup {
            let cached = cached_result::lookup(&*self.cas, &*self.action_cache, &action_digest).await?;
            if let Some(result) = cached {
                tracing::info!("using cached result for action {action_digest}");
                telemetry::set_attribute("action.cached", true);
                return Ok(cached_response(result));
            }
        }
        telemetry::set_attribute("action.cached", false);

        let read_action = crate::read_message(&*self.cas, &action_digest).instrument(tracing::info_span!("read_action"));
        let action: Action = read_action.await?;
        let executable = self.resolve(&action_digest, &action).await?;

        let result = match self.executor.execute(&executable).await {
            Ok(result) => result,
            Err(error) => return Ok(error_response(error)),
        };
        telemetry::set_attribute("action.exit_code", result.exit_code);

        if result.exit_code == 0 && !action.do_not_cache {
            self.action_cache
                .put(&action_digest, &result)
                .instrument(tracing::info_span!("update_action_cache"))
                .await
                .map_err(status::from_action_cache_error)?;
        }
        Ok(executed_response(result))
    }

    /// Fetch everything the executor needs to run `action`.
    #[tracing::instrument(name = "read_command", skip_all)]
    async fn resolve(&self, action_digest: &Digest, action: &Action) -> Result<executor::Action, ConnectError> {
        let command_digest = Digest::from_required_proto(action.command_digest.as_option())
            .map_err(|error| status::invalid_digest("command_digest", error))?;
        let command: Command = crate::read_message(&*self.cas, &command_digest).await?;

        let input_root = Digest::from_required_proto(action.input_root_digest.as_option())
            .map_err(|error| status::invalid_digest("input_root_digest", error))?;

        let timeout = match action.timeout.as_option() {
            Some(timeout) => Some(parse_timeout(timeout)?),
            None => None,
        };

        Ok(executor::Action {
            digest: action_digest.clone(),
            command,
            input_root,
            timeout,
        })
    }

    fn new_operation_name(&self) -> String {
        let id = self.next_operation_id.fetch_add(1, Ordering::Relaxed);
        format!("operations/{id}")
    }
}

// Generated traits return `impl Encodable<M>`; handlers narrow that to `M`.
#[allow(refining_impl_trait)]
impl Execution for ExecutionService {
    async fn execute(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, ExecuteRequest>,
    ) -> ServiceResult<ServiceStream<Operation>> {
        let request = request.to_owned_message();
        // Run the action before responding, so that if the client goes away
        // this future is dropped, which stops the action.
        let response = self.run(&request).await?;

        let operation = Operation {
            name: self.new_operation_name(),
            done: true,
            result: Some(operation::Result::Response(Box::new(Any::pack(
                &response,
                ExecuteResponse::TYPE_URL,
            )))),
            ..Default::default()
        };
        let stream: ServiceStream<Operation> = Box::pin(tokio_stream::once(Ok(operation)));
        Response::ok(stream)
    }

    /// Operations are complete by the time `Execute` returns them, so there
    /// is never one to wait for. NOT_FOUND tells the client to execute again.
    async fn wait_execution(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, WaitExecutionRequest>,
    ) -> ServiceResult<ServiceStream<Operation>> {
        let name = request.name;
        Err(ConnectError::not_found(format!("operation {name:?} not found")))
    }
}

fn cached_response(result: ActionResult) -> ExecuteResponse {
    ExecuteResponse {
        result: result.into(),
        cached_result: true,
        status: rpc::Status::default().into(),
        ..Default::default()
    }
}

fn executed_response(result: ActionResult) -> ExecuteResponse {
    ExecuteResponse {
        result: result.into(),
        status: rpc::Status::default().into(),
        ..Default::default()
    }
}

fn error_response(error: ExecuteError) -> ExecuteResponse {
    tracing::info!("action failed: {error}");
    telemetry::set_error(error.to_string());
    let details = match &error {
        ExecuteError::MissingInputs(digests) => vec![missing_blobs_details(digests)],
        _ => vec![],
    };

    let status = rpc::Status {
        details,
        ..status::to_rpc_status(status::from_execute_error(error))
    };
    ExecuteResponse {
        status: status.into(),
        ..Default::default()
    }
}

/// Details listing missing blobs, in the form clients use to decide what to
/// upload before retrying.
fn missing_blobs_details(digests: &[Digest]) -> Any {
    let violations = digests
        .iter()
        .map(|digest| Violation {
            r#type: MISSING_VIOLATION_TYPE.to_string(),
            subject: format!("blobs/{digest}"),
            ..Default::default()
        })
        .collect();

    let failure = PreconditionFailure {
        violations,
        ..Default::default()
    };
    Any::pack(&failure, PreconditionFailure::TYPE_URL)
}

fn parse_timeout(timeout: &buffa_types::google::protobuf::Duration) -> Result<Duration, ConnectError> {
    Duration::try_from(timeout.clone()).map_err(|error| ConnectError::invalid_argument(format!("timeout: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use actioncache_mem::MemActionCache;
    use buffa::Message;
    use cas_mem::MemCas;
    use connectrpc::ErrorCode;
    use remote_execution_rust_connect::build::bazel::remote::execution::v2::Directory;
    use std::sync::atomic::AtomicUsize;

    /// An executor that returns a fixed outcome and counts its calls.
    struct FakeExecutor {
        exit_code: i32,
        missing: Option<Digest>,
        calls: AtomicUsize,
    }

    impl FakeExecutor {
        fn exiting_with(exit_code: i32) -> Arc<FakeExecutor> {
            Arc::new(FakeExecutor {
                exit_code,
                missing: None,
                calls: AtomicUsize::new(0),
            })
        }

        fn missing(digest: Digest) -> Arc<FakeExecutor> {
            Arc::new(FakeExecutor {
                exit_code: 0,
                missing: Some(digest),
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Executor for FakeExecutor {
        async fn execute(&self, _action: &executor::Action) -> Result<ActionResult, ExecuteError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(digest) = &self.missing {
                return Err(ExecuteError::MissingInputs(vec![digest.clone()]));
            }
            Ok(ActionResult {
                exit_code: self.exit_code,
                ..Default::default()
            })
        }
    }

    struct Fixture {
        service: ExecutionService,
        executor: Arc<FakeExecutor>,
        action_digest: Digest,
    }

    async fn fixture(executor: Arc<FakeExecutor>, do_not_cache: bool) -> Fixture {
        let cas = Arc::new(MemCas::new());
        let command = cas.put_bytes(&Command::default().encode_to_vec()).await.unwrap();
        let input_root = cas.put_bytes(&Directory::default().encode_to_vec()).await.unwrap();
        let action = Action {
            command_digest: command.to_proto().into(),
            input_root_digest: input_root.to_proto().into(),
            do_not_cache,
            ..Default::default()
        };
        let action_digest = cas.put_bytes(&action.encode_to_vec()).await.unwrap();

        let service = ExecutionService::new(cas, Arc::new(MemActionCache::new()), executor.clone());
        Fixture {
            service,
            executor,
            action_digest,
        }
    }

    fn request(action_digest: &Digest, skip_cache_lookup: bool) -> ExecuteRequest {
        ExecuteRequest {
            action_digest: action_digest.to_proto().into(),
            skip_cache_lookup,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn caches_successful_results() {
        let f = fixture(FakeExecutor::exiting_with(0), false).await;

        let first = f.service.run(&request(&f.action_digest, false)).await.unwrap();
        let second = f.service.run(&request(&f.action_digest, false)).await.unwrap();

        assert!(!first.cached_result);
        assert!(second.cached_result);
        assert_eq!(f.executor.calls(), 1);
    }

    #[tokio::test]
    async fn skip_cache_lookup_runs_again() {
        let f = fixture(FakeExecutor::exiting_with(0), false).await;

        f.service.run(&request(&f.action_digest, false)).await.unwrap();
        let second = f.service.run(&request(&f.action_digest, true)).await.unwrap();

        assert!(!second.cached_result);
        assert_eq!(f.executor.calls(), 2);
    }

    #[tokio::test]
    async fn does_not_cache_failures() {
        let f = fixture(FakeExecutor::exiting_with(1), false).await;

        f.service.run(&request(&f.action_digest, false)).await.unwrap();
        f.service.run(&request(&f.action_digest, false)).await.unwrap();

        assert_eq!(f.executor.calls(), 2);
    }

    #[tokio::test]
    async fn respects_do_not_cache() {
        let f = fixture(FakeExecutor::exiting_with(0), true).await;

        f.service.run(&request(&f.action_digest, false)).await.unwrap();
        f.service.run(&request(&f.action_digest, false)).await.unwrap();

        assert_eq!(f.executor.calls(), 2);
    }

    #[tokio::test]
    async fn reports_missing_inputs_as_precondition_failures() {
        let missing = Digest::of_bytes(b"missing");
        let f = fixture(FakeExecutor::missing(missing.clone()), false).await;

        let response = f.service.run(&request(&f.action_digest, false)).await.unwrap();

        let status = response.status.unwrap();
        assert_eq!(status.code, ErrorCode::FailedPrecondition.grpc_code() as i32);
        let failure: PreconditionFailure = status.details[0].unpack_unchecked().unwrap();
        assert_eq!(failure.violations[0].r#type, "MISSING");
        assert_eq!(failure.violations[0].subject, format!("blobs/{missing}"));
    }

    #[tokio::test]
    async fn rejects_requests_for_missing_actions() {
        let f = fixture(FakeExecutor::exiting_with(0), false).await;
        let absent = Digest::of_bytes(b"no such action");

        let err = f.service.run(&request(&absent, false)).await.unwrap_err();

        assert_eq!(err.code, ErrorCode::FailedPrecondition);
    }
}
