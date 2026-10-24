//! Converting errors from the storage and execution layers into RPC errors.

use actioncache::ActionCacheError;
use cas::{CasError, DigestError};
use connectrpc::ConnectError;
use executor::ExecuteError;
use remote_execution_rust_connect::google::rpc;

pub fn from_cas_error(error: CasError) -> ConnectError {
    match error {
        CasError::NotFound(_) => ConnectError::not_found(error.to_string()),
        CasError::DigestMismatch { .. } => ConnectError::invalid_argument(error.to_string()),
        CasError::Io { .. } => ConnectError::internal(error.to_string()),
    }
}

pub fn from_action_cache_error(error: ActionCacheError) -> ConnectError {
    match error {
        ActionCacheError::Io { .. } => ConnectError::internal(error.to_string()),
    }
}

pub fn from_execute_error(error: ExecuteError) -> ConnectError {
    match error {
        ExecuteError::MissingInputs(_) => ConnectError::failed_precondition(error.to_string()),
        ExecuteError::InvalidAction(_) => ConnectError::invalid_argument(error.to_string()),
        ExecuteError::InvalidOutput(_) => ConnectError::failed_precondition(error.to_string()),
        ExecuteError::Timeout(_) => ConnectError::deadline_exceeded(error.to_string()),
        ExecuteError::Internal(_) => ConnectError::internal(error.to_string()),
    }
}

/// A digest in a request was invalid. `field` names it, for the message.
pub fn invalid_digest(field: &str, error: DigestError) -> ConnectError {
    ConnectError::invalid_argument(format!("{field}: {error}"))
}

/// The `google.rpc.Status` for an error, for responses that report a status
/// per item rather than failing the whole call.
pub fn to_rpc_status(error: ConnectError) -> rpc::Status {
    rpc::Status {
        code: error.code.grpc_code() as i32,
        message: error.message.unwrap_or_default(),
        ..Default::default()
    }
}
