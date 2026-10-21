//! Behaviour every [`ActionCache`] implementation must have.
//!
//! Each implementation's tests call [`run_all`] with a function that creates
//! an empty cache.

use actioncache::ActionCache;
use cas::Digest;
use remote_execution_proto::build::bazel::remote::execution::v2::ActionResult;
use std::future::Future;
use std::path::PathBuf;

/// Run every check against a fresh cache from `new_cache`, which is given an
/// empty directory it may use.
pub async fn run_all<F, Fut>(new_cache: F)
where
    F: Fn(PathBuf) -> Fut,
    Fut: Future<Output = Box<dyn ActionCache>>,
{
    check("get_of_absent_entry_is_none", &new_cache, get_of_absent_entry_is_none).await;
    check("put_then_get_round_trips", &new_cache, put_then_get_round_trips).await;
    check("put_replaces_previous_entry", &new_cache, put_replaces_previous_entry).await;
    check("entries_are_keyed_by_action", &new_cache, entries_are_keyed_by_action).await;
    check("usage_counts_entries", &new_cache, usage_counts_entries).await;
}

async fn check<F, Fut, T, TFut>(name: &str, new_cache: &F, test: T)
where
    F: Fn(PathBuf) -> Fut,
    Fut: Future<Output = Box<dyn ActionCache>>,
    T: FnOnce(Box<dyn ActionCache>) -> TFut,
    TFut: Future<Output = ()>,
{
    // Printed so a failure can be traced to its check.
    println!("action cache conformance: {name}");
    let scratch = tempfile::tempdir().unwrap();
    let cache = new_cache(scratch.path().join("ac")).await;
    test(cache).await;
}

fn result_with_exit_code(exit_code: i32) -> ActionResult {
    ActionResult {
        exit_code,
        stdout_digest: Some(Digest::of_bytes(b"stdout").to_proto()),
        ..Default::default()
    }
}

async fn get_of_absent_entry_is_none(cache: Box<dyn ActionCache>) {
    let action = Digest::of_bytes(b"action");

    assert_eq!(cache.get(&action).await.unwrap(), None);
}

async fn put_then_get_round_trips(cache: Box<dyn ActionCache>) {
    let action = Digest::of_bytes(b"action");
    let result = result_with_exit_code(0);

    cache.put(&action, &result).await.unwrap();

    assert_eq!(cache.get(&action).await.unwrap(), Some(result));
}

async fn put_replaces_previous_entry(cache: Box<dyn ActionCache>) {
    let action = Digest::of_bytes(b"action");

    cache.put(&action, &result_with_exit_code(1)).await.unwrap();
    cache.put(&action, &result_with_exit_code(2)).await.unwrap();

    assert_eq!(cache.get(&action).await.unwrap(), Some(result_with_exit_code(2)));
}

async fn entries_are_keyed_by_action(cache: Box<dyn ActionCache>) {
    let first = Digest::of_bytes(b"first");
    let second = Digest::of_bytes(b"second");

    cache.put(&first, &result_with_exit_code(1)).await.unwrap();

    assert_eq!(cache.get(&second).await.unwrap(), None);
}

async fn usage_counts_entries(cache: Box<dyn ActionCache>) {
    assert_eq!(cache.usage().await.unwrap().entries, 0);

    cache.put(&Digest::of_bytes(b"a"), &result_with_exit_code(0)).await.unwrap();
    cache.put(&Digest::of_bytes(b"a"), &result_with_exit_code(0)).await.unwrap();
    cache.put(&Digest::of_bytes(b"b"), &result_with_exit_code(0)).await.unwrap();

    let usage = cache.usage().await.unwrap();
    assert_eq!(usage.entries, 2);
    assert!(usage.bytes > 0);
}
