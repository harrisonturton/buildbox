//! An [`ActionCache`] that keeps entries in memory.

use actioncache::{ActionCache, ActionCacheError, Usage};
use async_trait::async_trait;
use cas::Digest;
use prost::Message;
use remote_execution_proto::build::bazel::remote::execution::v2::ActionResult;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

type Entries = HashMap<Digest, ActionResult>;

/// Keeps action results in memory, for tests and short-lived servers.
#[derive(Debug, Clone, Default)]
pub struct MemActionCache {
    entries: Arc<Mutex<Entries>>,
}

impl MemActionCache {
    pub fn new() -> MemActionCache {
        MemActionCache::default()
    }
}

#[async_trait]
impl ActionCache for MemActionCache {
    async fn get(&self, action: &Digest) -> Result<Option<ActionResult>, ActionCacheError> {
        Ok(lock(&self.entries).get(action).cloned())
    }

    async fn put(&self, action: &Digest, result: &ActionResult) -> Result<(), ActionCacheError> {
        lock(&self.entries).insert(action.clone(), result.clone());
        Ok(())
    }

    async fn usage(&self) -> Result<Usage, ActionCacheError> {
        let entries = lock(&self.entries);
        let mut usage = Usage::default();
        for result in entries.values() {
            usage.entries += 1;
            usage.bytes += result.encoded_len() as u64;
        }
        Ok(usage)
    }
}

/// The map is never left inconsistent, so a panic while it was held doesn't
/// make it unusable.
fn lock(entries: &Mutex<Entries>) -> MutexGuard<'_, Entries> {
    entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn conformance() {
        actioncache_conformance::run_all(|_dir| async move {
            Box::new(MemActionCache::new()) as Box<dyn ActionCache>
        })
        .await;
    }
}
