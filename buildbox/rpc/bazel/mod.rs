//! Implementation of the RPC services required for Bazel remote execution.

pub mod action_cache;
pub use action_cache::ActionCacheService;

pub mod capabilities;
pub use capabilities::CapabilitiesService;

pub mod bytestream;
pub use bytestream::ByteStreamService;

pub mod cas;
pub use cas::ContentAddressableStorageService;

pub mod execution;
pub use execution::ExecutionService;

pub mod fetch;
pub use fetch::FetchService;

pub mod push;
pub use push::PushService;

use std::pin::Pin;
use tokio_stream::Stream;
pub type ResponseStream<T> = Pin<Box<dyn Stream<Item = T> + Send + 'static>>;

pub fn read_digest<T>(
    storage: &storage::FileStore,
    digest: &proto::bazel::exec::Digest,
) -> Result<T, common::Error>
where
    T: prost::Message + std::default::Default,
{
    use storage::Store;
    storage.read_message(digest)
}

pub fn read_digest2<T, S: storage::Store + 'static>(
    storage: &S,
    digest: &proto::bazel::exec::Digest,
) -> Result<T, common::Error>
where
    T: prost::Message + std::default::Default,
{
    use storage::Store;
    storage.read_message(digest)
}