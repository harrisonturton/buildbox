//! The traits that keys and values implement.

use crate::{Ctx, SkyError};
use skycodec::{Decode, DecodeError, Encode, Fingerprint};
use std::fmt::Debug;
use std::future::Future;
use std::hash::Hash;

/// Identifies a value in the graph, and says how to compute it.
///
/// A key's value must be a function of the key and of the values of the
/// keys it asks for through [`Ctx`]; nothing else. That is what lets
/// workspaces share computed values: a value computed in one workspace is
/// reused by another whose dependencies have the same values. The exceptions
/// are [`EXTERNAL`](Key::EXTERNAL) keys, which read the world outside the
/// graph.
pub trait Key: Encode + Decode + Clone + Eq + Hash + Debug + Send + Sync + 'static {
    type Value: Value;

    /// Names this kind of key in statistics and spans, e.g. `"PACKAGE"`.
    const KIND: &'static str;

    /// Whether values depend on something outside the graph, such as a file.
    /// They are never shared between workspaces, and are computed again
    /// only when [invalidated](crate::Graph::invalidate).
    const EXTERNAL: bool = false;

    fn compute(&self, ctx: &Ctx) -> impl Future<Output = Result<Self::Value, SkyError>> + Send;
}

/// A value in the graph.
///
/// Most values are encoded with [`skycodec`] and kept compressed;
/// [`encoded_value!`](crate::encoded_value) implements this trait for those.
/// Values that can't be encoded, such as evaluated Starlark modules, are kept
/// as live objects instead, and say what identifies them.
pub trait Value: Sized + Send + Sync + 'static {
    fn to_storage(&self) -> Storage;

    /// Decode a value that [`Value::to_storage`] encoded.
    fn from_bytes(bytes: &[u8]) -> Result<Self, DecodeError>;
}

/// How a [`Value`] is kept.
pub enum Storage {
    /// As its encoding, which also identifies it.
    Encoded(Vec<u8>),
    /// As a live object. Values with the same fingerprint must be equal.
    /// `size` is roughly how many bytes it takes, for accounting.
    Live { fingerprint: Fingerprint, size: usize },
}

/// Implement [`Value`] for types that implement `Encode` and `Decode`.
#[macro_export]
macro_rules! encoded_value {
    ($($ty:ty),* $(,)?) => {$(
        impl $crate::Value for $ty {
            fn to_storage(&self) -> $crate::Storage {
                $crate::Storage::Encoded($crate::skycodec::to_bytes(self))
            }

            fn from_bytes(bytes: &[u8]) -> Result<Self, $crate::skycodec::DecodeError> {
                $crate::skycodec::from_bytes(bytes)
            }
        }
    )*};
}
