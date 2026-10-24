//! Implementation of the RPC services required for Bazel remote execution.

pub mod action_cache_service;
pub use action_cache_service::ActionCacheService;

pub mod capabilities_service;
pub use capabilities_service::CapabilitiesService;

pub mod bytestream_service;
pub use bytestream_service::ByteStreamService;

pub mod cas_service;
pub use cas_service::ContentAddressableStorageService;

pub mod execution_service;
pub use execution_service::ExecutionService;

mod cached_result;
mod status;

use cas::{Cas, CasError, Digest};
use connectrpc::ConnectError;

/// The most data a client may send or request in one batch CAS call.
pub const MAX_BATCH_TOTAL_SIZE_BYTES: u64 = 4 * 1024 * 1024;

/// The largest message the services accept. Larger than the batch limit, to
/// leave room for the rest of the message.
pub const MAX_MESSAGE_SIZE_BYTES: usize = 2 * MAX_BATCH_TOTAL_SIZE_BYTES as usize;

/// Read a proto message the client stored in the CAS.
async fn read_message<T: buffa::Message>(cas: &dyn Cas, digest: &Digest) -> Result<T, ConnectError> {
    let data = match cas.read_bytes(digest).await {
        Ok(data) => data,
        Err(CasError::NotFound(digest)) => {
            return Err(ConnectError::failed_precondition(format!("blob {digest} not found")));
        }
        Err(error) => return Err(status::from_cas_error(error)),
    };

    let type_name = std::any::type_name::<T>();
    T::decode_from_slice(&data)
        .map_err(|error| ConnectError::invalid_argument(format!("blob {digest} isn't a {type_name}: {error}")))
}
