//! Loading: evaluating BUILD and `.bzl` files into packages, as keys of the
//! build graph.
//!
//! Where repositories are, and how they see each other, comes from a
//! [`Repositories`] service, which a higher layer (such as Bzlmod)
//! provides. Everything else is computed from files.

mod bzl;
mod package;
mod recursive;
mod rule_class;

pub use bzl::{BzlLoadKey, BzlModule};
pub use package::{LoadedPackage, PackageKey, PackageLookup, PackageLookupKey};
pub use recursive::{PackagesUnderDirectory, PackagesUnderDirectoryKey, packages_beneath};
pub use rule_class::{RuleClassKey, RuleClassValue, rule_class};

use bazel_fs::Root;
use bazel_label::{RepoMappingTable, RepoName};
use futures::future::BoxFuture;
use skycodec::{Decode, Encode};
use skyframe::{encoded_value, Ctx, Key, SkyError};
use std::sync::Arc;

/// Where repositories are, and how each sees the others.
///
/// Implementations compute their answers through `ctx`, so the keys that
/// ask them depend on whatever the answers were computed from.
pub trait RepositoryResolver: Send + Sync + 'static {
    fn root<'a>(&'a self, ctx: &'a Ctx, repo: RepoName) -> BoxFuture<'a, Result<Root, SkyError>>;
    fn mapping<'a>(&'a self, ctx: &'a Ctx, repo: RepoName) -> BoxFuture<'a, Result<RepoMappingTable, SkyError>>;
}

/// The graph service holding the [`RepositoryResolver`].
pub struct Repositories(pub Arc<dyn RepositoryResolver>);

/// The directory a repository's files are in.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct RepoRoot(pub Root);

encoded_value!(RepoRoot);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RepoRootKey(pub RepoName);

impl Key for RepoRootKey {
    type Value = RepoRoot;
    const KIND: &'static str = "REPOSITORY_DIRECTORY";

    async fn compute(&self, ctx: &Ctx) -> Result<RepoRoot, SkyError> {
        let repositories = ctx.global::<Repositories>()?;
        Ok(RepoRoot(repositories.0.root(ctx, self.0).await?))
    }
}

/// How a repository sees the others.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct RepoMappingValue(pub RepoMappingTable);

encoded_value!(RepoMappingValue);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RepoMappingKey(pub RepoName);

impl Key for RepoMappingKey {
    type Value = RepoMappingValue;
    const KIND: &'static str = "REPOSITORY_MAPPING";

    async fn compute(&self, ctx: &Ctx) -> Result<RepoMappingValue, SkyError> {
        let repositories = ctx.global::<Repositories>()?;
        Ok(RepoMappingValue(repositories.0.mapping(ctx, self.0).await?))
    }
}

/// Run CPU-heavy work, such as evaluating Starlark, on a blocking thread.
pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, SkyError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| SkyError::transient(format!("evaluation task failed: {error}")))
}
