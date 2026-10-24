//! The state every workspace's graph shares.

use crate::cache::{CacheStats, ValueCache};
use crate::graph::{Globals, Graph};
use crate::key::{Key, Storage};
use crate::keys::KeyTable;
use crate::persist::{self, Persistence};
use crate::kinds::{self, Computed, KindId, ERROR_STORE_KIND, MAX_KINDS};
use crate::trace::Trace;
use crate::SkyError;
use rustc_hash::FxHashMap;
use skycodec::{Fingerprint, Fingerprinter};
use skystore::{KindStats, StoreStats, Stored, ValueRef, ValueStore};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

const MEMO_SHARDS: usize = 64;

/// How an [`Engine`] uses memory.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// How many bytes of decoded values to keep cached.
    pub cache_bytes: usize,
    /// How many traces to remember for each key, for workspaces to reuse.
    /// Keeping more lets more workspaces at different revisions share work,
    /// at the cost of memory and of checking more traces.
    pub traces_per_key: usize,
}

impl Default for EngineConfig {
    fn default() -> EngineConfig {
        EngineConfig {
            cache_bytes: 16 << 20,
            traces_per_key: 4,
        }
    }
}

/// The values and traces that every workspace's [`Graph`] shares.
///
/// An engine holds each distinct value once, compressed, however many
/// workspaces computed it, and remembers recent [traces](crate::trace) of how
/// each key was computed. A workspace asking for a key first checks whether
/// any remembered trace's dependencies have the values they have in that
/// workspace; if one does, it reuses the trace's value without computing it.
#[derive(Clone)]
pub struct Engine(pub(crate) Arc<EngineInner>);

pub(crate) struct EngineInner {
    pub store: ValueStore,
    pub keys: KeyTable,
    pub cache: ValueCache,
    memo: Box<[Mutex<FxHashMap<u32, Vec<ValueRef>>>]>,
    counters: Box<[KindCounters]>,
    config: EngineConfig,
    pub persistence: Option<Persistence>,
    /// Held while checkpointing, so checkpoints run one at a time, with
    /// how many changes the memo had at the last one.
    pub checkpointing: Mutex<u64>,
    /// How many times the memo has changed.
    pub memo_changes: AtomicU64,
}

#[derive(Default)]
struct KindCounters {
    computed: AtomicU64,
    verified: AtomicU64,
    reused: AtomicU64,
    compute_nanos: AtomicU64,
}

/// What an engine holds, and the work done for each kind of key.
#[derive(Clone, Debug, Default)]
pub struct EngineStats {
    pub keys: u64,
    pub key_bytes: u64,
    pub memo_keys: u64,
    pub memo_traces: u64,
    pub store: StoreStats,
    pub cache: CacheStats,
    pub kinds: Vec<KindRow>,
}

/// Statistics for one kind of key.
#[derive(Clone, Debug, Default)]
pub struct KindRow {
    pub name: String,
    /// Values computed by running the key's function.
    pub computed: u64,
    /// Values found still valid after their dependencies were checked.
    pub verified: u64,
    /// Values reused from a trace another computation left.
    pub reused: u64,
    /// Time spent computing, including waiting for dependencies.
    pub compute_time: Duration,
    pub values: KindStats,
    pub traces: KindStats,
}

/// What a kind of computation did, for [`EngineStats`].
#[derive(Clone, Copy)]
pub(crate) enum Event {
    Computed(Duration),
    Verified,
    Reused,
}

impl Engine {
    pub fn new(config: EngineConfig) -> Engine {
        Engine::with_parts(config, None, KeyTable::default(), ValueStore::new(), Vec::new())
    }

    /// The engine saved under `persistence`, or a new one if there's none
    /// this build can use; it's saved there by [`Engine::checkpoint`]. Call
    /// before anything is interned or any key type registered, so that the
    /// saved ids are free to restore.
    pub fn open(config: EngineConfig, persistence: Persistence) -> Engine {
        match persist::restore(&persistence) {
            Ok(Some(restored)) => {
                tracing::info!("restored {} keys and {} remembered keys from {}", restored.keys.len(), restored.memo.len(), persistence.dir.display());
                return Engine::with_parts(config, Some(persistence), restored.keys, restored.store, restored.memo);
            }
            Ok(None) => {}
            Err(error) => tracing::warn!("not restoring the saved engine: {error}"),
        }
        if let Err(error) = persist::clear(&persistence) {
            tracing::warn!("couldn't remove the old saved engine: {error}");
        }
        Engine::with_parts(config, Some(persistence), KeyTable::default(), ValueStore::new(), Vec::new())
    }

    fn with_parts(config: EngineConfig, persistence: Option<Persistence>, keys: KeyTable, store: ValueStore, memo: Vec<(u32, Vec<ValueRef>)>) -> Engine {
        let shards: Box<[Mutex<FxHashMap<u32, Vec<ValueRef>>>]> = (0..MEMO_SHARDS).map(|_| Mutex::new(FxHashMap::default())).collect();
        for (key, traces) in memo {
            let mut shard = shards[key as usize % MEMO_SHARDS].lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            shard.insert(key, traces);
        }
        Engine(Arc::new(EngineInner {
            store,
            keys,
            cache: ValueCache::new(config.cache_bytes),
            memo: shards,
            counters: (0..MAX_KINDS).map(|_| KindCounters::default()).collect(),
            config,
            persistence,
            checkpointing: Mutex::new(0),
            memo_changes: AtomicU64::new(0),
        }))
    }

    /// A new, empty graph for one workspace. `globals` are the services its
    /// keys' functions use, such as the workspace's filesystem.
    pub fn graph(&self, globals: Globals) -> Graph {
        Graph::new(self.clone(), globals)
    }

    pub fn stats(&self) -> EngineStats {
        self.0.stats()
    }
}

impl Default for Engine {
    fn default() -> Engine {
        Engine::new(EngineConfig::default())
    }
}

impl EngineInner {
    /// Store a computed value, returning a reference the caller owns.
    pub(crate) fn store_computed(&self, kind: KindId, computed: Computed) -> ValueRef {
        match computed.storage {
            Storage::Encoded(bytes) => {
                let fingerprint = value_fingerprint(kind, b"encoded", &bytes);
                let value = self.store.insert(kind.value_store_kind(), fingerprint, &bytes);
                self.cache.insert(value.id(), computed.value, decoded_size(bytes.len()));
                value
            }
            Storage::Live { fingerprint, size } => {
                let fingerprint = value_fingerprint(kind, b"live", fingerprint.as_bytes());
                self.store.insert_live(kind.value_store_kind(), fingerprint, computed.value, size)
            }
        }
    }

    /// Store an error as a key's result, returning a reference the caller
    /// owns.
    pub(crate) fn store_error(&self, error: &SkyError) -> ValueRef {
        let bytes = skycodec::to_bytes(error);
        let fingerprint = Fingerprint::of(&bytes);
        self.store.insert(ERROR_STORE_KIND, fingerprint, &bytes)
    }

    pub(crate) fn read_value<K: Key>(&self, value: ValueRef) -> Result<Arc<K::Value>, SkyError> {
        if let Some(decoded) = self.cache.get(value.id()) {
            return downcast::<K>(decoded);
        }
        let stored = self.store.read(value).map_err(|error| SkyError::new(error.to_string()))?;
        match stored {
            Stored::Live(live) => downcast::<K>(live),
            Stored::Bytes(bytes) => {
                let decoded = Arc::new(kinds::decode_value::<K>(&bytes)?);
                self.cache.insert(value.id(), decoded.clone(), decoded_size(bytes.len()));
                Ok(decoded)
            }
        }
    }

    pub(crate) fn read_error(&self, value: ValueRef) -> SkyError {
        let Ok(Stored::Bytes(bytes)) = self.store.read(value) else {
            return SkyError::new("stored error is missing");
        };
        skycodec::from_bytes(&bytes).unwrap_or_else(|error| SkyError::new(format!("corrupt stored error: {error}")))
    }

    /// Store a trace, taking over the caller's reference to its value, and
    /// return a reference to the trace the caller owns.
    pub(crate) fn insert_trace(&self, kind: KindId, trace: Trace) -> ValueRef {
        let bytes = skycodec::to_bytes(&trace);
        let fingerprint = Fingerprint::of(&bytes);
        let inserted = self.store.insert_new(kind.trace_store_kind(), fingerprint, &bytes);
        if !inserted.is_new {
            self.release_value(trace.value);
            return inserted.value;
        }
        for dep in &trace.deps {
            self.store.retain(dep.value);
        }
        self.cache.insert(inserted.value.id(), Arc::new(trace), decoded_size(bytes.len()));
        inserted.value
    }

    pub(crate) fn trace(&self, trace: ValueRef) -> Result<Arc<Trace>, SkyError> {
        if let Some(decoded) = self.cache.get(trace.id()) {
            return decoded.downcast::<Trace>().map_err(|_| SkyError::new("cached trace has the wrong type"));
        }
        let Ok(Stored::Bytes(bytes)) = self.store.read(trace) else {
            return Err(SkyError::new("stored trace is missing"));
        };
        let decoded = skycodec::from_bytes::<Trace>(&bytes).map_err(|error| SkyError::new(format!("corrupt trace: {error}")))?;
        let decoded = Arc::new(decoded);
        self.cache.insert(trace.id(), decoded.clone(), decoded_size(bytes.len()));
        Ok(decoded)
    }

    /// Release a reference to a trace, and the references it holds if it was
    /// the last.
    pub(crate) fn release_trace(&self, trace: ValueRef) {
        let Ok(Some(Stored::Bytes(bytes))) = self.store.release_and_take(trace) else {
            return;
        };
        self.cache.remove(trace.id());
        let Ok(decoded) = skycodec::from_bytes::<Trace>(&bytes) else {
            tracing::error!("freed a corrupt trace; its references are leaked");
            return;
        };
        self.release_value(decoded.value);
        for dep in &decoded.deps {
            self.release_value(dep.value);
        }
    }

    /// Whether a trace's value is a live object a restored engine doesn't
    /// have, which must be computed again.
    pub(crate) fn has_absent_value(&self, trace: ValueRef) -> bool {
        self.trace(trace).is_ok_and(|trace| self.store.is_absent(trace.value))
    }

    pub(crate) fn release_value(&self, value: ValueRef) {
        if self.store.release(value) {
            self.cache.remove(value.id());
        }
    }

    /// Every key's remembered traces, newest first.
    pub(crate) fn memo_snapshot(&self) -> Vec<(u32, Vec<ValueRef>)> {
        let mut snapshot = Vec::new();
        for shard in self.memo.iter() {
            let shard = shard.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            snapshot.extend(shard.iter().map(|(key, traces)| (*key, traces.clone())));
        }
        snapshot.sort_unstable_by_key(|(key, _)| *key);
        snapshot
    }

    /// The traces remembered for `key`, newest first, each with a reference
    /// the caller owns.
    pub(crate) fn memo_candidates(&self, key: u32) -> Vec<ValueRef> {
        let memo = self.memo_shard(key);
        let Some(traces) = memo.get(&key) else {
            return Vec::new();
        };
        for trace in traces {
            self.store.retain(*trace);
        }
        traces.clone()
    }

    /// Remember a trace for `key`, forgetting the oldest if there are too
    /// many.
    pub(crate) fn memo_add(&self, key: u32, trace: ValueRef) {
        let evicted = {
            let mut memo = self.memo_shard(key);
            let traces = memo.entry(key).or_default();
            if traces.contains(&trace) {
                return;
            }
            self.store.retain(trace);
            traces.insert(0, trace);
            self.memo_changes.fetch_add(1, Ordering::Relaxed);
            if traces.len() > self.config.traces_per_key {
                traces.pop()
            } else {
                None
            }
        };
        if let Some(evicted) = evicted {
            self.release_trace(evicted);
        }
    }

    pub(crate) fn record(&self, kind: KindId, event: Event) {
        let counters = &self.counters[kind.0 as usize];
        match event {
            Event::Computed(time) => {
                counters.computed.fetch_add(1, Ordering::Relaxed);
                counters.compute_nanos.fetch_add(time.as_nanos() as u64, Ordering::Relaxed);
            }
            Event::Verified => {
                counters.verified.fetch_add(1, Ordering::Relaxed);
            }
            Event::Reused => {
                counters.reused.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn memo_shard(&self, key: u32) -> MutexGuard<'_, FxHashMap<u32, Vec<ValueRef>>> {
        let shard = &self.memo[key as usize % MEMO_SHARDS];
        shard.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn stats(&self) -> EngineStats {
        let store = self.store.stats();
        let mut stats = EngineStats {
            keys: self.keys.len() as u64,
            key_bytes: self.keys.allocated_bytes() as u64,
            cache: self.cache.stats(),
            ..EngineStats::default()
        };
        for shard in self.memo.iter() {
            let shard = shard.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            stats.memo_keys += shard.len() as u64;
            stats.memo_traces += shard.values().map(|traces| traces.len() as u64).sum::<u64>();
        }
        for (index, name) in kinds::names().into_iter().enumerate() {
            let Some(name) = name else {
                continue;
            };
            let name = name.to_string();
            let kind = KindId(index as u16);
            let counters = &self.counters[index];
            let store_kind = |kind: skystore::Kind| store.kinds.get(kind.0 as usize).copied().unwrap_or_default();
            stats.kinds.push(KindRow {
                name,
                computed: counters.computed.load(Ordering::Relaxed),
                verified: counters.verified.load(Ordering::Relaxed),
                reused: counters.reused.load(Ordering::Relaxed),
                compute_time: Duration::from_nanos(counters.compute_nanos.load(Ordering::Relaxed)),
                values: store_kind(kind.value_store_kind()),
                traces: store_kind(kind.trace_store_kind()),
            });
        }
        for (name, kind) in kinds::part_kinds() {
            stats.kinds.push(KindRow {
                name,
                values: store.kinds.get(kind.0 as usize).copied().unwrap_or_default(),
                ..KindRow::default()
            });
        }
        stats.store = store;
        stats
    }
}

fn value_fingerprint(kind: KindId, storage: &[u8], bytes: &[u8]) -> Fingerprint {
    let mut fingerprinter = Fingerprinter::new();
    let name = kinds::info(kind).map_or("", |info| info.name);
    fingerprinter.u64(kind.0 as u64).bytes(name.as_bytes()).bytes(storage).bytes(bytes);
    fingerprinter.finish()
}

/// Roughly how much memory a value takes decoded, given its encoded size.
fn decoded_size(encoded: usize) -> usize {
    64 + encoded * 3
}

fn downcast<K: Key>(decoded: Arc<dyn std::any::Any + Send + Sync>) -> Result<Arc<K::Value>, SkyError> {
    decoded
        .downcast::<K::Value>()
        .map_err(|_| SkyError::new(format!("stored {} value has the wrong type", K::KIND)))
}
