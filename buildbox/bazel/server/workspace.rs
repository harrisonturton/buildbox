//! The workspaces the server knows, each with its own graph.

use crate::written::Written;
use bazel_analysis::workspace_status::{BuildStatus, StatusValues, WorkspaceStatusKey};
use bazel_bzlmod::command_overrides::{CommandOverrides, CommandOverridesKey, CommandOverridesService};
use bazel_bzlmod::config::CommandCredentials;
use bazel_bzlmod::registry::{CommandRegistries, Registries, RegistriesKey};
use bazel_bzlmod::repo_env::{ClientEnv, RepoEnvKey, RepoEnvVarKey, RepoEnvironment};
use bazel_execution::{Jobs, SourceDigestKey, TestKey};
use bazel_fs::{DirListingKey, FileChanges, FileContentKey, FileStateKey};
use bazel_loading::autoload::{AutoloadSettings, AutoloadSettingsKey, CommandAutoloads};
use bazel_progress::Progress;
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
    /// The environment repository rules see.
    pub client_env: Arc<ClientEnv>,
    /// What the status files of builds hold.
    pub status: Arc<BuildStatus>,
    /// The registries modules are resolved from.
    pub registries: Arc<CommandRegistries>,
    /// The repositories and modules the command's options replace or add.
    pub overrides: Arc<CommandOverridesService>,
    /// The credential helpers downloads authenticate with.
    pub credentials: Arc<CommandCredentials>,
    /// Which symbols are autoloaded, and where.
    pub autoloads: Arc<CommandAutoloads>,
    /// What builds wrote to the output tree.
    pub written: Arc<Written>,
    /// What the graph's evaluations are doing, for progress output.
    pub progress: Arc<Progress>,
    /// How many actions builds' `--jobs` lets run at once.
    pub jobs: Arc<Jobs>,
    /// The tests that failed, whose results only commands with
    /// `--cache_test_results=yes` reuse.
    pub failed_tests: FailedTests,
}

/// Tests whose failures a workspace's graph holds.
#[derive(Default)]
pub struct FailedTests(Mutex<Vec<TestKey>>);

impl FailedTests {
    pub fn extend(&self, tests: impl IntoIterator<Item = TestKey>) {
        let mut failed = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for test in tests {
            if !failed.contains(&test) {
                failed.push(test);
            }
        }
    }

    /// Those of `tests` that failed, which are forgotten.
    pub fn take_among(&self, tests: &[TestKey]) -> Vec<TestKey> {
        let mut failed = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let (taken, kept) = std::mem::take(&mut *failed).into_iter().partition(|test| tests.contains(test));
        *failed = kept;
        taken
    }
}

impl Workspace {
    /// Invalidate the files that changed since the graph read them, the
    /// environment, if a command gives another, and results that may not
    /// recur, such as failed actions, which Bazel runs again each build.
    pub async fn refresh(&self, environment: RepoEnvironment) -> usize {
        let retried = self.graph.invalidate_transient().await;
        if retried > 0 {
            tracing::info!(keys = retried, "computing transient results again");
        }
        let env_changes = self.client_env.set(environment);
        let mut env_keys: Vec<RepoEnvVarKey> = env_changes.names.into_iter().map(RepoEnvVarKey).collect();
        env_keys.sort_by(|a, b| a.0.cmp(&b.0));
        if !env_keys.is_empty() {
            self.graph.invalidate(&env_keys).await;
        }
        if env_changes.overrides {
            self.graph.invalidate(&[RepoEnvKey]).await;
        }
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

    /// Resolve modules from these registries, or the default if there are
    /// none, with these mirrors.
    pub async fn set_registries(&self, registries: Registries) {
        if self.registries.set(registries) {
            self.graph.invalidate(&[RegistriesKey]).await;
        }
    }

    /// Replace and add repositories and modules as these overrides say.
    pub async fn set_overrides(&self, overrides: CommandOverrides) {
        if self.overrides.set(overrides) {
            self.graph.invalidate(&[CommandOverridesKey]).await;
        }
    }

    /// Autoload symbols as these settings say.
    pub async fn set_autoloads(&self, settings: AutoloadSettings) {
        if self.autoloads.set(settings) {
            self.graph.invalidate(&[AutoloadSettingsKey]).await;
        }
    }

    /// Give builds' status files these values.
    pub async fn set_status(&self, values: StatusValues) {
        if self.status.set(values) {
            self.graph.invalidate(&[WorkspaceStatusKey]).await;
        }
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
        let client_env = ClientEnv::new();
        let status = BuildStatus::new();
        let registries = CommandRegistries::new();
        let overrides = CommandOverridesService::new();
        let credentials = CommandCredentials::new();
        let autoloads = CommandAutoloads::new();
        let progress = Progress::new();
        let jobs = Arc::new(Jobs::default());
        let globals = self
            .services
            .clone()
            .with(bazel_fs::workspace(root))
            .with(changes.clone())
            .with(client_env.clone())
            .with(status.clone())
            .with(registries.clone())
            .with(overrides.clone())
            .with(credentials.clone())
            .with(autoloads.clone())
            .with(progress.clone())
            .with(jobs.clone());
        let workspace = Arc::new(Workspace {
            root: root.to_path_buf(),
            graph: self.engine.graph(globals),
            changes,
            client_env,
            status,
            registries,
            overrides,
            credentials,
            autoloads,
            written: Arc::default(),
            progress,
            jobs,
            failed_tests: FailedTests::default(),
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
