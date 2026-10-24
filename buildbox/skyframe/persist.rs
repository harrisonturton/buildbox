//! Saving an engine to disk, and restoring it after a restart.
//!
//! An engine is saved as its memo: the traces it remembers for each key and
//! the values they reach. A restored engine's workspaces reuse them as they
//! would another workspace's, so after a restart only what changed, and
//! live values such as evaluated Starlark, is computed again. Values go to
//! the store's segment files. The rest goes to a manifest, replaced
//! atomically by each checkpoint: the intern tables, kinds' names, keys,
//! memo and the store's image. A manifest written by a different build of
//! the program is ignored, since what it saved may mean something else now.

use crate::Engine;
use crate::engine::EngineInner;
use crate::keys::KeyTable;
use crate::kinds;
use skycodec::{Decode, DecodeError, Decoder, Encode, Encoder, Fingerprint};
use skystore::{StoreImage, ValueRef, ValueStore};
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;

/// Where and how an engine is saved.
#[derive(Clone, Debug)]
pub struct Persistence {
    pub dir: PathBuf,
    /// Identifies the build of the program, whose saved engines it reads.
    pub version: Fingerprint,
    /// Above this, a checkpoint saves only each key's newest trace.
    pub max_disk_bytes: u64,
}

/// What a checkpoint saved.
#[derive(Clone, Copy, Debug, Default)]
pub struct CheckpointStats {
    pub keys: usize,
    pub traces: usize,
    pub values: usize,
    pub segment_bytes: u64,
    pub manifest_bytes: u64,
}

const MAGIC: &[u8; 8] = b"bbengine";
const FORMAT: u32 = 1;

/// Everything but the values.
#[derive(Default)]
struct Manifest {
    symbols: Vec<String>,
    paths: Vec<(u32, u32)>,
    kinds: Vec<String>,
    parts: Vec<String>,
    keys: Vec<Vec<u8>>,
    memo: Vec<(u32, Vec<u32>)>,
    store: StoreImage,
}

impl Persistence {
    fn manifest_path(&self) -> PathBuf {
        self.dir.join("manifest")
    }

    fn segments_dir(&self) -> PathBuf {
        self.dir.join("segments")
    }
}

/// The parts of a restored engine.
pub(crate) struct Restored {
    pub keys: KeyTable,
    pub store: ValueStore,
    pub memo: Vec<(u32, Vec<ValueRef>)>,
}

/// The engine saved under `persistence`, if there's one this build can
/// use. Call before anything is interned or any key type registered.
pub(crate) fn restore(persistence: &Persistence) -> io::Result<Option<Restored>> {
    let bytes = match std::fs::read(persistence.manifest_path()) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(manifest) = parse(&bytes, persistence.version)? else {
        tracing::info!("the saved engine is from another build of bb; starting afresh");
        return Ok(None);
    };
    let invalid = |what: String| io::Error::other(format!("saved engine: {what}"));
    intern::snapshot::restore_symbols(0, manifest.symbols.iter().map(String::as_str)).map_err(|error| invalid(error.to_string()))?;
    intern::snapshot::restore_paths(0, manifest.paths.iter().copied()).map_err(|error| invalid(error.to_string()))?;
    kinds::assign(&manifest.kinds).map_err(|conflict| invalid(format!("kind {} has another id", conflict.0)))?;
    kinds::assign_parts(&manifest.parts).map_err(|conflict| invalid(format!("part {} has another id", conflict.0)))?;
    let keys = KeyTable::restore(manifest.keys.iter().map(Vec::as_slice)).ok_or_else(|| invalid("keys don't restore to their ids".to_string()))?;
    let store = ValueStore::restore(&persistence.segments_dir(), &manifest.store)?;
    let memo = manifest.memo.into_iter().map(|(key, traces)| (key, traces.into_iter().map(ValueRef::from_id).collect())).collect();
    Ok(Some(Restored { keys, store, memo }))
}

/// Forget any saved engine's values.
pub(crate) fn clear(persistence: &Persistence) -> io::Result<()> {
    skystore::remove_unused_segments(&persistence.segments_dir(), &[])
}

impl Engine {
    /// Save the engine, if it was opened with [`Persistence`]. Blocks on the
    /// filesystem; call while no computation is running.
    pub fn checkpoint(&self) -> io::Result<Option<CheckpointStats>> {
        let Some(persistence) = &self.0.persistence else {
            return Ok(None);
        };
        let mut checkpointed = self.0.checkpointing.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let changes = self.0.memo_changes.load(std::sync::atomic::Ordering::Relaxed);
        if changes == *checkpointed && persistence.manifest_path().exists() {
            return Ok(None);
        }
        let _span = tracing::info_span!("skyframe.checkpoint").entered();
        let mut memo = self.0.memo_snapshot();
        let mut image = self.0.save_store(persistence, &memo)?;
        if image.disk_bytes() > persistence.max_disk_bytes {
            tracing::info!("saved engine is {} bytes; keeping only the newest traces", image.disk_bytes());
            for (_, traces) in &mut memo {
                traces.truncate(1);
            }
            image = self.0.save_store(persistence, &memo)?;
        }
        let saved: HashSet<ValueRef> = image.entries.iter().map(|entry| entry.value).collect();
        self.0.keep_saved_traces(&mut memo, &saved);
        let manifest = Manifest {
            symbols: intern::snapshot::symbols_from(0).into_iter().map(str::to_string).collect(),
            paths: intern::snapshot::paths_from(0),
            kinds: kinds::assigned_names(),
            parts: kinds::part_names(),
            keys: self.0.keys.blobs().into_iter().map(<[u8]>::to_vec).collect(),
            memo: memo.iter().map(|(key, traces)| (*key, traces.iter().map(|trace| trace.id()).collect())).collect(),
            store: image,
        };
        let bytes = serialize(&manifest, persistence.version);
        persist::write_atomically(&persistence.manifest_path(), &bytes)?;
        skystore::remove_unused_segments(&persistence.segments_dir(), &manifest.store.segments)?;
        *checkpointed = changes;
        Ok(Some(CheckpointStats {
            keys: manifest.keys.len(),
            traces: manifest.memo.iter().map(|(_, traces)| traces.len()).sum(),
            values: manifest.store.entries.len(),
            segment_bytes: manifest.store.disk_bytes(),
            manifest_bytes: bytes.len() as u64,
        }))
    }
}

impl EngineInner {
    /// Forget the traces that refer to a value the image didn't save, such
    /// as one freed while it was saved: its reference could be given to
    /// another value in the restored store.
    fn keep_saved_traces(&self, memo: &mut Vec<(u32, Vec<ValueRef>)>, saved: &HashSet<ValueRef>) {
        let complete = |trace: &ValueRef| {
            saved.contains(trace)
                && self
                    .trace(*trace)
                    .is_ok_and(|decoded| saved.contains(&decoded.value) && decoded.deps.iter().all(|dep| saved.contains(&dep.value)))
        };
        for (_, traces) in memo.iter_mut() {
            traces.retain(complete);
        }
        memo.retain(|(_, traces)| !traces.is_empty());
    }

    /// Save the values `memo` reaches, each with the references the memo's
    /// traces hold on it.
    fn save_store(&self, persistence: &Persistence, memo: &[(u32, Vec<ValueRef>)]) -> io::Result<StoreImage> {
        let mut refs: HashMap<ValueRef, u32> = HashMap::new();
        for trace in memo.iter().flat_map(|(_, traces)| traces) {
            *refs.entry(*trace).or_default() += 1;
        }
        let traces: Vec<ValueRef> = refs.keys().copied().collect();
        for trace in traces {
            let Ok(decoded) = self.trace(trace) else {
                refs.remove(&trace);
                continue;
            };
            *refs.entry(decoded.value).or_default() += 1;
            for dep in &decoded.deps {
                *refs.entry(dep.value).or_default() += 1;
            }
        }
        let mut refs: Vec<(ValueRef, u32)> = refs.into_iter().collect();
        refs.sort_unstable();
        self.store.checkpoint(&persistence.segments_dir(), &refs)
    }
}

/// The manifest file: the magic, format and program version, the encoded
/// manifest, and a checksum of everything before it.
fn serialize(manifest: &Manifest, version: Fingerprint) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&FORMAT.to_le_bytes());
    bytes.extend_from_slice(version.as_bytes());
    bytes.extend_from_slice(&skycodec::to_bytes(manifest));
    let checksum = Fingerprint::of(&bytes);
    bytes.extend_from_slice(checksum.as_bytes());
    bytes
}

/// The manifest, or `None` if another build or format wrote it.
fn parse(bytes: &[u8], version: Fingerprint) -> io::Result<Option<Manifest>> {
    let damaged = || io::Error::other("the saved engine's manifest is damaged");
    let body_len = bytes.len().checked_sub(Fingerprint::LEN).ok_or_else(damaged)?;
    let (body, checksum) = bytes.split_at(body_len);
    if Fingerprint::of(body).as_bytes() != checksum {
        return Err(damaged());
    }
    let header_len = MAGIC.len() + 4 + Fingerprint::LEN;
    if body.len() < header_len || &body[..MAGIC.len()] != MAGIC {
        return Err(damaged());
    }
    let format = u32::from_le_bytes(body[MAGIC.len()..MAGIC.len() + 4].try_into().map_err(|_| damaged())?);
    if format != FORMAT || &body[MAGIC.len() + 4..header_len] != version.as_bytes() {
        return Ok(None);
    }
    skycodec::from_bytes(&body[header_len..]).map(Some).map_err(|error| io::Error::other(format!("saved engine: {error}")))
}

impl Encode for Manifest {
    fn encode(&self, e: &mut Encoder) {
        e.usize(self.symbols.len());
        for symbol in &self.symbols {
            e.str(symbol);
        }
        e.usize(self.paths.len());
        for (parent, name) in &self.paths {
            e.u32(*parent);
            e.u32(*name);
        }
        self.kinds.encode(e);
        self.parts.encode(e);
        e.usize(self.keys.len());
        for key in &self.keys {
            e.bytes(key);
        }
        e.usize(self.memo.len());
        for (key, traces) in &self.memo {
            e.u32(*key);
            e.usize(traces.len());
            for trace in traces {
                e.u32(*trace);
            }
        }
        self.store.encode(e);
    }
}

impl Decode for Manifest {
    fn decode(d: &mut Decoder<'_>) -> Result<Manifest, DecodeError> {
        let mut manifest = Manifest::default();
        for _ in 0..d.usize()? {
            manifest.symbols.push(d.str()?.to_string());
        }
        for _ in 0..d.usize()? {
            manifest.paths.push((d.u32()?, d.u32()?));
        }
        manifest.kinds = Vec::decode(d)?;
        manifest.parts = Vec::decode(d)?;
        for _ in 0..d.usize()? {
            manifest.keys.push(d.bytes()?.to_vec());
        }
        for _ in 0..d.usize()? {
            let key = d.u32()?;
            let mut traces = Vec::new();
            for _ in 0..d.usize()? {
                traces.push(d.u32()?);
            }
            manifest.memo.push((key, traces));
        }
        manifest.store = StoreImage::decode(d)?;
        Ok(manifest)
    }
}
