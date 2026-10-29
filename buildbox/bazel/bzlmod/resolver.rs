//! Serving repository roots and mappings to loading.

use crate::fetch_repo::RepoFetchKey;
use crate::resolve::DepGraphKey;
use bazel_fs::Root;
use bazel_label::{RepoMappingTable, RepoName};
use bazel_loading::{Registration, RepositoryResolver};
use futures::future::BoxFuture;
use intern::Symbol;
use skyframe::{Ctx, SkyError};

/// Repositories as Bzlmod defines them.
pub struct BzlmodRepositories;

impl RepositoryResolver for BzlmodRepositories {
    /// Each module's registrations: the root module's first, then the
    /// others in the order resolution found them.
    fn registrations<'a>(&'a self, ctx: &'a Ctx) -> BoxFuture<'a, Result<Vec<Registration>, SkyError>> {
        Box::pin(async move {
            let graph = ctx.compute(&DepGraphKey).await?;
            let registrations = graph
                .modules
                .iter()
                .map(|module| Registration {
                    repo: module.repo,
                    toolchains: module.file.toolchains.clone(),
                    execution_platforms: module.file.execution_platforms.clone(),
                })
                .collect();
            Ok(registrations)
        })
    }

    fn root<'a>(&'a self, ctx: &'a Ctx, repo: RepoName) -> BoxFuture<'a, Result<Root, SkyError>> {
        Box::pin(async move {
            if repo.is_main() {
                return Ok(Root::Workspace);
            }
            let fetched = ctx.compute(&RepoFetchKey(repo)).await?;
            Ok(Root::Dir(fetched.dir))
        })
    }

    fn mapping<'a>(&'a self, ctx: &'a Ctx, repo: RepoName) -> BoxFuture<'a, Result<RepoMappingTable, SkyError>> {
        Box::pin(async move {
            if repo.as_str() == "local_config_platform" {
                let entries = vec![
                    (Symbol::intern("platforms"), RepoName::new("platforms")),
                    (Symbol::intern("local_config_platform"), repo),
                ];
                return Ok(RepoMappingTable::new(repo, entries));
            }
            let graph = ctx.compute(&DepGraphKey).await?;
            if let Some((index, _)) = graph.module(repo) {
                return Ok(graph.module_mapping(index));
            }
            if let Some((module, _)) = graph.repo_rule_of(repo) {
                let mut mapping = graph.module_mapping(module);
                mapping.repo = repo;
                return Ok(mapping);
            }
            if let Some((extension, _)) = graph.extension_of(repo) {
                return crate::extension::extension_mapping(ctx, &graph, extension, repo).await;
            }
            Err(SkyError::new(format!("no repository is visible as '@@{repo}'")))
        })
    }

    fn module_repo<'a>(&'a self, ctx: &'a Ctx, name: &'a str, newer_than: Option<&'a str>) -> BoxFuture<'a, Result<Option<RepoName>, SkyError>> {
        Box::pin(async move {
            let graph = ctx.compute(&DepGraphKey).await?;
            let version = |text: &str| crate::version::Version::parse(text).ok();
            let highest = graph.modules.iter().filter(|module| module.name == name).max_by_key(|module| version(&module.version));
            let Some(highest) = highest else {
                return Ok(None);
            };
            let newer = match newer_than.and_then(version) {
                Some(minimum) => version(&highest.version).is_none_or(|found| found > minimum),
                None => true,
            };
            Ok(newer.then_some(highest.repo))
        })
    }

    fn module<'a>(&'a self, ctx: &'a Ctx, repo: RepoName) -> BoxFuture<'a, Result<Option<(String, String)>, SkyError>> {
        Box::pin(async move {
            let graph = ctx.compute(&DepGraphKey).await?;
            let owner = match graph.module(repo) {
                Some((index, _)) => Some(index),
                None => graph
                    .extension_of(repo)
                    .and_then(|(extension, _)| graph.module(graph.extensions[extension].bzl.repo()))
                    .map(|(index, _)| index),
            };
            Ok(owner.map(|index| {
                let module = &graph.modules[index];
                (module.name.clone(), module.version.clone())
            }))
        })
    }
}
