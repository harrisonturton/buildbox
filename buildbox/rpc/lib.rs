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
    store: &internal::blob::LocalBlobStore,
    digest: &internal::proto::remote_execution::Digest,
) -> Result<T, internal::Error>
where
    T: prost::Message + std::default::Default,
{
    let mut data = store.read_to_end(&digest.hash)?;
    let mut bytes = bytes::BytesMut::from(&data[..]);
    T::decode(&mut bytes).map_err(internal::Error::boxed)
}
