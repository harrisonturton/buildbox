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
use std::pin::Pin;
use tokio_stream::Stream;
use tonic::Status;

pub type ResponseStream<T> = Pin<Box<dyn Stream<Item = T> + Send + 'static>>;

/// Read a proto message the client stored in the CAS.
async fn read_message<T: prost::Message + Default>(cas: &dyn Cas, digest: &Digest) -> Result<T, Status> {
    let data = match cas.read_bytes(digest).await {
        Ok(data) => data,
        Err(CasError::NotFound(digest)) => {
            return Err(Status::failed_precondition(format!("blob {digest} not found")));
        }
        Err(error) => return Err(status::from_cas_error(error)),
    };

    let type_name = std::any::type_name::<T>();
    T::decode(data.as_slice()).map_err(|error| Status::invalid_argument(format!("blob {digest} isn't a {type_name}: {error}")))
}
