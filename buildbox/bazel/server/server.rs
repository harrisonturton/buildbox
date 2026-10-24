//! A long-lived Bazel server shared by every workspace on the machine.
//!
//! Each workspace gets a build graph of its own, but every graph belongs to
//! one [`Engine`], so workspaces with the same files, such as worktrees of
//! one repository, share the values computed for them. Commands run
//! concurrently, in the same workspace or different ones; there is no
//! server lock.

mod command;
mod commands;
mod output;
mod repos;
mod service;
mod workspace;

pub use command::{BAZEL_VERSION, Invocation, exit};
pub use output::Output;
pub use service::BazelService;
pub use workspace::{Workspace, Workspaces, find_root};

use bazel_loading::{Repositories, RepositoryResolver};
use skyframe::{Engine, EngineConfig};
use std::path::PathBuf;
use std::sync::Arc;

/// Where the server keeps what it fetches and builds.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bazel_dir: PathBuf,
    pub home: Option<PathBuf>,
}

/// The server's state: the shared engine, and each workspace's graph.
pub struct BazelServer {
    config: ServerConfig,
    workspaces: Workspaces,
}

impl BazelServer {
    pub fn new(config: ServerConfig) -> BazelServer {
        let bazel_tools = repos::find_bazel_tools(config.home.as_deref(), None);
        match &bazel_tools {
            Some(dir) => tracing::info!("using @bazel_tools from {}", dir.display()),
            None => tracing::warn!("no @bazel_tools found; set BB_BAZEL_TOOLS"),
        }
        let resolver: Arc<dyn RepositoryResolver> = Arc::new(repos::BuiltinRepositories { bazel_tools });
        BazelServer::with_repositories(config, resolver)
    }

    /// A server whose repositories come from `resolver`.
    pub fn with_repositories(config: ServerConfig, resolver: Arc<dyn RepositoryResolver>) -> BazelServer {
        let engine = Engine::new(EngineConfig::default());
        let repositories = Arc::new(Repositories(resolver));
        BazelServer {
            config,
            workspaces: Workspaces::new(engine, repositories),
        }
    }

    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    pub fn workspaces(&self) -> &Workspaces {
        &self.workspaces
    }

    /// Run a command line, writing its output to `out`, and return its exit
    /// code.
    pub async fn run(&self, invocation: Invocation, out: &Output) -> i32 {
        command::run(self, invocation, out).await
    }
}
