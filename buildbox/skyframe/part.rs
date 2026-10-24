//! Parts of live values kept compressed in the engine's store.
//!
//! Some values, such as analyzed targets, hold Starlark objects and can't be
//! encoded as a whole, but much of what they hold can be. Such a value keeps
//! its encodable parts as [`Stored`] handles: the bytes live in the engine's
//! store, compressed with a dictionary trained on parts of the same type and
//! held once however many values have them, and are decoded when read.

use crate::SkyError;
use crate::engine::EngineInner;
use crate::kinds;
use skycodec::{Decode, Encode, Fingerprint};
use skystore::{Stored as StoredBytes, ValueRef};
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

/// A type whose values live values keep as [`Stored`] parts.
pub trait Part: Encode + Decode + Send + Sync + 'static {
    /// Names this kind of part in statistics, e.g. `"ACTION"`.
    const KIND: &'static str;
}

/// A part kept in an engine's store. Cloning shares it.
pub struct Stored<T: Part> {
    engine: Arc<EngineInner>,
    value: ValueRef,
    part: PhantomData<fn() -> T>,
}

impl<T: Part> Stored<T> {
    pub(crate) fn new(engine: Arc<EngineInner>, part: &T) -> Stored<T> {
        let bytes = skycodec::to_bytes(part);
        let kind = kinds::part_store_kind::<T>();
        let mut fingerprint = skycodec::Fingerprinter::new();
        fingerprint.bytes(T::KIND.as_bytes()).bytes(&bytes);
        let value = engine.store.insert(kind, fingerprint.finish(), &bytes);
        Stored {
            engine,
            value,
            part: PhantomData,
        }
    }

    /// Decode the part.
    pub fn get(&self) -> Result<T, SkyError> {
        let stored = self.engine.store.read(self.value).map_err(|error| SkyError::new(error.to_string()))?;
        let StoredBytes::Bytes(bytes) = stored else {
            return Err(SkyError::new(format!("stored {} part isn't bytes", T::KIND)));
        };
        skycodec::from_bytes(&bytes).map_err(|error| SkyError::new(format!("corrupt {} part: {error}", T::KIND)))
    }

    /// Identifies the part's contents.
    pub fn fingerprint(&self) -> Option<Fingerprint> {
        self.engine.store.fingerprint(self.value)
    }
}

impl<T: Part> Clone for Stored<T> {
    fn clone(&self) -> Stored<T> {
        self.engine.store.retain(self.value);
        Stored {
            engine: self.engine.clone(),
            value: self.value,
            part: PhantomData,
        }
    }
}

impl<T: Part> Drop for Stored<T> {
    fn drop(&mut self) {
        self.engine.release_value(self.value);
    }
}

impl<T: Part> PartialEq for Stored<T> {
    fn eq(&self, other: &Stored<T>) -> bool {
        Arc::ptr_eq(&self.engine, &other.engine) && self.value == other.value
    }
}

impl<T: Part> fmt::Debug for Stored<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Stored<{}>({})", T::KIND, self.value.id())
    }
}
