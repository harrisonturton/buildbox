//! Reaching the build graph from Starlark code running on a blocking
//! thread, as repository rules and module extensions do.

use crate::fetch_repo::RepoFetchKey;
use bazel_fs::{FileContentKey, Root, RootedPath, Workspace};
use bazel_label::{Label, RepoName};
use intern::PathId;
use skyframe::{Ctx, Key};
use std::path::PathBuf;
use std::sync::Arc;

/// The key computing a repository rule or extension, which what it reads
/// is recorded against, and the runtime to wait on the graph with.
#[derive(Clone)]
pub struct GraphAccess {
    ctx: Ctx,
    runtime: tokio::runtime::Handle,
}

impl GraphAccess {
    pub fn new(ctx: Ctx) -> GraphAccess {
        GraphAccess {
            ctx,
            runtime: tokio::runtime::Handle::current(),
        }
    }

    /// Compute a key, blocking until it's done. Must be called from a
    /// blocking thread.
    pub fn compute<K: Key>(&self, key: &K) -> anyhow::Result<Arc<K::Value>> {
        Ok(self.runtime.block_on(self.ctx.compute(key))?)
    }

    pub fn workspace_root(&self) -> anyhow::Result<PathBuf> {
        Ok(self.ctx.global::<Workspace>()?.root.clone())
    }

    /// The directory a repository is in, fetching it if needed.
    pub fn repo_dir(&self, repo: RepoName) -> anyhow::Result<PathBuf> {
        if repo.is_main() {
            return self.workspace_root();
        }
        Ok(self.compute(&RepoFetchKey(repo))?.path())
    }

    /// The file a label names. Files of the main repository are read
    /// through the graph, so a change to them is a change to what depends on
    /// them.
    pub fn label_path(&self, label: Label) -> anyhow::Result<PathBuf> {
        let relative = label.package.path.join_path(PathId::parse(label.name.as_str())?);
        if label.repo().is_main() {
            self.compute(&FileContentKey(RootedPath::new(Root::Workspace, relative)))?;
        }
        let dir = self.repo_dir(label.repo())?;
        Ok(if relative.is_root() { dir } else { dir.join(relative.to_string()) })
    }
}
