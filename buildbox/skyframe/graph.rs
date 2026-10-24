//! One workspace's view of the graph, and evaluating keys in it.

use crate::ctx::{Ctx, Recorded};
use crate::engine::{Engine, Event};
use crate::key::Key;
use crate::keys::KeyId;
use crate::kinds::{self, KindId};
use crate::trace::{Trace, ERROR, TRANSIENT};
use crate::SkyError;
use futures::future::BoxFuture;
use futures::FutureExt;
use rustc_hash::{FxHashMap, FxHashSet};
use skystore::ValueRef;
use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;
use tokio::sync::{Notify, OwnedRwLockReadGuard, RwLock};
use tracing::Instrument;

/// A node must be computed again, even if its dependencies are unchanged.
const DIRTY: u8 = 4;

/// One workspace's graph: the result each key has in that workspace, and
/// the revision it was last checked at.
///
/// A graph's values are only reused within an evaluation [`Session`] at the
/// revision they were checked at. When something outside the graph changes,
/// [`Graph::invalidate`] starts a new revision; each key asked for after that
/// is checked again, from its dependencies up. A key whose dependencies'
/// values are unchanged keeps its value without being computed again
/// (change pruning), and a key computed again to the same value doesn't
/// cause the keys that depend on it to be computed again.
///
/// Many sessions can evaluate a graph at once, and many keys within each
/// session: a key asked for while it is being computed waits for that
/// computation rather than starting another.
#[derive(Clone)]
pub struct Graph(Arc<GraphInner>);

pub(crate) struct GraphInner {
    pub engine: Engine,
    pub globals: Globals,
    state: Mutex<GraphState>,
    revision: AtomicU32,
    sessions: Arc<RwLock<()>>,
}

#[derive(Default)]
struct GraphState {
    nodes: FxHashMap<u32, Node>,
    in_flight: FxHashMap<u32, Arc<InFlight>>,
    /// Which in-flight keys each computing key is waiting for, to find cycles.
    waiting_on: FxHashMap<u32, Vec<u32>>,
}

/// A key's result in this workspace: 16 bytes.
#[derive(Clone, Copy)]
struct Node {
    /// How the result was computed. The node holds a reference to it, and
    /// through it to the value.
    trace: ValueRef,
    value: ValueRef,
    verified_at: u32,
    flags: u8,
}

/// A key's result, as seen by the keys that depend on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub value: ValueRef,
    pub flags: u8,
}

impl Outcome {
    pub(crate) fn is_error(self) -> bool {
        self.flags & ERROR != 0
    }

    pub(crate) fn is_transient(self) -> bool {
        self.flags & TRANSIENT != 0
    }
}

#[derive(Default)]
struct InFlight {
    outcome: OnceLock<Result<Outcome, SkyError>>,
    done: Notify,
}

impl InFlight {
    /// End the computation's waits with `error`, as its result.
    fn fail(&self, error: SkyError) {
        let _ = self.outcome.set(Err(error));
        self.done.notify_waiters();
    }

    async fn wait(&self) -> Result<Outcome, SkyError> {
        loop {
            let notified = self.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(outcome) = self.outcome.get() {
                return outcome.clone();
            }
            notified.await;
        }
    }
}

/// How a node's evaluation ended.
enum Resolution {
    /// The node's current result is still valid.
    Kept,
    /// The node has a new result, with the given trace, which the
    /// evaluation holds a reference to.
    New(ValueRef),
}

/// The services a graph's key functions use, by type.
#[derive(Clone, Default)]
pub struct Globals {
    values: Arc<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl Globals {
    pub fn new() -> Globals {
        Globals::default()
    }

    /// Add a service, replacing any of the same type.
    pub fn with<T: Any + Send + Sync>(mut self, value: Arc<T>) -> Globals {
        Arc::make_mut(&mut self.values).insert(TypeId::of::<T>(), value);
        self
    }

    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        let value = self.values.get(&TypeId::of::<T>())?.clone();
        value.downcast::<T>().ok()
    }
}

/// How much a graph holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GraphStats {
    pub nodes: u64,
    /// The bytes the node table takes.
    pub node_bytes: u64,
    pub revision: u32,
}

/// Evaluates keys at one revision of a graph. The graph can't be
/// invalidated while a session is open.
pub struct Session {
    graph: Arc<GraphInner>,
    revision: u32,
    _guard: OwnedRwLockReadGuard<()>,
}

impl Graph {
    pub(crate) fn new(engine: Engine, globals: Globals) -> Graph {
        Graph(Arc::new(GraphInner {
            engine,
            globals,
            state: Mutex::new(GraphState::default()),
            revision: AtomicU32::new(1),
            sessions: Arc::new(RwLock::new(())),
        }))
    }

    /// Start evaluating at the current revision, waiting for any
    /// invalidation in progress.
    pub async fn session(&self) -> Session {
        let guard = self.0.sessions.clone().read_owned().await;
        Session {
            graph: self.0.clone(),
            revision: self.0.revision.load(Ordering::Acquire),
            _guard: guard,
        }
    }

    /// Mark keys as needing to be computed again, because something they
    /// read outside the graph has changed, and start a new revision. Waits
    /// for open sessions to finish.
    pub async fn invalidate<K: Key>(&self, keys: &[K]) {
        if keys.is_empty() {
            return;
        }
        let _guard = self.0.sessions.write().await;
        let engine = &self.0.engine.0;
        let mut state = self.0.lock();
        for key in keys {
            let Some(id) = engine.keys.get(key) else {
                continue;
            };
            if let Some(node) = state.nodes.get_mut(&id.0) {
                node.flags |= DIRTY;
            }
        }
        self.0.revision.fetch_add(1, Ordering::AcqRel);
    }

    /// Every key of type `K` with a result in this graph, for finding which
    /// external keys have changed.
    pub fn keys<K: Key>(&self) -> Vec<K> {
        let kind = kinds::kind_of::<K>();
        let engine = &self.0.engine.0;
        let ids: Vec<u32> = {
            let state = self.0.lock();
            state.nodes.keys().copied().collect()
        };
        ids.into_iter()
            .filter(|&id| engine.keys.kind(KeyId(id)) == kind)
            .filter_map(|id| skycodec::from_bytes::<K>(engine.keys.encoding(KeyId(id))).ok())
            .collect()
    }

    pub fn stats(&self) -> GraphStats {
        let state = self.0.lock();
        let node_bytes = state.nodes.capacity() * (std::mem::size_of::<(u32, Node)>() + 1);
        GraphStats {
            nodes: state.nodes.len() as u64,
            node_bytes: node_bytes as u64,
            revision: self.0.revision.load(Ordering::Acquire),
        }
    }

    pub fn engine(&self) -> &Engine {
        &self.0.engine
    }
}

impl Session {
    pub fn revision(&self) -> u32 {
        self.revision
    }

    /// The value of `key` at this session's revision.
    pub async fn evaluate<K: Key>(&self, key: &K) -> Result<Arc<K::Value>, SkyError> {
        let span = tracing::info_span!("skyframe.evaluate", span.name = %format!("evaluate {}", K::KIND), span.nested = true);
        async {
            let id = self.graph.engine.0.keys.intern(key);
            let outcome = self.graph.clone().request(None, id, self.revision).await?;
            self.graph.read::<K>(outcome)
        }
        .instrument(span)
        .await
    }

    /// The values of `keys`, evaluated concurrently.
    pub async fn evaluate_many<K: Key>(&self, keys: &[K]) -> Vec<Result<Arc<K::Value>, SkyError>> {
        let requests = keys.iter().map(|key| self.evaluate(key));
        futures::future::join_all(requests).await
    }
}

impl GraphInner {
    fn lock(&self) -> MutexGuard<'_, GraphState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Decode a key's typed value, or its error.
    pub(crate) fn read<K: Key>(&self, outcome: Outcome) -> Result<Arc<K::Value>, SkyError> {
        if outcome.is_error() {
            return Err(self.engine.0.read_error(outcome.value));
        }
        self.engine.0.read_value::<K>(outcome.value)
    }

    /// The result of `key` at `revision`, evaluating it if needed. Fails
    /// if `key` is, or depends on, a key in a cycle.
    ///
    /// Boxed, since evaluation recurses through it.
    pub(crate) fn request(self: Arc<Self>, requester: Option<KeyId>, key: KeyId, revision: u32) -> BoxFuture<'static, Result<Outcome, SkyError>> {
        Box::pin(async move { self.request_unboxed(requester, key, revision).await })
    }

    async fn request_unboxed(self: Arc<Self>, requester: Option<KeyId>, key: KeyId, revision: u32) -> Result<Outcome, SkyError> {
        let (in_flight, started, cycle) = {
            let mut state = self.lock();
            if let Some(node) = state.nodes.get(&key.0) {
                if node.verified_at == revision && node.flags & DIRTY == 0 {
                    return Ok(Outcome {
                        value: node.value,
                        flags: node.flags,
                    });
                }
            }
            let existing = state.in_flight.get(&key.0).cloned();
            if let Some(requester) = requester {
                state.waiting_on.entry(requester.0).or_default().push(key.0);
            }
            match existing {
                Some(in_flight) => (in_flight, false, state.stalled_cycle()),
                None => {
                    let in_flight = Arc::new(InFlight::default());
                    state.in_flight.insert(key.0, in_flight.clone());
                    (in_flight, true, None)
                }
            }
        };

        if let Some((path, broken)) = cycle {
            self.break_cycle(path, &broken);
        }
        if started {
            tokio::spawn(self.clone().evaluate(key, revision, in_flight.clone()).in_current_span());
        }
        let outcome = in_flight.wait().await;
        if let Some(requester) = requester {
            self.lock().stop_waiting(requester.0, key.0);
        }
        outcome
    }

    /// Evaluate `key`, which this task has claimed, and publish its result.
    async fn evaluate(self: Arc<Self>, key: KeyId, revision: u32, in_flight: Arc<InFlight>) {
        let evaluation = AssertUnwindSafe(self.clone().resolve(key, revision)).catch_unwind();
        let resolution = match evaluation.await {
            Ok(resolution) => resolution,
            Err(panic) => Err(SkyError::new(format!(
                "computing {} panicked: {}",
                self.engine.0.keys.describe(key),
                panic_message(&*panic)
            ))),
        };
        let outcome = self.finish(key, revision, resolution);
        let _ = in_flight.outcome.set(outcome);
        in_flight.done.notify_waiters();
    }

    /// Record a resolved node and stop tracking it as in flight.
    fn finish(&self, key: KeyId, revision: u32, resolution: Result<Resolution, SkyError>) -> Result<Outcome, SkyError> {
        let engine = &self.engine.0;
        // Read a new trace before taking the lock, which every request takes.
        let new = match resolution {
            Err(error) => Err(error),
            Ok(Resolution::Kept) => Ok(None),
            Ok(Resolution::New(trace)) => Ok(Some((trace, engine.trace(trace)))),
        };
        let (outcome, released, cycle) = {
            let mut state = self.lock();
            state.in_flight.remove(&key.0);
            state.waiting_on.remove(&key.0);
            let (outcome, released) = match new {
                Err(error) => (Err(error), None),
                Ok(None) => (state.keep(key, revision), None),
                Ok(Some((trace, decoded))) => state.record(key, revision, trace, decoded),
            };
            (outcome, released, state.stalled_cycle())
        };
        if let Some(old) = released {
            engine.release_trace(old);
        }
        if let Some((path, broken)) = cycle {
            self.break_cycle(path, &broken);
        }
        outcome
    }

    /// Fail the waits for a computation on a cycle, so that the keys on it
    /// fail rather than wait forever.
    fn break_cycle(&self, path: Vec<u32>, broken: &InFlight) {
        let names: Vec<String> = path.into_iter().map(|id| self.engine.0.keys.describe(KeyId(id))).collect();
        broken.fail(SkyError::cycle(&names));
    }

    /// Decide `key`'s result at `revision`: keep its current one if its
    /// dependencies are unchanged, reuse a remembered trace whose
    /// dependencies match, or compute it.
    async fn resolve(self: Arc<Self>, key: KeyId, revision: u32) -> Result<Resolution, SkyError> {
        let engine = self.engine.clone();
        let kind = engine.0.keys.kind(key);
        let Some(info) = kinds::info(kind) else {
            return Err(SkyError::transient(format!("{} is of a kind not registered yet", engine.0.keys.describe(key))));
        };
        let current = self.lock().nodes.get(&key.0).copied();

        if let Some(node) = current {
            if node.flags & (DIRTY | TRANSIENT) == 0 && self.clone().verify(key, node.trace, revision).await {
                engine.0.record(kind, Event::Verified);
                return Ok(Resolution::Kept);
            }
        }

        if !info.external {
            if let Some(trace) = self.clone().reuse(key, current.map(|node| node.trace), revision).await {
                engine.0.record(kind, Event::Reused);
                return Ok(Resolution::New(trace));
            }
        }

        self.compute(key, kind, info, revision).await.map(Resolution::New)
    }

    /// Find a remembered trace for `key`, other than `current`, whose
    /// dependencies have the same values here. Returns it with a reference
    /// the caller owns.
    async fn reuse(self: Arc<Self>, key: KeyId, current: Option<ValueRef>, revision: u32) -> Option<ValueRef> {
        let engine = self.engine.clone();
        let candidates = engine.0.memo_candidates(key.0);
        let mut found = None;
        for candidate in candidates {
            if found.is_none() && Some(candidate) != current && !engine.0.has_absent_value(candidate) && self.clone().verify(key, candidate, revision).await {
                found = Some(candidate);
                continue;
            }
            engine.0.release_trace(candidate);
        }
        found
    }

    /// Whether every dependency in `trace` still has the value it recorded,
    /// checking group by group.
    async fn verify(self: Arc<Self>, key: KeyId, trace: ValueRef, revision: u32) -> bool {
        let Ok(trace) = self.engine.0.trace(trace) else {
            return false;
        };
        for group in trace.groups() {
            let requests = group.iter().map(|dep| self.clone().request(Some(key), dep.key, revision));
            let outcomes = futures::future::join_all(requests).await;
            for (dep, outcome) in group.iter().zip(outcomes) {
                match outcome {
                    Ok(outcome) if outcome.value == dep.value && !outcome.is_transient() => {}
                    _ => return false,
                }
            }
        }
        true
    }

    /// Run `key`'s function, and store its result and trace. Returns the
    /// trace with a reference the caller owns.
    async fn compute(self: Arc<Self>, key: KeyId, kind: KindId, info: &'static kinds::KindInfo, revision: u32) -> Result<ValueRef, SkyError> {
        let engine = self.engine.clone();
        let span = tracing::debug_span!("skyframe.compute", span.name = info.name, span.nested = true);
        let ctx = Ctx::new(self.clone(), key, revision);
        let started = Instant::now();
        let computed = (info.compute)(ctx.clone(), engine.0.keys.encoding(key)).instrument(span).await;
        engine.0.record(kind, Event::Computed(started.elapsed()));

        let Recorded {
            deps,
            group_ends,
            transient,
        } = ctx.finish();
        let mut flags = if transient { TRANSIENT } else { 0 };
        let value = match computed {
            Ok(computed) => engine.0.store_computed(kind, computed),
            Err(error) => {
                flags |= ERROR;
                if !error.is_cacheable() {
                    flags |= TRANSIENT;
                }
                engine.0.store_error(&error)
            }
        };
        let trace = Trace {
            key,
            flags,
            value,
            deps,
            group_ends,
        };
        let trace = engine.0.insert_trace(kind, trace);
        if !info.external && flags & TRANSIENT == 0 {
            engine.0.memo_add(key.0, trace);
        }
        Ok(trace)
    }
}

impl Drop for GraphInner {
    fn drop(&mut self) {
        let state = std::mem::take(&mut *self.lock());
        for node in state.nodes.values() {
            self.engine.0.release_trace(node.trace);
        }
    }
}

impl Node {
    fn outcome(&self) -> Outcome {
        Outcome {
            value: self.value,
            flags: self.flags & (ERROR | TRANSIENT),
        }
    }
}

impl GraphState {
    /// Keep a node's result, as checked at `revision`.
    fn keep(&mut self, key: KeyId, revision: u32) -> Result<Outcome, SkyError> {
        let Some(node) = self.nodes.get_mut(&key.0) else {
            return Err(SkyError::new("a node being kept has been removed"));
        };
        node.verified_at = revision;
        Ok(node.outcome())
    }

    /// Give a node a new result, from `trace`, returning the trace it had,
    /// which the node no longer holds.
    fn record(&mut self, key: KeyId, revision: u32, trace: ValueRef, decoded: Result<Arc<Trace>, SkyError>) -> (Result<Outcome, SkyError>, Option<ValueRef>) {
        let (value, flags) = match &decoded {
            Ok(decoded) => (decoded.value, decoded.flags),
            Err(_) => (trace, ERROR | TRANSIENT),
        };
        let node = Node {
            trace,
            value,
            verified_at: revision,
            flags,
        };
        let released = self.nodes.insert(key.0, node).map(|old| old.trace);
        (decoded.map(|_| node.outcome()), released)
    }

    /// A cycle of waits, if every key being computed is waiting for another,
    /// so that none can finish: the keys on it, from one back to itself,
    /// and the in-flight computation of the second, whose waiters are told
    /// of the cycle. Waits are only searched for cycles when nothing can
    /// make progress, so that most requests don't search at all.
    fn stalled_cycle(&self) -> Option<(Vec<u32>, Arc<InFlight>)> {
        if self.waiting_on.len() < self.in_flight.len() {
            return None;
        }
        let path = self.find_cycle()?;
        let broken = self.in_flight.get(&path[1])?.clone();
        Some((path, broken))
    }

    /// A cycle in the waits, found depth first: the keys on it, starting and
    /// ending with the same key, each waiting for the next.
    fn find_cycle(&self) -> Option<Vec<u32>> {
        let mut finished: FxHashSet<u32> = FxHashSet::default();
        for &start in self.waiting_on.keys() {
            if finished.contains(&start) {
                continue;
            }
            // The path from `start`, and how many of each key's waits have
            // been followed.
            let mut path = vec![start];
            let mut followed = vec![0];
            while let (Some(&current), Some(index)) = (path.last(), followed.last_mut()) {
                let waits = self.waiting_on.get(&current).map(Vec::as_slice).unwrap_or_default();
                let Some(&next) = waits.get(*index) else {
                    finished.insert(current);
                    path.pop();
                    followed.pop();
                    continue;
                };
                *index += 1;
                if let Some(position) = path.iter().position(|&key| key == next) {
                    let mut cycle = path.split_off(position);
                    cycle.push(next);
                    return Some(cycle);
                }
                if !finished.contains(&next) {
                    path.push(next);
                    followed.push(0);
                }
            }
        }
        None
    }

    fn stop_waiting(&mut self, waiter: u32, key: u32) {
        let Some(keys) = self.waiting_on.get_mut(&waiter) else {
            return;
        };
        if let Some(index) = keys.iter().position(|&k| k == key) {
            keys.swap_remove(index);
        }
        if keys.is_empty() {
            self.waiting_on.remove(&waiter);
        }
    }
}

fn panic_message(panic: &(dyn Any + Send)) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        return message.to_string();
    }
    if let Some(message) = panic.downcast_ref::<String>() {
        return message.clone();
    }
    "unknown panic".to_string()
}
