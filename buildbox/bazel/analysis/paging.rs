//! Paging analyzed targets' frozen heaps out of memory.
//!
//! An analyzed target's providers live in a frozen Starlark heap, which
//! refers to its dependencies' heaps and to the heaps of the `.bzl`
//! modules, builtins and globals it used. A [`HeapStore`] serializes
//! targets' heaps to files and reads them back when they're next used.
//! Heaps of other kinds, such as `.bzl` modules', are never serialized:
//! they're pinned, kept live, and referred to by a key that resolves back to
//! the live heap when a target is read.
//!
//! Targets are paged out together, since a target's heap is freed only once
//! no dependent's live heap refers to it.

use dashmap::DashMap;
use pagable::arc_erase::{ArcEraseDyn, WeakEraseDyn};
use pagable::context::PagableDeserializerImpl;
use pagable::storage::data::{DataKey, PagableData};
use pagable::storage::handle::PagableStorageHandle;
use pagable::storage::support::SerializerForPaging;
use pagable::storage::traits::{ArcSerSlot, DeserializedArcCache, PageOutError, PagableStorage};
use pagable::{PagableDeserialize, PagableSerialize, SessionContext};
use starlark::values::{FrozenHeapRef, OwnedFrozenValue};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// The store targets' heaps are paged to, set up by the server.
static HEAP_STORE: OnceLock<Arc<HeapStore>> = OnceLock::new();

/// Page targets' heaps to files in `dir`, which is emptied first.
pub fn set_up(dir: &Path) -> io::Result<()> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;
    let store = Arc::new(HeapStore::new(dir.to_path_buf()));
    let _ = HEAP_STORE.set(store);
    Ok(())
}

fn store() -> Option<&'static Arc<HeapStore>> {
    HEAP_STORE.get()
}

/// Serializes targets' heaps to files, and reads them back.
pub struct HeapStore {
    dir: PathBuf,
    session: SessionContext,
    arcs: DeserializedArcCache,
    state: Mutex<StoreState>,
    /// Held while paging out. Page-outs share the session's record of the
    /// heaps being serialized, which each forgets when it's done, so two
    /// at once would lose each other's.
    paging_out: Mutex<()>,
}

#[derive(Default)]
struct StoreState {
    /// Where each stored blob is: its file, offset and length.
    index: HashMap<DataKey, (u32, u64, u32)>,
    /// Blobs stored since the last flush, not yet in a file.
    pending: HashMap<DataKey, Arc<PagableData>>,
    files: Vec<File>,
    /// The key of every pinned heap, by its identity. Only pinned heaps,
    /// which are kept live, are known by identity: a target's heap may be
    /// freed and its address reused by another.
    known: HashMap<usize, DataKey>,
    /// The key each heap read back came from, by identity, while it's
    /// live, which the weak reference tells.
    read_back: HashMap<usize, ReadBack>,
    /// The heaps whose values those read back may point into, kept live,
    /// by identity: their values' addresses are registered for reading
    /// back.
    registered: HashMap<usize, FrozenHeapRef>,
    /// Pinned heaps, kept live, by key.
    pinned: HashMap<DataKey, Box<dyn ArcEraseDyn>>,
    /// The heaps of the targets being paged out now, which are stored
    /// rather than pinned.
    paging: HashSet<usize>,
    /// The keys read since the last value was read back, whose heaps are
    /// then found in the cache, for `read_back`.
    fetched: Vec<DataKey>,
    next_pin: u128,
}

impl HeapStore {
    fn new(dir: PathBuf) -> HeapStore {
        HeapStore {
            dir,
            session: SessionContext::new(),
            arcs: DeserializedArcCache::new(),
            state: Mutex::new(StoreState {
                next_pin: 1,
                ..StoreState::default()
            }),
            paging_out: Mutex::new(()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, StoreState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Write the blobs stored since the last flush to a new file.
    fn write_pending(&self) -> io::Result<()> {
        let mut state = self.lock();
        if state.pending.is_empty() {
            return Ok(());
        }
        let id = state.files.len() as u32;
        let path = self.dir.join(format!("{id:08}.heaps"));
        let mut contents = Vec::new();
        let mut placed = Vec::with_capacity(state.pending.len());
        for (key, data) in &state.pending {
            let record = encode_record(data);
            placed.push((*key, contents.len() as u64, record.len() as u32));
            contents.extend_from_slice(&record);
        }
        let mut file = File::create(&path)?;
        file.write_all(&contents)?;
        drop(file);
        state.files.push(File::open(&path)?);
        for (key, offset, len) in placed {
            state.index.insert(key, (id, offset, len));
        }
        state.pending.clear();
        Ok(())
    }

    fn read(&self, key: &DataKey) -> anyhow::Result<Arc<PagableData>> {
        let (file, offset, len) = {
            let mut state = self.lock();
            if let Some(data) = state.pending.get(key) {
                return Ok(data.clone());
            }
            let &(id, offset, len) = state.index.get(key).ok_or_else(|| anyhow::anyhow!("no paged heap {key:?}"))?;
            state.fetched.push(*key);
            (state.files[id as usize].try_clone()?, offset, len)
        };
        let mut record = vec![0u8; len as usize];
        file.read_exact_at(&mut record, offset)?;
        decode_record(&record)
    }
}

#[async_trait::async_trait]
impl PagableStorage for HeapStore {
    fn arc_cache(&self) -> &DeserializedArcCache {
        &self.arcs
    }

    fn fetch_data_blocking(&self, key: &DataKey) -> anyhow::Result<Arc<PagableData>> {
        self.read(key)
    }

    async fn fetch_data(&self, key: &DataKey) -> anyhow::Result<Arc<PagableData>> {
        self.read(key)
    }

    fn schedule_for_paging(&self, _arc: Box<dyn ArcEraseDyn>) {}

    fn session_context(&self) -> &SessionContext {
        &self.session
    }

    fn store_data(&self, data: PagableData) -> anyhow::Result<DataKey> {
        let key = data.compute_key();
        let mut state = self.lock();
        if !state.index.contains_key(&key) {
            state.pending.entry(key).or_insert_with(|| Arc::new(data));
        }
        Ok(key)
    }

    fn pinned_key(&self, arc: &dyn ArcEraseDyn) -> Option<DataKey> {
        let identity = arc.identity();
        let mut state = self.lock();
        if state.paging.contains(&identity) {
            return None;
        }
        if let Some(key) = state.known.get(&identity).copied().or_else(|| state.read_back_key(identity)) {
            return Some(key);
        }
        let key = DataKey(u128::MAX - state.next_pin);
        state.next_pin += 1;
        state.known.insert(identity, key);
        state.pinned.insert(key, arc.clone_dyn());
        drop(state);
        self.arcs.insert(arc.as_arc_any().type_id(), key, arc);
        Some(key)
    }
}

/// A heap read back: the key it came from, and a weak reference to it.
struct ReadBack {
    key: DataKey,
    heap: Box<dyn WeakEraseDyn>,
}

impl StoreState {
    /// The key a heap was read back from, if it was and is still live.
    fn read_back_key(&self, identity: usize) -> Option<DataKey> {
        let read_back = self.read_back.get(&identity)?;
        (!read_back.heap.is_expired()).then_some(read_back.key)
    }
}

/// A record: the number of nested arcs, their keys, then the data,
/// compressed.
fn encode_record(data: &PagableData) -> Vec<u8> {
    let mut raw = Vec::with_capacity(4 + data.arcs.len() * 16 + data.data.len());
    raw.extend_from_slice(&(data.arcs.len() as u32).to_le_bytes());
    for key in &data.arcs {
        raw.extend_from_slice(&key.0.to_le_bytes());
    }
    raw.extend_from_slice(&data.data);
    zstd::bulk::compress(&raw, 1).unwrap_or(raw)
}

fn decode_record(record: &[u8]) -> anyhow::Result<Arc<PagableData>> {
    let raw = zstd::stream::decode_all(record)?;
    let count = u32::from_le_bytes(raw.get(..4).ok_or_else(|| anyhow::anyhow!("short paged record"))?.try_into()?) as usize;
    let keys_end = 4 + count * 16;
    let keys = raw.get(4..keys_end).ok_or_else(|| anyhow::anyhow!("short paged record"))?;
    let arcs = keys.chunks_exact(16).map(|chunk| DataKey(u128::from_le_bytes(chunk.try_into().unwrap_or_default()))).collect();
    Ok(Arc::new(PagableData {
        data: raw[keys_end..].to_vec(),
        arcs,
    }))
}

/// A target's providers: live, or paged out and read back when used.
pub struct PagedValue {
    state: Mutex<PagedState>,
}

struct PagedState {
    live: Option<OwnedFrozenValue>,
    /// Where the value is stored, once it has been paged out.
    key: Option<DataKey>,
}

impl PagedValue {
    pub fn new(value: OwnedFrozenValue) -> PagedValue {
        PagedValue {
            state: Mutex::new(PagedState { live: Some(value), key: None }),
        }
    }

    /// The value, reading it back if it's paged out.
    pub fn get(&self) -> anyhow::Result<OwnedFrozenValue> {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(live) = &state.live {
            return Ok(live.clone());
        }
        let key = state.key.ok_or_else(|| anyhow::anyhow!("a paged value has no key"))?;
        let store = store().ok_or_else(|| anyhow::anyhow!("paging isn't set up"))?;
        let value = store.page_in(key)?;
        state.live = Some(value.clone());
        Ok(value)
    }

    fn live(&self) -> Option<OwnedFrozenValue> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).live.clone()
    }

    fn stored(&self, key: DataKey) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.key = Some(key);
        state.live = None;
    }

    /// Where the value is stored, if it has been paged out.
    fn key(&self) -> Option<DataKey> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).key
    }

    /// Drop the live copy of a value read back, which is still stored.
    fn release(&self) {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).live = None;
    }
}

/// Page out every live value of `values` that can be, together, returning
/// how many were. A value that can't be is kept in memory. Blocks on the
/// filesystem.
pub fn page_out<'a>(values: impl IntoIterator<Item = &'a PagedValue>) -> anyhow::Result<usize> {
    match store() {
        Some(store) => store.page_out(values),
        None => Ok(0),
    }
}

impl HeapStore {
    /// Read back the value stored at `key`.
    fn page_in(self: &Arc<HeapStore>, key: DataKey) -> anyhow::Result<OwnedFrozenValue> {
        let data = self.read(&key)?;
        let handle = PagableStorageHandle::new(self.clone());
        let mut deserializer = PagableDeserializerImpl::new(&data.data, &data.arcs, &handle);
        let value = OwnedFrozenValue::pagable_deserialize(&mut deserializer)?;
        self.note_read_back();
        Ok(value)
    }

    /// Page out every live value of `values` that can be, together,
    /// returning how many were. What the serializer learns of heaps'
    /// addresses is forgotten afterwards: it takes an entry for every value
    /// of every heap referred to, and those heaps may be freed or read back
    /// before the next page-out.
    ///
    /// A value read back is still stored as it was, so it's released rather
    /// than stored again: stored again, its heap could get another key, and
    /// reading both keys back would make two copies of one heap.
    fn page_out<'a>(self: &Arc<HeapStore>, values: impl IntoIterator<Item = &'a PagedValue>) -> anyhow::Result<usize> {
        let _paging_out = self.paging_out.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let live: Vec<(&PagedValue, OwnedFrozenValue)> = values.into_iter().filter_map(|value| Some((value, value.live()?))).collect();
        let (read_back, new): (Vec<_>, Vec<_>) = live.into_iter().partition(|(value, _)| value.key().is_some());
        self.mark_paging(&new);
        self.register_pinned_heaps(new.iter().map(|(_, value)| value.owner()));
        let stored = self.store_values(&new);
        starlark::pagable::forget_serialized_heaps(&self.session);
        self.forget_freed();
        let mut paged = 0;
        for ((value, _), key) in new.iter().zip(stored?) {
            if let Some(key) = key {
                value.stored(key);
                paged += 1;
            }
        }
        for (value, _) in &read_back {
            value.release();
        }
        Ok(paged + read_back.len())
    }

    /// Note the heaps of `values` as being paged out, other than those read
    /// back, which are stored already: an alias's value is its target's, so
    /// its heap may have been read back for the target.
    fn mark_paging(&self, values: &[(&PagedValue, OwnedFrozenValue)]) {
        let identities: Vec<usize> = values.iter().filter_map(|(_, value)| heap_identity(&self.session, value.owner())).collect();
        let mut state = self.lock();
        let paging = identities.into_iter().filter(|identity| state.read_back_key(*identity).is_none()).collect();
        state.paging = paging;
    }

    /// Serialize `values` and write them to a file, returning their keys,
    /// or `None` for those that couldn't be serialized, which are logged.
    fn store_values(&self, values: &[(&PagedValue, OwnedFrozenValue)]) -> anyhow::Result<Vec<Option<DataKey>>> {
        let finished: DashMap<usize, Arc<ArcSerSlot>> = DashMap::new();
        let mut keys = Vec::with_capacity(values.len());
        for (_, value) in values {
            let key = self.store_value(value, &finished);
            if let Err(error) = &key {
                tracing::warn!("keeping a value in memory that couldn't be paged out: {error:#}");
            }
            keys.push(key.ok());
        }
        self.write_pending()?;
        Ok(keys)
    }

    fn store_value(&self, value: &OwnedFrozenValue, finished: &DashMap<usize, Arc<ArcSerSlot>>) -> anyhow::Result<DataKey> {
        let mut serializer = SerializerForPaging::new(&self.session);
        value.pagable_serialize(&mut serializer)?;
        let (data, arcs) = serializer.finish();
        self.page_out_item(data, arcs, finished, &self.session).map_err(|error| match error {
            PageOutError::Failed(error) => error,
            PageOutError::AlreadyFailed => anyhow::anyhow!("paging out a heap failed"),
        })
    }

    /// Forget the heaps read back that have since been freed. The cache of
    /// read heaps holds them weakly, so a heap is read once while it's live
    /// however many values point into it, and freed once none is used.
    fn forget_freed(&self) {
        let mut state = self.lock();
        state.paging.clear();
        state.read_back.retain(|_, read_back| !read_back.heap.is_expired());
        drop(state);
        self.arcs.prune();
    }
}

impl HeapStore {
    /// Register, for reading back, the values of every heap that `heaps`
    /// refer to, directly or not, other than those being paged out, which
    /// stay live and are referred to by key.
    fn register_pinned_heaps<'a>(&self, heaps: impl IntoIterator<Item = &'a FrozenHeapRef>) {
        let mut pending: Vec<FrozenHeapRef> = heaps.into_iter().cloned().collect();
        let mut visited: HashSet<usize> = HashSet::new();
        while let Some(heap) = pending.pop() {
            let Some(identity) = heap_identity(&self.session, &heap) else {
                continue;
            };
            if !visited.insert(identity) {
                continue;
            }
            pending.extend(heap.refs().cloned());
            let mut state = self.lock();
            if state.paging.contains(&identity) || state.registered.contains_key(&identity) {
                continue;
            }
            starlark::pagable::register_live_heap(&self.session, &heap);
            state.registered.insert(identity, heap);
        }
    }

    /// Note the identities of the heaps just read back.
    fn note_read_back(&self) {
        let heap_type = heap_type_id();
        let mut state = self.lock();
        let fetched = std::mem::take(&mut state.fetched);
        for key in fetched {
            let Some(arc) = self.arcs.get(&heap_type, &key) else {
                continue;
            };
            let Some(heap) = arc.downgrade() else {
                continue;
            };
            state.read_back.insert(arc.identity(), ReadBack { key, heap });
        }
    }
}

/// The identity a heap has as an arc, found by serializing a reference to
/// it.
fn heap_identity(session: &SessionContext, heap: &FrozenHeapRef) -> Option<usize> {
    let mut serializer = SerializerForPaging::new(session);
    heap.pagable_serialize(&mut serializer).ok()?;
    let (_, arcs) = serializer.finish();
    arcs.first().map(|arc| arc.identity())
}

/// The type of the arc a frozen heap is held in, as the cache of read
/// heaps knows it. The heap probed for it holds a value, since an empty
/// heap isn't held in an arc at all.
fn heap_type_id() -> std::any::TypeId {
    static HEAP_TYPE: OnceLock<std::any::TypeId> = OnceLock::new();
    *HEAP_TYPE.get_or_init(|| {
        let probe = starlark::values::FrozenHeap::new();
        probe.alloc_str("probe");
        let heap = probe.into_ref_named(starlark::values::FrozenHeapName::User(Box::new("probe".to_string())));
        let session = SessionContext::new();
        let mut serializer = SerializerForPaging::new(&session);
        let _ = heap.pagable_serialize(&mut serializer);
        let (_, arcs) = serializer.finish();
        arcs.first().map_or(std::any::TypeId::of::<()>(), |arc| arc.as_arc_any().type_id())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{Globals, Module};
    use starlark::syntax::{AstModule, Dialect};
    use starlark::values::FrozenHeapName;

    fn name(text: &str) -> FrozenHeapName {
        FrozenHeapName::User(Box::new(text.to_string()))
    }

    /// A frozen module from `code`, with `deps`' values as globals.
    fn module(code: &str, heap_name: &str, deps: &[(&str, &OwnedFrozenValue)]) -> starlark::environment::FrozenModule {
        Module::with_temp_heap(|module| {
            for (variable, value) in deps {
                module.set(variable, module.heap().access_owned_frozen_value(value));
            }
            let ast = AstModule::parse("test.bzl", code.to_string(), &Dialect::Extended).unwrap();
            let mut eval = starlark::eval::Evaluator::new(&module);
            eval.eval_module(ast, &Globals::standard()).unwrap();
            drop(eval);
            module.freeze_named(name(heap_name))
        })
        .unwrap()
    }

    #[test]
    fn heaps_are_found_in_the_cache_of_read_heaps() {
        assert_ne!(heap_type_id(), std::any::TypeId::of::<()>());
    }

    #[test]
    fn a_value_pointing_into_a_pinned_heap_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HeapStore::new(dir.path().to_path_buf()));
        let bzl = module("shared = {'key': 'value', 'other': [1, 2]}\nlabel = 'a string'", "bzl", &[]);
        let shared = bzl.get("shared").unwrap();
        let target = module("providers = {'files': [label], 'shared': shared, shared['key']: 3}", "target", &[("shared", &shared), ("label", &bzl.get("label").unwrap())]);
        let value = PagedValue::new(target.get("providers").unwrap());
        let before = value.get().unwrap().value().to_repr();
        drop(target);

        assert_eq!(store.page_out([&value]).unwrap(), 1);
        let key = value.state.lock().unwrap().key.unwrap();
        let read = store.page_in(key).unwrap();
        assert_eq!(read.value().to_repr(), before);
    }

    #[test]
    fn page_outs_at_once_take_turns() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HeapStore::new(dir.path().to_path_buf()));
        let bzl = module("shared = {'key': 'value', 'other': [1, 2]}", "bzl", &[]);
        let shared = bzl.get("shared").unwrap();
        let values: Vec<PagedValue> = (0..64)
            .map(|index| {
                let code = format!("providers = {{'shared': shared, 'own': [{index}, 'x']}}");
                let target = module(&code, &format!("target {index}"), &[("shared", &shared)]);
                PagedValue::new(target.get("providers").unwrap())
            })
            .collect();
        std::thread::scope(|scope| {
            for chunk in values.chunks(8) {
                let store = store.clone();
                scope.spawn(move || assert_eq!(store.page_out(chunk).unwrap(), chunk.len()));
            }
        });
        for value in &values {
            assert!(value.key().is_some());
        }
    }

    #[test]
    fn a_value_read_back_pages_out_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HeapStore::new(dir.path().to_path_buf()));
        let bzl = module("shared = {'key': 'value'}", "bzl", &[]);
        let target = module("providers = {'shared': shared, 'own': ['a', 'b']}", "target", &[("shared", &bzl.get("shared").unwrap())]);
        let value = PagedValue::new(target.get("providers").unwrap());
        let before = value.get().unwrap().value().to_repr();
        drop(target);

        for _ in 0..3 {
            assert_eq!(store.page_out([&value]).unwrap(), 1);
            let key = value.state.lock().unwrap().key.unwrap();
            value.state.lock().unwrap().live = Some(store.page_in(key).unwrap());
        }
        assert_eq!(value.live().unwrap().value().to_repr(), before);
    }

    #[test]
    fn a_value_read_back_is_released_not_stored_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HeapStore::new(dir.path().to_path_buf()));
        let target = module("providers = {'own': ['a', 'b']}", "target", &[]);
        let value = PagedValue::new(target.get("providers").unwrap());
        drop(target);
        store.page_out([&value]).unwrap();
        let key = value.key().unwrap();
        let stored = store.lock().index.len();

        value.state.lock().unwrap().live = Some(store.page_in(key).unwrap());
        assert_eq!(store.page_out([&value]).unwrap(), 1);
        assert_eq!(value.key(), Some(key));
        assert!(value.live().is_none());
        assert_eq!(store.lock().index.len(), stored);
    }

    #[test]
    fn a_heap_read_back_is_stored_once_for_every_value_in_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HeapStore::new(dir.path().to_path_buf()));
        let target = module("providers = {'own': ['a', 'b']}", "target", &[]);
        let value = PagedValue::new(target.get("providers").unwrap());
        drop(target);
        store.page_out([&value]).unwrap();

        let read = store.page_in(value.key().unwrap()).unwrap();
        value.state.lock().unwrap().live = Some(read.clone());
        let alias = PagedValue::new(read);
        assert_eq!(store.page_out([&value, &alias]).unwrap(), 2);
        let target = store.page_in(value.key().unwrap()).unwrap();
        let aliased = store.page_in(alias.key().unwrap()).unwrap();
        assert_eq!(heap_identity(&store.session, target.owner()), heap_identity(&store.session, aliased.owner()));
    }

    #[test]
    fn a_function_runs_after_its_heap_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HeapStore::new(dir.path().to_path_buf()));
        let bzl = module("def make(n):\n    return lambda x: x + n", "bzl", &[]);
        let target = module("providers = {'add': make(2)}", "target", &[("make", &bzl.get("make").unwrap())]);
        let value = PagedValue::new(target.get("providers").unwrap());
        drop(target);

        assert_eq!(store.page_out([&value]).unwrap(), 1);
        let read = store.page_in(value.key().unwrap()).unwrap();
        let result = module("result = providers['add'](1)", "user", &[("providers", &read)]);
        assert_eq!(result.get("result").unwrap().value().unpack_i32(), Some(3));
    }

    #[test]
    fn a_function_read_back_through_its_dependencies_runs() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HeapStore::new(dir.path().to_path_buf()));
        let bzl = module("def make(n):\n    return lambda x: x + n", "bzl", &[]);
        let leaf = module("providers = {'n': [2]}", "leaf", &[]);
        let leaf_value = PagedValue::new(leaf.get("providers").unwrap());
        let functions = module("providers = {'add': make(leaf['n'][0]), 'n': leaf['n']}", "functions", &[("make", &bzl.get("make").unwrap()), ("leaf", &leaf.get("providers").unwrap())]);
        let functions_value = PagedValue::new(functions.get("providers").unwrap());
        let target = module("providers = {'add': dep['add'], 'n': dep['n']}", "target", &[("dep", &functions.get("providers").unwrap())]);
        let value = PagedValue::new(target.get("providers").unwrap());
        drop((leaf, functions, target));

        assert_eq!(store.page_out([&leaf_value, &functions_value, &value]).unwrap(), 3);
        let read = store.page_in(value.key().unwrap()).unwrap();
        let result = module("result = providers['add'](providers['n'][0])", "user", &[("providers", &read)]);
        assert_eq!(result.get("result").unwrap().value().unpack_i32(), Some(4));
    }

    #[test]
    fn a_function_read_back_pages_out_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(HeapStore::new(dir.path().to_path_buf()));
        let bzl = module("def make(n):\n    return lambda x: x * n", "bzl", &[]);
        let target = module("providers = {'times': make(3)}", "target", &[("make", &bzl.get("make").unwrap())]);
        let value = PagedValue::new(target.get("providers").unwrap());
        drop(target);

        for round in 1..=3 {
            assert_eq!(store.page_out([&value]).unwrap(), 1);
            let read = store.page_in(value.key().unwrap()).unwrap();
            value.state.lock().unwrap().live = Some(read.clone());
            let result = module("result = providers['times'](5)", &format!("user{round}"), &[("providers", &read)]);
            assert_eq!(result.get("result").unwrap().value().unpack_i32(), Some(15));
        }
    }
}
