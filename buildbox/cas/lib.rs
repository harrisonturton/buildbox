pub mod digest;
pub use digest::{Digest, DigestError, Hasher};

pub(crate) mod tee;
pub(crate) mod store;
pub(crate) mod proto;

pub mod mem;
pub mod fs;

pub use store::{Store, ReadHandle, WriteHandle};
pub use proto::ProtoStoreExt;