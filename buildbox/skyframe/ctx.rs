//! What a key's function uses to read other keys.

use crate::graph::GraphInner;
use crate::key::Key;
use crate::keys::KeyId;
use crate::trace::Dep;
use crate::SkyError;
use std::any::Any;
use std::sync::{Arc, Mutex, MutexGuard};

/// Passed to [`Key::compute`]: reads the values of other keys, recording
/// them as dependencies, and the graph's services.
///
/// Each call to [`Ctx::compute`] or [`Ctx::compute_many`] records a group of
/// dependencies. Keys that don't depend on each other's values should be
/// asked for in one group, so they are computed in parallel and checked in
/// parallel later.
#[derive(Clone)]
pub struct Ctx(Arc<CtxInner>);

struct CtxInner {
    graph: Arc<GraphInner>,
    key: KeyId,
    revision: u32,
    recorded: Mutex<Recorded>,
}

/// The dependencies a computation read.
#[derive(Default)]
pub(crate) struct Recorded {
    pub deps: Vec<Dep>,
    pub group_ends: Vec<u32>,
    /// Whether a dependency's result was transient, so this one is too.
    pub transient: bool,
}

impl Ctx {
    pub(crate) fn new(graph: Arc<GraphInner>, key: KeyId, revision: u32) -> Ctx {
        Ctx(Arc::new(CtxInner {
            graph,
            key,
            revision,
            recorded: Mutex::new(Recorded::default()),
        }))
    }

    /// The dependencies recorded so far, leaving none.
    pub(crate) fn finish(&self) -> Recorded {
        std::mem::take(&mut *self.lock())
    }

    /// The value of `key`, or the error computing it.
    pub async fn compute<K: Key>(&self, key: &K) -> Result<Arc<K::Value>, SkyError> {
        let mut results = self.compute_many(std::slice::from_ref(key)).await;
        results.pop().unwrap_or_else(|| Err(SkyError::new("no result for a requested key")))
    }

    /// The values of `keys`, computed in parallel.
    pub async fn compute_many<K: Key>(&self, keys: &[K]) -> Vec<Result<Arc<K::Value>, SkyError>> {
        if keys.is_empty() {
            return Vec::new();
        }
        let graph = &self.0.graph;
        let ids: Vec<KeyId> = keys.iter().map(|key| graph.engine.0.keys.intern(key)).collect();
        let requests = ids.iter().map(|&id| graph.clone().request(Some(self.0.key), id, self.0.revision));
        let outcomes = futures::future::join_all(requests).await;

        {
            let mut recorded = self.lock();
            for (&key, outcome) in ids.iter().zip(&outcomes) {
                match outcome {
                    Ok(outcome) => {
                        recorded.deps.push(Dep { key, value: outcome.value });
                        recorded.transient |= outcome.is_transient();
                    }
                    Err(_) => recorded.transient = true,
                }
            }
            let end = recorded.deps.len() as u32;
            recorded.group_ends.push(end);
        }

        outcomes
            .into_iter()
            .map(|outcome| outcome.and_then(|outcome| graph.read::<K>(outcome)))
            .collect()
    }

    /// The graph's service of type `T`.
    pub fn global<T: Any + Send + Sync>(&self) -> Result<Arc<T>, SkyError> {
        self.0
            .graph
            .globals
            .get::<T>()
            .ok_or_else(|| SkyError::new(format!("the graph has no {}", std::any::type_name::<T>())))
    }

    fn lock(&self) -> MutexGuard<'_, Recorded> {
        self.0.recorded.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
