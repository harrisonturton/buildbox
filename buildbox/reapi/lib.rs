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

use std::pin::Pin;
use tokio_stream::Stream;
pub type ResponseStream<T> = Pin<Box<dyn Stream<Item = T> + Send + 'static>>;
