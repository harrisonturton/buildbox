//! The workspaces the server knows, each with its own graph.

use bazel_execution::SourceDigestKey;
use bazel_fs::{DirListingKey, FileChanges, FileContentKey, FileStateKey};
use skyframe::{Engine, Globals, Graph};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The files whose presence marks a workspace's root directory.
const WORKSPACE_FILES: [&str; 4] = ["MODULE.bazel", "REPO.bazel", "WORKSPACE.bazel", "WORKSPACE"];

/// Find the workspace a directory is in: the nearest directory, itself or
/// above, with a `MODULE.bazel` or `WORKSPACE` file.
pub fn find_root(dir: &Path) -> Option<PathBuf> {
    let mut current = Some(dir);
    while let Some(candidate) = current {
        if WORKSPACE_FILES.iter().any(|name| candidate.join(name).is_file()) {
            return Some(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    None
}

/// One workspace: its graph, and the files the graph has read.
pub struct Workspace {
    pub root: PathBuf,
    pub graph: Graph,
    pub changes: Arc<FileChanges>,
}

impl Workspace {
    /// Invalidate the files that changed since the graph read them.
    pub async fn refresh(&self) -> usize {
        let changed = self.changes.changed().await;
        if changed.is_empty() {
            return 0;
        }
        tracing::info!(files = changed.len(), "invalidating changed files");
        let states: Vec<FileStateKey> = changed.iter().map(|path| FileStateKey(*path)).collect();
        let listings: Vec<DirListingKey> = changed.iter().map(|path| DirListingKey(*path)).collect();
        let contents: Vec<FileContentKey> = changed.iter().map(|path| FileContentKey(*path)).collect();
        let digests: Vec<SourceDigestKey> = changed.iter().map(|path| SourceDigestKey(*path)).collect();
        self.graph.invalidate(&states).await;
        self.graph.invalidate(&listings).await;
        self.graph.invalidate(&contents).await;
        self.graph.invalidate(&digests).await;
        changed.len()
    }
}

/// Every workspace commands have run in, by root. Their graphs share one
/// engine, so they share what they compute.
pub struct Workspaces {
    engine: Engine,
    /// The services every workspace's graph has, such as where repositories
    /// come from.
    services: Globals,
    workspaces: Mutex<HashMap<PathBuf, Arc<Workspace>>>,
}

impl Workspaces {
    pub fn new(engine: Engine, services: Globals) -> Workspaces {
        Workspaces {
            engine,
            services,
            workspaces: Mutex::new(HashMap::new()),
        }
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// The workspace rooted at `root`, creating its graph if needed.
    pub fn get(&self, root: &Path) -> Arc<Workspace> {
        let mut workspaces = self.workspaces.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(workspace) = workspaces.get(root) {
            return workspace.clone();
        }
        let changes = FileChanges::new();
        let globals = self.services.clone().with(bazel_fs::workspace(root)).with(changes.clone());
        let workspace = Arc::new(Workspace {
            root: root.to_path_buf(),
            graph: self.engine.graph(globals),
            changes,
        });
        workspaces.insert(root.to_path_buf(), workspace.clone());
        workspace
    }

    /// Forget a workspace's graph, freeing what only it used.
    pub fn remove(&self, root: &Path) -> bool {
        let mut workspaces = self.workspaces.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        workspaces.remove(root).is_some()
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        let workspaces = self.workspaces.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut roots: Vec<PathBuf> = workspaces.keys().cloned().collect();
        roots.sort();
        roots
    }
}
