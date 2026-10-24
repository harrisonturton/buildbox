//! A long-lived Bazel server shared by every workspace on the machine.
//!
//! Each workspace gets a build graph of its own, but every graph belongs to
//! one [`Engine`], so workspaces with the same files, such as worktrees of
//! one repository, share the values computed for them. Commands run
//! concurrently, in the same workspace or different ones; there is no
//! server lock.

mod build;
mod command;
mod commands;
mod memory;
mod output;
mod repos;
mod run;
mod runfiles;
mod service;
mod test;
mod workspace;

pub use command::{BAZEL_VERSION, Invocation, exit};
pub use memory::configure_allocator;
pub use output::Output;
pub use service::BazelService;
pub use workspace::{Workspace, Workspaces, find_root};

use bazel_bzlmod::{BzlmodConfig, BzlmodRepositories};
use bazel_execution::Backend;
use bazel_loading::{BuiltinsDir, Repositories};
use skyframe::{Engine, EngineConfig, Globals};
use std::path::PathBuf;
use std::sync::Arc;

/// Where the server keeps what it fetches and builds.
#[derive(Clone)]
pub struct ServerConfig {
    pub bazel_dir: PathBuf,
    pub home: Option<PathBuf>,
    /// Where actions run, or `None` if this server can't build.
    pub backend: Option<Arc<Backend>>,
}

/// The server's state: the shared engine, and each workspace's graph.
pub struct BazelServer {
    config: ServerConfig,
    workspaces: Workspaces,
}

/// The engine's configuration. `BB_CACHE_MB` sets how many megabytes of
/// decoded values it keeps, for tuning.
fn engine_config() -> EngineConfig {
    let mut config = EngineConfig::default();
    if let Some(megabytes) = std::env::var("BB_CACHE_MB").ok().and_then(|value| value.parse::<usize>().ok()) {
        config.cache_bytes = megabytes << 20;
    }
    config
}

impl BazelServer {
    pub fn new(config: ServerConfig) -> BazelServer {
        bazel_analysis::cc::register();
        let bazel_tools = repos::find_bazel_tools(config.home.as_deref(), Some(BAZEL_VERSION));
        match &bazel_tools {
            Some(dir) => tracing::info!("using @bazel_tools from {}", dir.display()),
            None => tracing::warn!("no @bazel_tools found; set BB_BAZEL_TOOLS"),
        }
        let builtins = bazel_tools
            .as_deref()
            .and_then(|tools| repos::find_builtins(tools, &config.bazel_dir.join("builtins")));
        match &builtins {
            Some(dir) => tracing::info!("using @_builtins from {}", dir.display()),
            None => tracing::warn!("no @_builtins found; native rules such as cc_library are unavailable"),
        }
        let mut services = Globals::new()
            .with(Arc::new(Repositories(Arc::new(BzlmodRepositories))))
            .with(Arc::new(BzlmodConfig::new(config.bazel_dir.clone(), bazel_tools)))
            .with(Arc::new(BuiltinsDir(builtins)));
        if let Some(backend) = &config.backend {
            services = services.with(backend.clone());
        }
        let engine = Engine::new(engine_config());
        BazelServer {
            config,
            workspaces: Workspaces::new(engine, services),
        }
    }

    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Where this server runs actions.
    pub fn backend(&self) -> Option<&Arc<Backend>> {
        self.config.backend.as_ref()
    }

    pub fn workspaces(&self) -> &Workspaces {
        &self.workspaces
    }

    /// Run a command line, writing its output to `out`, and return its exit
    /// code.
    pub async fn run(&self, invocation: Invocation, out: &Output) -> i32 {
        let code = command::run(self, invocation, out).await;
        tokio::task::spawn_blocking(memory::release_free_memory);
        code
    }
}
