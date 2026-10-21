//! Converting errors from the storage and execution layers into gRPC
//! statuses.

use actioncache::ActionCacheError;
use cas::{CasError, DigestError};
use executor::ExecuteError;
use tonic::Status;

pub fn from_cas_error(error: CasError) -> Status {
    match error {
        CasError::NotFound(_) => Status::not_found(error.to_string()),
        CasError::DigestMismatch { .. } => Status::invalid_argument(error.to_string()),
        CasError::Io { .. } => Status::internal(error.to_string()),
    }
}

pub fn from_action_cache_error(error: ActionCacheError) -> Status {
    match error {
        ActionCacheError::Io { .. } => Status::internal(error.to_string()),
    }
}

pub fn from_execute_error(error: ExecuteError) -> Status {
    match error {
        ExecuteError::MissingInputs(_) => Status::failed_precondition(error.to_string()),
        ExecuteError::InvalidAction(_) => Status::invalid_argument(error.to_string()),
        ExecuteError::InvalidOutput(_) => Status::failed_precondition(error.to_string()),
        ExecuteError::Timeout(_) => Status::deadline_exceeded(error.to_string()),
        ExecuteError::Internal(_) => Status::internal(error.to_string()),
    }
}

/// A digest in a request was invalid. `field` names it, for the message.
pub fn invalid_digest(field: &str, error: DigestError) -> Status {
    Status::invalid_argument(format!("{field}: {error}"))
}
