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
