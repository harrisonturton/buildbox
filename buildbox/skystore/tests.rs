use super::*;

const KIND: Kind = Kind(1);

fn insert(store: &ValueStore, bytes: &[u8]) -> ValueRef {
    store.insert(KIND, Fingerprint::of(bytes), bytes)
}

fn read_bytes(store: &ValueStore, value: ValueRef) -> Vec<u8> {
    match store.read(value).unwrap() {
        Stored::Bytes(bytes) => bytes,
        Stored::Live(_) => panic!("expected bytes"),
    }
}

fn target(i: usize) -> Vec<u8> {
    format!("rust_library name=lib{i} srcs=[src/lib{i}.rs, src/util.rs] deps=[//third_party:serde, //buildbox/intern] visibility=//buildbox:__subpackages__")
        .into_bytes()
}

#[test]
fn insert_then_read() {
    let store = ValueStore::new();
    for bytes in [&b""[..], b"tiny", &target(1), &vec![9u8; 1 << 20]] {
        let value = insert(&store, bytes);
        assert_eq!(read_bytes(&store, value), bytes);
        assert_eq!(store.fingerprint(value), Some(Fingerprint::of(bytes)));
    }
}

#[test]
fn equal_values_are_stored_once() {
    let store = ValueStore::new();
    let a = insert(&store, &target(1));
    let b = insert(&store, &target(1));
    assert_eq!(a, b);
    assert_eq!(store.stats().values, 1);
    assert_ne!(a, insert(&store, &target(2)));
}

#[test]
fn values_are_freed_with_their_last_reference() {
    let store = ValueStore::new();
    let value = insert(&store, &target(1));
    store.retain(value);
    assert!(!store.release(value));
    assert!(read_bytes(&store, value) == target(1));
    assert!(store.release(value));
    assert_eq!(store.read(value).err(), Some(StoreError::Missing(value)));
    assert_eq!(store.stats().values, 0);
    assert_eq!(store.stats().stored_bytes, 0);
}

#[test]
fn freed_slots_are_reused() {
    let store = ValueStore::new();
    for round in 0..3 {
        let values: Vec<ValueRef> = (0..1000).map(|i| insert(&store, &target(round * 1000 + i))).collect();
        for value in values {
            store.release(value);
        }
    }
    let stats = store.stats();
    assert_eq!(stats.values, 0);
    assert!(stats.entry_bytes < 200_000, "{stats:?}");
}

#[test]
fn a_dictionary_is_trained_for_each_kind() {
    let store = ValueStore::new();
    let values: Vec<ValueRef> = (0..3000).map(|i| insert(&store, &target(i))).collect();
    let stats = store.stats();
    let kind = stats.kinds[KIND.0 as usize];
    assert!(kind.dictionary_bytes > 0, "{stats:?}");
    assert!(kind.stored_bytes * 4 < kind.raw_bytes, "{kind:?}");
    for (i, value) in values.iter().enumerate() {
        assert_eq!(read_bytes(&store, *value), target(i));
    }
}

#[test]
fn live_values_are_deduplicated() {
    let store = ValueStore::new();
    let fingerprint = Fingerprint::of(b"module");
    let a = store.insert_live(KIND, fingerprint, Arc::new(String::from("first")), 100);
    let b = store.insert_live(KIND, fingerprint, Arc::new(String::from("second")), 100);
    assert_eq!(a, b);
    let Stored::Live(live) = store.read(a).unwrap() else {
        panic!("expected a live value");
    };
    assert_eq!(live.downcast_ref::<String>().unwrap(), "first");
    assert_eq!(store.stats().live_bytes, 100);
    store.release(a);
    store.release(b);
    assert_eq!(store.stats().live_values, 0);
}

/// Bytes that don't compress, different for each seed.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

#[test]
fn freed_pages_are_compacted() {
    let store = ValueStore::new();
    let values: Vec<ValueRef> = (0..40_000).map(|i| insert(&store, &noise(i, 1000))).collect();
    let before = store.stats();
    assert_eq!(before.values, 40_000);
    for (i, value) in values.iter().enumerate() {
        if i % 10 != 0 {
            store.release(*value);
        }
    }
    let after = store.stats();
    assert_eq!(after.values, 4000);
    assert!(after.page_bytes < before.page_bytes / 4, "{before:?} -> {after:?}");
    for (i, value) in values.iter().enumerate().step_by(10) {
        assert_eq!(read_bytes(&store, *value), noise(i as u64, 1000));
    }
}

#[test]
fn insert_new_says_whether_the_value_was_stored() {
    let store = ValueStore::new();
    let bytes = target(1);
    let first = store.insert_new(KIND, Fingerprint::of(&bytes), &bytes);
    let second = store.insert_new(KIND, Fingerprint::of(&bytes), &bytes);
    assert!(first.is_new);
    assert!(!second.is_new);
    assert_eq!(first.value, second.value);
}

/// A live value that releases another value when it's dropped, as a
/// value holding stored parts does.
struct ReleasesOnDrop {
    store: Arc<ValueStore>,
    held: ValueRef,
}

impl Drop for ReleasesOnDrop {
    fn drop(&mut self) {
        self.store.release(self.held);
    }
}

#[test]
fn a_freed_value_can_release_values_in_its_own_shard() {
    let store = Arc::new(ValueStore::new());
    let held = insert(&store, b"held");
    let fingerprint = (0u32..)
        .map(|i| Fingerprint::of(&i.to_le_bytes()))
        .find(|fingerprint| shard_of(fingerprint) == held.shard())
        .unwrap();
    let holder = ReleasesOnDrop { store: store.clone(), held };
    let live = store.insert_live(KIND, fingerprint, Arc::new(holder), 8);
    let (done, finished) = std::sync::mpsc::channel();
    let releasing = store.clone();
    std::thread::spawn(move || {
        releasing.release(live);
        let _ = done.send(());
    });
    let released = finished.recv_timeout(std::time::Duration::from_secs(10));
    assert!(released.is_ok(), "releasing the value deadlocked");
    assert!(store.read(held).is_err());
}

#[test]
fn release_and_take_returns_the_freed_value() {
    let store = ValueStore::new();
    let value = insert(&store, &target(7));
    store.retain(value);
    assert!(store.release_and_take(value).unwrap().is_none());
    let Some(Stored::Bytes(bytes)) = store.release_and_take(value).unwrap() else {
        panic!("expected the freed bytes");
    };
    assert_eq!(bytes, target(7));
}

#[test]
fn checkpoint_then_restore_keeps_values_and_refs() {
    let dir = tempfile::tempdir().unwrap();
    let store = ValueStore::new();
    let values: Vec<ValueRef> = (0..300).map(|i| insert(&store, &target(i))).collect();
    let small = insert(&store, b"tiny");
    let saving: Vec<(ValueRef, u32)> = values.iter().chain([&small]).map(|value| (*value, 1)).collect();
    let image = store.checkpoint(dir.path(), &saving).unwrap();
    assert_eq!(store.stats().page_live_bytes, 0, "saved values leave memory");
    assert_eq!(read_bytes(&store, values[7]), target(7));

    let decoded: StoreImage = skycodec::from_bytes(&skycodec::to_bytes(&image)).unwrap();
    assert_eq!(decoded, image);
    let restored = ValueStore::restore(dir.path(), &decoded).unwrap();
    for (i, value) in values.iter().enumerate() {
        assert_eq!(read_bytes(&restored, *value), target(i));
    }
    assert_eq!(read_bytes(&restored, small), b"tiny");
    assert_eq!(insert(&restored, &target(3)), values[3], "inserting a saved value finds it");
}

#[test]
fn live_values_are_restored_absent_and_refilled() {
    let dir = tempfile::tempdir().unwrap();
    let store = ValueStore::new();
    let fingerprint = Fingerprint::of(b"live");
    let live = store.insert_live(KIND, fingerprint, Arc::new(5u32), 8);
    let image = store.checkpoint(dir.path(), &[(live, 1)]).unwrap();
    let restored = ValueStore::restore(dir.path(), &image).unwrap();
    assert!(restored.is_absent(live));
    assert!(restored.read(live).is_err());
    assert_eq!(restored.insert_live(KIND, fingerprint, Arc::new(5u32), 8), live);
    assert!(!restored.is_absent(live));
    assert!(matches!(restored.read(live), Ok(Stored::Live(_))));
}

#[test]
fn sparse_segments_are_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let store = ValueStore::new();
    let values: Vec<ValueRef> = (0..200).map(|i| insert(&store, &target(i))).collect();
    let all: Vec<(ValueRef, u32)> = values.iter().map(|value| (*value, 1)).collect();
    let first = store.checkpoint(dir.path(), &all).unwrap();
    for value in &values[10..] {
        store.release(*value);
    }
    let kept: Vec<(ValueRef, u32)> = values[..10].iter().map(|value| (*value, 1)).collect();
    let second = store.checkpoint(dir.path(), &kept).unwrap();
    assert_ne!(first.segments, second.segments);
    assert!(second.disk_bytes() < first.disk_bytes() / 4);
    remove_unused_segments(dir.path(), &second.segments).unwrap();
    let restored = ValueStore::restore(dir.path(), &second).unwrap();
    for (i, value) in values[..10].iter().enumerate() {
        assert_eq!(read_bytes(&restored, *value), target(i));
    }
}

#[test]
fn a_damaged_segment_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = ValueStore::new();
    let value = insert(&store, &target(1));
    let image = store.checkpoint(dir.path(), &[(value, 1)]).unwrap();
    let path = dir.path().join(format!("{:08}.seg", image.segments[0].id));
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    assert!(ValueStore::restore(dir.path(), &image).is_err());
}
