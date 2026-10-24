//! The registry of key types, so keys can be computed from their encoding.

use crate::key::{Key, Storage, Value};
use crate::{Ctx, SkyError};
use futures::future::BoxFuture;
use skycodec::from_bytes;
use skystore::Kind;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};

/// The most key types a process can register.
pub(crate) const MAX_KINDS: usize = 1024;

/// Identifies a key type, numbered in the order types are first used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct KindId(pub u16);

/// The store kind that errors are kept under.
pub(crate) const ERROR_STORE_KIND: Kind = Kind(0);

impl KindId {
    /// The store kind for values of this kind of key.
    pub(crate) fn value_store_kind(self) -> Kind {
        Kind(2 + 2 * self.0)
    }

    /// The store kind for traces of this kind of key.
    pub(crate) fn trace_store_kind(self) -> Kind {
        Kind(3 + 2 * self.0)
    }
}

/// A value just computed: how to store it, and the value itself, ready to
/// be cached.
pub(crate) struct Computed {
    pub storage: Storage,
    pub value: Arc<dyn Any + Send + Sync>,
}

type ComputeFn = fn(Ctx, &[u8]) -> BoxFuture<'static, Result<Computed, SkyError>>;
type DescribeFn = fn(&[u8]) -> String;

/// What the graph needs to know about a key type.
pub(crate) struct KindInfo {
    pub name: &'static str,
    pub external: bool,
    pub compute: ComputeFn,
    pub describe: DescribeFn,
}

#[derive(Default)]
struct Registry {
    by_type: HashMap<TypeId, KindId>,
    infos: Vec<&'static KindInfo>,
}

static REGISTRY: LazyLock<RwLock<Registry>> = LazyLock::new(Default::default);

/// The id of key type `K`, registering it if needed.
pub(crate) fn kind_of<K: Key>() -> KindId {
    let type_id = TypeId::of::<K>();
    {
        let registry = REGISTRY.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(&kind) = registry.by_type.get(&type_id) {
            return kind;
        }
    }

    let mut registry = REGISTRY.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(&kind) = registry.by_type.get(&type_id) {
        return kind;
    }
    assert!(registry.infos.len() < MAX_KINDS, "more than {MAX_KINDS} key types");
    let kind = KindId(registry.infos.len() as u16);
    let info: &'static KindInfo = Box::leak(Box::new(KindInfo {
        name: K::KIND,
        external: K::EXTERNAL,
        compute: compute_erased::<K>,
        describe: describe::<K>,
    }));
    registry.infos.push(info);
    registry.by_type.insert(type_id, kind);
    kind
}

pub(crate) fn info(kind: KindId) -> &'static KindInfo {
    let registry = REGISTRY.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.infos[kind.0 as usize]
}

/// Every registered kind's name, indexed by kind.
pub(crate) fn names() -> Vec<&'static str> {
    let registry = REGISTRY.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.infos.iter().map(|info| info.name).collect()
}

fn compute_erased<K: Key>(ctx: Ctx, bytes: &[u8]) -> BoxFuture<'static, Result<Computed, SkyError>> {
    let key = from_bytes::<K>(bytes);
    Box::pin(async move {
        let key = key.map_err(|error| SkyError::new(format!("corrupt {} key: {error}", K::KIND)))?;
        let value = key.compute(&ctx).await?;
        Ok(Computed {
            storage: value.to_storage(),
            value: Arc::new(value),
        })
    })
}

fn describe<K: Key>(bytes: &[u8]) -> String {
    match from_bytes::<K>(bytes) {
        Ok(key) => format!("{}:{key:?}", K::KIND),
        Err(_) => format!("{}:<corrupt>", K::KIND),
    }
}

/// Decode a value of key type `K` from the store's bytes.
pub(crate) fn decode_value<K: Key>(bytes: &[u8]) -> Result<K::Value, SkyError> {
    K::Value::from_bytes(bytes).map_err(|error| SkyError::new(format!("corrupt {} value: {error}", K::KIND)))
}
