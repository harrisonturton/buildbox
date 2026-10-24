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

/// Key types by id. Ids can be assigned to names before their types are
/// registered, so that a restored engine's keys keep their kinds.
#[derive(Default)]
struct Registry {
    by_type: HashMap<TypeId, KindId>,
    by_name: HashMap<String, KindId>,
    infos: Vec<Option<&'static KindInfo>>,
}

static REGISTRY: LazyLock<RwLock<Registry>> = LazyLock::new(Default::default);

/// The id of key type `K`, registering it if needed.
pub(crate) fn kind_of<K: Key>() -> KindId {
    let type_id = TypeId::of::<K>();
    if let Some(&kind) = read_registry().by_type.get(&type_id) {
        return kind;
    }
    let mut registry = REGISTRY.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(&kind) = registry.by_type.get(&type_id) {
        return kind;
    }
    let kind = match registry.by_name.get(K::KIND) {
        Some(&kind) => kind,
        None => {
            assert!(registry.infos.len() < MAX_KINDS, "more than {MAX_KINDS} key types");
            registry.infos.push(None);
            KindId((registry.infos.len() - 1) as u16)
        }
    };
    let info: &'static KindInfo = Box::leak(Box::new(KindInfo {
        name: K::KIND,
        external: K::EXTERNAL,
        compute: compute_erased::<K>,
        describe: describe::<K>,
    }));
    registry.infos[kind.0 as usize] = Some(info);
    registry.by_type.insert(type_id, kind);
    registry.by_name.insert(K::KIND.to_string(), kind);
    kind
}

/// What's known of a kind, if its key type has been registered.
pub(crate) fn info(kind: KindId) -> Option<&'static KindInfo> {
    read_registry().infos.get(kind.0 as usize).copied().flatten()
}

/// Every kind's name, indexed by kind, or `None` for ids assigned to names
/// whose types aren't registered yet.
pub(crate) fn names() -> Vec<Option<&'static str>> {
    read_registry().infos.iter().map(|info| info.map(|info| info.name)).collect()
}

/// Every kind id's name, registered or assigned, indexed by kind.
pub(crate) fn assigned_names() -> Vec<String> {
    let registry = read_registry();
    let mut names = vec![String::new(); registry.infos.len()];
    for (name, kind) in &registry.by_name {
        names[kind.0 as usize] = name.clone();
    }
    names
}

/// A name already has a different id than a saved engine gave it.
#[derive(Debug)]
pub(crate) struct KindConflict(pub String);

/// Give each name the id it had when an engine was saved: the name at
/// index `i` gets id `i`.
pub(crate) fn assign(names: &[String]) -> Result<(), KindConflict> {
    let mut registry = REGISTRY.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    for (id, name) in names.iter().enumerate().filter(|(_, name)| !name.is_empty()) {
        let kind = KindId(id as u16);
        match registry.by_name.get(name) {
            Some(&existing) if existing == kind => continue,
            Some(_) => return Err(KindConflict(name.clone())),
            None => {}
        }
        if registry.infos.get(id).is_some_and(Option::is_some) {
            return Err(KindConflict(name.clone()));
        }
        if registry.infos.len() <= id {
            registry.infos.resize(id + 1, None);
        }
        registry.by_name.insert(name.clone(), kind);
    }
    Ok(())
}

fn read_registry() -> std::sync::RwLockReadGuard<'static, Registry> {
    REGISTRY.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The first store kind of parts, above every key kind's.
const FIRST_PART_STORE_KIND: u16 = 2 + 2 * MAX_KINDS as u16;

/// Part types' names, by index; a part type's store kind follows from it.
static PARTS: LazyLock<RwLock<Vec<String>>> = LazyLock::new(Default::default);

/// The store kind of part type `T`, registering its name if needed.
pub(crate) fn part_store_kind<T: crate::Part>() -> Kind {
    let index = |parts: &[String]| parts.iter().position(|name| name == T::KIND);
    if let Some(index) = index(&PARTS.read().unwrap_or_else(|poisoned| poisoned.into_inner())) {
        return Kind(FIRST_PART_STORE_KIND + index as u16);
    }
    let mut parts = PARTS.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    let index = match index(&parts) {
        Some(index) => index,
        None => {
            parts.push(T::KIND.to_string());
            parts.len() - 1
        }
    };
    Kind(FIRST_PART_STORE_KIND + index as u16)
}

/// Every part type's name and store kind.
pub(crate) fn part_kinds() -> Vec<(String, Kind)> {
    let parts = PARTS.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    parts.iter().enumerate().map(|(index, name)| (name.clone(), Kind(FIRST_PART_STORE_KIND + index as u16))).collect()
}

/// Every part type's name, by index.
pub(crate) fn part_names() -> Vec<String> {
    PARTS.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

/// Give part type names the indexes they had when an engine was saved.
pub(crate) fn assign_parts(names: &[String]) -> Result<(), KindConflict> {
    let mut parts = PARTS.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    for (index, name) in names.iter().enumerate() {
        match parts.get(index) {
            Some(existing) if existing == name => {}
            Some(_) => return Err(KindConflict(name.clone())),
            None if parts.iter().any(|existing| existing == name) => return Err(KindConflict(name.clone())),
            None => parts.push(name.clone()),
        }
    }
    Ok(())
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
