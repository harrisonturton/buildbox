//! The named fields of struct-like values, such as provider instances.
//!
//! Values made in the same place usually have the same field names, so each
//! distinct list of names is interned once, and a value holds only that list
//! and its own values.

use allocative::Allocative;
use intern::Symbol;
use starlark::coerce::Coerce;
use starlark::values::{Freeze, Trace, Value, ValueLifetimeless};
use std::collections::HashSet;
use std::hash::BuildHasher;
use std::sync::{Arc, LazyLock, Mutex};

const SHARDS: usize = 16;

/// Every list of field names in use, never freed: there are few distinct
/// lists, however many values have them.
static NAME_LISTS: LazyLock<[Mutex<HashSet<Arc<[Symbol]>>>; SHARDS]> = LazyLock::new(|| std::array::from_fn(|_| Mutex::new(HashSet::new())));

/// Field names, in the order they were given, and their values.
#[derive(Debug, Trace, Freeze, Allocative)]
#[repr(C)]
pub struct FieldsGen<V: ValueLifetimeless> {
    #[trace(static)]
    #[freeze(identity)]
    #[allocative(skip)]
    names: Arc<[Symbol]>,
    values: Box<[V]>,
}

/// Fields whose values are on a heap being evaluated.
pub type Fields<'v> = FieldsGen<Value<'v>>;

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<FieldsGen<To>> for FieldsGen<From> {}

impl<V: ValueLifetimeless> FieldsGen<V> {
    /// Fields from names and values. A name given twice keeps its first
    /// position and its last value.
    pub fn new<'n>(fields: impl IntoIterator<Item = (&'n str, V)>) -> FieldsGen<V> {
        let mut names: Vec<Symbol> = Vec::new();
        let mut values: Vec<V> = Vec::new();
        for (name, value) in fields {
            let name = Symbol::intern(name);
            match names.iter().position(|existing| *existing == name) {
                Some(index) => values[index] = value,
                None => {
                    names.push(name);
                    values.push(value);
                }
            }
        }
        FieldsGen {
            names: shared_names(names),
            values: values.into_boxed_slice(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&V> {
        let name = Symbol::get(name)?;
        let index = self.names.iter().position(|existing| *existing == name)?;
        self.values.get(index)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The names and values, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &V)> {
        self.names.iter().map(|name| name.as_str()).zip(self.values.iter())
    }

    /// The names, in order.
    pub fn keys(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.names.iter().map(|name| name.as_str())
    }
}

/// The interned copy of a list of names.
fn shared_names(names: Vec<Symbol>) -> Arc<[Symbol]> {
    let hash = rustc_hash::FxBuildHasher.hash_one(&names);
    let shard = &NAME_LISTS[(hash as usize) % SHARDS];
    let mut lists = shard.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(existing) = lists.get(names.as_slice()) {
        return existing.clone();
    }
    let shared: Arc<[Symbol]> = names.into();
    lists.insert(shared.clone());
    shared
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::values::FrozenValue;

    fn none() -> FrozenValue {
        FrozenValue::new_none()
    }

    #[test]
    fn equal_name_lists_are_shared() {
        let a = FieldsGen::new([("files", none()), ("runfiles", none())]);
        let b = FieldsGen::new([("files", none()), ("runfiles", none())]);
        assert!(Arc::ptr_eq(&a.names, &b.names));
    }

    #[test]
    fn repeated_names_keep_the_last_value() {
        let fields = FieldsGen::new([("a", none()), ("b", none()), ("a", none())]);
        assert_eq!(fields.keys().collect::<Vec<_>>(), ["a", "b"]);
        assert!(fields.get("b").is_some());
        assert!(fields.get("c").is_none());
    }
}
