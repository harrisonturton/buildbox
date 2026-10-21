//! The Execution service, which runs actions.

use crate::{cached_result, status, ResponseStream};
use actioncache::ActionCache;
use any_proto::google::protobuf::Any;
use cas::{Cas, Digest};
use error_details_proto::google::rpc::precondition_failure::Violation;
use error_details_proto::google::rpc::PreconditionFailure;
use executor::{ExecuteError, Executor};
use operations_proto::google::longrunning::{operation, Operation};
use prost::Message;
use remote_execution_proto::build::bazel::remote::execution::v2::execution_server::Execution;
use remote_execution_proto::build::bazel::remote::execution::v2::{
    Action, ActionResult, Command, ExecuteRequest, ExecuteResponse, WaitExecutionRequest,
};
use status_proto::google::rpc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

const EXECUTE_RESPONSE_TYPE_URL: &str = "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse";
const PRECONDITION_FAILURE_TYPE_URL: &str = "type.googleapis.com/google.rpc.PreconditionFailure";

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
    async fn run(&self, request: &ExecuteRequest) -> Result<ExecuteResponse, Status> {
        let action_digest = Digest::from_required_proto(request.action_digest.as_ref())
            .map_err(|error| status::invalid_digest("action_digest", error))?;

        if !request.skip_cache_lookup {
            let cached = cached_result::lookup(&*self.cas, &*self.action_cache, &action_digest).await?;
            if let Some(result) = cached {
                tracing::info!("using cached result for action {action_digest}");
                return Ok(cached_response(result));
            }
        }

        let action: Action = crate::read_message(&*self.cas, &action_digest).await?;
        let executable = self.resolve(&action_digest, &action).await?;

        let result = match self.executor.execute(&executable).await {
            Ok(result) => result,
            Err(error) => return Ok(error_response(error)),
        };

        if result.exit_code == 0 && !action.do_not_cache {
            self.action_cache
                .put(&action_digest, &result)
                .await
                .map_err(status::from_action_cache_error)?;
        }
        Ok(executed_response(result))
    }

    /// Fetch everything the executor needs to run `action`.
    async fn resolve(&self, action_digest: &Digest, action: &Action) -> Result<executor::Action, Status> {
        let command_digest = Digest::from_required_proto(action.command_digest.as_ref())
            .map_err(|error| status::invalid_digest("command_digest", error))?;
        let command: Command = crate::read_message(&*self.cas, &command_digest).await?;

        let input_root = Digest::from_required_proto(action.input_root_digest.as_ref())
            .map_err(|error| status::invalid_digest("input_root_digest", error))?;

        let timeout = match &action.timeout {
            Some(timeout) => Some(parse_timeout(timeout.seconds, timeout.nanos)?),
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

#[tonic::async_trait]
impl Execution for ExecutionService {
    type ExecuteStream = ReceiverStream<Result<Operation, Status>>;

    async fn execute(&self, request: Request<ExecuteRequest>) -> Result<Response<Self::ExecuteStream>, Status> {
        let request = request.into_inner();
        // Run the action before responding, so that if the client goes away
        // this future is dropped, which stops the action.
        let response = self.run(&request).await?;

        let operation = Operation {
            name: self.new_operation_name(),
            metadata: None,
            done: true,
            result: Some(operation::Result::Response(Any {
                type_url: EXECUTE_RESPONSE_TYPE_URL.to_string(),
                value: response.encode_to_vec(),
            })),
        };

        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        // The receiver is still held, so this can't fail.
        let _ = sender.send(Ok(operation)).await;
        Ok(Response::new(ReceiverStream::new(receiver)))
    }

    type WaitExecutionStream = ResponseStream<Result<Operation, Status>>;

    /// Operations are complete by the time `Execute` returns them, so there
    /// is never one to wait for. NOT_FOUND tells the client to execute again.
    async fn wait_execution(
        &self,
        request: Request<WaitExecutionRequest>,
    ) -> Result<Response<Self::WaitExecutionStream>, Status> {
        let name = request.into_inner().name;
        Err(Status::not_found(format!("operation {name:?} not found")))
    }
}

fn cached_response(result: ActionResult) -> ExecuteResponse {
    ExecuteResponse {
        result: Some(result),
        cached_result: true,
        status: Some(rpc::Status::default()),
        ..Default::default()
    }
}

fn executed_response(result: ActionResult) -> ExecuteResponse {
    ExecuteResponse {
        result: Some(result),
        status: Some(rpc::Status::default()),
        ..Default::default()
    }
}

fn error_response(error: ExecuteError) -> ExecuteResponse {
    tracing::info!("action failed: {error}");
    let details = match &error {
        ExecuteError::MissingInputs(digests) => vec![missing_blobs_details(digests)],
        _ => vec![],
    };

    let status = status::from_execute_error(error);
    ExecuteResponse {
        status: Some(rpc::Status {
            code: status.code() as i32,
            message: status.message().to_string(),
            details,
        }),
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
            description: String::new(),
        })
        .collect();

    let failure = PreconditionFailure { violations };
    Any {
        type_url: PRECONDITION_FAILURE_TYPE_URL.to_string(),
        value: failure.encode_to_vec(),
    }
}

fn parse_timeout(seconds: i64, nanos: i32) -> Result<Duration, Status> {
    let (Ok(seconds), Ok(nanos)) = (u64::try_from(seconds), u32::try_from(nanos)) else {
        return Err(Status::invalid_argument("timeout must not be negative"));
    };
    Ok(Duration::new(seconds, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Code;
    use actioncache_mem::MemActionCache;
    use cas_mem::MemCas;
    use remote_execution_proto::build::bazel::remote::execution::v2::Directory;
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

    #[tonic::async_trait]
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
            command_digest: Some(command.to_proto()),
            input_root_digest: Some(input_root.to_proto()),
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
            action_digest: Some(action_digest.to_proto()),
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
        assert_eq!(status.code, Code::FailedPrecondition as i32);
        let failure = PreconditionFailure::decode(status.details[0].value.as_slice()).unwrap();
        assert_eq!(failure.violations[0].r#type, "MISSING");
        assert_eq!(failure.violations[0].subject, format!("blobs/{missing}"));
    }

    #[tokio::test]
    async fn rejects_requests_for_missing_actions() {
        let f = fixture(FakeExecutor::exiting_with(0), false).await;
        let absent = Digest::of_bytes(b"no such action");

        let err = f.service.run(&request(&absent, false)).await.unwrap_err();

        assert_eq!(err.code(), Code::FailedPrecondition);
    }
}
