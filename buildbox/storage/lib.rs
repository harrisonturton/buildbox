pub(crate) mod tee;
pub(crate) mod store;
pub(crate) mod proto;

pub mod mem;
pub mod file;

pub use store::{Store, ReadHandle, WriteHandle};
pub use proto::ProtoStoreExt;