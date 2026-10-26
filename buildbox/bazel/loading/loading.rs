//! Loading: evaluating BUILD and `.bzl` files into packages, as keys of the
//! build graph.
//!
//! Where repositories are, and how they see each other, comes from a
//! [`Repositories`] service, which a higher layer (such as Bzlmod)
//! provides. Everything else is computed from files.

mod builtins;
mod bzl;
mod package;
mod printed;
mod recursive;
mod rule_class;

pub use builtins::{BUILTINS_REPO, Builtins, BuiltinsDir, BuiltinsKey, builtins_mapping};
pub use bzl::{BzlLoadKey, BzlModule, mappings};

pub use package::{LoadedPackage, PackageKey, PackageLookup, PackageLookupKey, build_file_path, display as display_package, no_such_target, target_location};
pub use printed::{read_source, report_printed, source_path};
pub use recursive::{PackagesUnderDirectory, PackagesUnderDirectoryKey, packages_beneath};
pub use rule_class::{RuleClassKey, RuleClassValue, rule_class};

use bazel_fs::Root;
use bazel_label::{RepoMappingTable, RepoName};
use futures::future::BoxFuture;
use skycodec::{Decode, Encode};
use skyframe::{encoded_value, Ctx, Key, SkyError};
use std::sync::{Arc, LazyLock};

/// Where repositories are, and how each sees the others.
///
/// Implementations compute their answers through `ctx`, so the keys that
/// ask them depend on whatever the answers were computed from.
pub trait RepositoryResolver: Send + Sync + 'static {
    fn root<'a>(&'a self, ctx: &'a Ctx, repo: RepoName) -> BoxFuture<'a, Result<Root, SkyError>>;
    fn mapping<'a>(&'a self, ctx: &'a Ctx, repo: RepoName) -> BoxFuture<'a, Result<RepoMappingTable, SkyError>>;

    /// The name and version of the module a repository belongs to, as
    /// `module_name()` and `module_version()` give them.
    fn module<'a>(&'a self, ctx: &'a Ctx, repo: RepoName) -> BoxFuture<'a, Result<Option<(String, String)>, SkyError>> {
        let _ = (ctx, repo);
        Box::pin(async { Ok(None) })
    }

    /// The toolchains and execution platforms the build registers, in
    /// order of precedence.
    fn registrations<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Vec<Registration>, SkyError>> {
        let _ = ctx;
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// What one repository registers, as target patterns relative to it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Registration {
    pub repo: RepoName,
    pub toolchains: Vec<String>,
    pub execution_platforms: Vec<String>,
}

/// The registrations of the whole build.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Registrations(pub Vec<Registration>);

encoded_value!(Registrations);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RegistrationsKey;

impl Key for RegistrationsKey {
    type Value = Registrations;
    const KIND: &'static str = "REGISTRATIONS";

    async fn compute(&self, ctx: &Ctx) -> Result<Registrations, SkyError> {
        let repositories = ctx.global::<Repositories>()?;
        Ok(Registrations(repositories.0.registrations(ctx).await?))
    }
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
        if self.0.as_str() == builtins::BUILTINS_REPO {
            let bazel_tools = repositories.0.mapping(ctx, RepoName::new("bazel_tools")).await?;
            return Ok(RepoMappingValue(builtins::builtins_mapping(&bazel_tools)));
        }
        Ok(RepoMappingValue(repositories.0.mapping(ctx, self.0).await?))
    }
}

/// Limits how many Starlark evaluations run at once to the machine's
/// parallelism: they're CPU-bound, and each evaluator holds a megabyte of
/// stack, so running more only costs memory.
static EVALUATIONS: LazyLock<tokio::sync::Semaphore> = LazyLock::new(|| {
    let parallelism = std::thread::available_parallelism().map(usize::from).unwrap_or(4);
    tokio::sync::Semaphore::new(parallelism)
});

/// Run CPU-heavy work, such as evaluating Starlark, on a blocking thread,
/// once one of the limited evaluation slots is free.
pub async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, SkyError> {
    let _permit = EVALUATIONS
        .acquire()
        .await
        .map_err(|error| SkyError::transient(format!("evaluation slots closed: {error}")))?;
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| SkyError::transient(format!("evaluation task failed: {error}")))
}
