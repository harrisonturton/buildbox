//! Evaluating keys from either a key's function or a session.

use crate::{Ctx, Key, Session, SkyError};
use std::future::Future;
use std::sync::Arc;

/// Something that can evaluate keys: a [`Ctx`], recording what a key's
/// function reads, or a [`Session`], at the top of an evaluation. Code that
/// works over the graph, such as a query, can be written once for both.
pub trait Compute: Send + Sync {
    fn compute<K: Key>(&self, key: &K) -> impl Future<Output = Result<Arc<K::Value>, SkyError>> + Send;

    fn compute_many<K: Key>(&self, keys: &[K]) -> impl Future<Output = Vec<Result<Arc<K::Value>, SkyError>>> + Send;
}

impl Compute for Ctx {
    async fn compute<K: Key>(&self, key: &K) -> Result<Arc<K::Value>, SkyError> {
        Ctx::compute(self, key).await
    }

    async fn compute_many<K: Key>(&self, keys: &[K]) -> Vec<Result<Arc<K::Value>, SkyError>> {
        Ctx::compute_many(self, keys).await
    }
}

impl Compute for Session {
    async fn compute<K: Key>(&self, key: &K) -> Result<Arc<K::Value>, SkyError> {
        self.evaluate(key).await
    }

    async fn compute_many<K: Key>(&self, keys: &[K]) -> Vec<Result<Arc<K::Value>, SkyError>> {
        self.evaluate_many(keys).await
    }
}
