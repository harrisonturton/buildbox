//! Paging analyzed targets' frozen heaps out of memory.
//!
//! An analyzed target's providers live in a frozen Starlark heap, which
//! refers to its dependencies' heaps and to the heaps of the `.bzl`
//! modules, builtins and globals it used. A [`HeapStore`] serializes
//! targets' heaps to files and reads them back when they're next used.
//! Heaps of other kinds are never serialized, since starlark-rust can't yet
//! serialize compiled functions: they're pinned, kept live, and referred to
//! by a key that resolves back to the live heap when a target is read.
//!
//! Targets are paged out together, since a target's heap is freed only once
//! no dependent's live heap refers to it.

use dashmap::DashMap;
use pagable::arc_erase::ArcEraseDyn;
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
    /// The key each heap read back since the cache of read heaps was last
    /// cleared came from, by identity. The cache holds them, so their
    /// identities can't be reused meanwhile.
    read_back: HashMap<usize, DataKey>,
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
        if let Some(key) = state.known.get(&identity).or_else(|| state.read_back.get(&identity)) {
            return Some(*key);
        }
        let key = DataKey(u128::MAX - state.next_pin);
        state.next_pin += 1;
        state.known.insert(identity, key);
        state.pinned.insert(key, arc.clone_dyn());
        drop(state);
        let cell = self.arcs.get_or_create_cell(arc.as_arc_any().type_id(), key);
        let _ = cell.set(arc.clone_dyn());
        Some(key)
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
}

/// Page out every live value of `values` together, returning how many
/// were. Blocks on the filesystem.
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

    /// Page out every live value of `values` together, returning how many
    /// were.
    fn page_out<'a>(self: &Arc<HeapStore>, values: impl IntoIterator<Item = &'a PagedValue>) -> anyhow::Result<usize> {
        let store = self;
        let live: Vec<(&PagedValue, OwnedFrozenValue)> = values.into_iter().filter_map(|value| Some((value, value.live()?))).collect();
    {
        let mut state = store.lock();
        state.paging = live.iter().filter_map(|(_, value)| heap_identity(&store.session, value.owner())).collect();
    }
    store.register_pinned_heaps(live.iter().map(|(_, value)| value.owner()));
    let finished: DashMap<usize, Arc<ArcSerSlot>> = DashMap::new();
    let mut keys = Vec::with_capacity(live.len());
    for (_, value) in &live {
        let mut serializer = SerializerForPaging::new(&store.session);
        value.pagable_serialize(&mut serializer)?;
        let (data, arcs) = serializer.finish();
        let key = store.page_out_item(data, arcs, &finished, &store.session).map_err(|error| match error {
            PageOutError::Failed(error) => error,
            PageOutError::AlreadyFailed => anyhow::anyhow!("paging out a heap failed"),
        })?;
        keys.push(key);
    }
    store.write_pending()?;
    {
        let mut state = store.lock();
        state.paging.clear();
        state.read_back.clear();
    }
    store.arcs.clear();
    store.reregister_pinned();
    for ((value, _), key) in live.iter().zip(keys) {
        value.stored(key);
    }
    Ok(live.len())
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

    /// Note the identities of the heaps just read back, which the cache of
    /// read heaps now holds.
    fn note_read_back(&self) {
        let heap_type = heap_type_id();
        let mut state = self.lock();
        let fetched = std::mem::take(&mut state.fetched);
        for key in fetched {
            if let Some(arc) = self.arcs.get(&heap_type, &key) {
                state.read_back.insert(arc.identity(), key);
            }
        }
    }

    /// Put the pinned heaps back in the cache of read arcs, after it's
    /// cleared of targets' heaps so that those can be freed.
    fn reregister_pinned(&self) {
        let state = self.lock();
        for (key, arc) in &state.pinned {
            let cell = self.arcs.get_or_create_cell(arc.as_arc_any().type_id(), *key);
            let _ = cell.set(arc.clone_dyn());
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
}
