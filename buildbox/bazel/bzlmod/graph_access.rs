//! Reaching the build graph from Starlark code running on a blocking
//! thread, as repository rules and module extensions do.

use crate::fetch_repo::RepoFetchKey;
use crate::repo_env::RepoEnvVarKey;
use bazel_fs::{DirListingKey, EntryKind, FileContentKey, Root, RootedPath, Workspace};
use bazel_label::{Label, RepoName};
use bazel_starlark::print::Printed;
use intern::PathId;
use skyframe::{Ctx, Key};
use intern::Symbol;
use skycodec::Fingerprinter;
use std::path::{Path, PathBuf};
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

    /// Show what Starlark code printed to the user, as Bazel's `DEBUG:`
    /// messages. Must be called from a blocking thread.
    pub fn report_printed(&self, printed: &[Printed]) {
        self.runtime.block_on(bazel_loading::report_printed(&self.ctx, printed));
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

    /// An environment variable's value as repository rules see it, which
    /// what's being computed then depends on.
    pub fn env_var(&self, name: &str) -> anyhow::Result<Option<String>> {
        Ok(self.compute(&RepoEnvVarKey(name.to_string()))?.0.clone())
    }

    /// The digest of the file at `path`, `None` if there's none, which what's
    /// being computed then depends on.
    pub fn file_digest(&self, path: &Path) -> anyhow::Result<Option<String>> {
        let content = self.compute(&FileContentKey(self.rooted(path)?))?;
        Ok(content.digest.map(|digest| digest.to_string()))
    }

    /// A digest of the directory tree at `path`: its entries' names and
    /// kinds, and its files' contents, which what's being computed then
    /// depends on.
    pub fn tree_digest(&self, path: &Path) -> anyhow::Result<String> {
        let mut fingerprinter = Fingerprinter::new();
        let mut pending = vec![path.to_path_buf()];
        while let Some(dir) = pending.pop() {
            let listing = self.compute(&DirListingKey(self.rooted(&dir)?))?;
            for (name, kind) in &listing.entries {
                let child = dir.join(name.as_str());
                fingerprinter.bytes(child.display().to_string().as_bytes());
                match kind {
                    EntryKind::Directory => pending.push(child),
                    EntryKind::File => {
                        fingerprinter.bytes(self.file_digest(&child)?.unwrap_or_default().as_bytes());
                    }
                    EntryKind::Other => {
                        fingerprinter.bytes(b"other");
                    }
                }
            }
        }
        Ok(fingerprinter.finish().to_string())
    }

    /// An absolute path as the graph's file keys name it: in the
    /// workspace, or in its parent directory.
    fn rooted(&self, path: &Path) -> anyhow::Result<RootedPath> {
        let workspace = self.workspace_root()?;
        if let Ok(relative) = path.strip_prefix(&workspace) {
            let relative = relative.display().to_string();
            let relative = if relative.is_empty() { PathId::ROOT } else { PathId::parse(&relative)? };
            return Ok(RootedPath::new(Root::Workspace, relative));
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Ok(RootedPath::new(Root::Dir(Symbol::intern(&path.display().to_string())), PathId::ROOT));
        };
        let name = PathId::parse(&name.to_string_lossy())?;
        Ok(RootedPath::new(Root::Dir(Symbol::intern(&parent.display().to_string())), name))
    }
}
