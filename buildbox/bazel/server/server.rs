//! A long-lived Bazel server shared by every workspace on the machine.
//!
//! Each workspace gets a build graph of its own, but every graph belongs to
//! one [`Engine`], so workspaces with the same files, such as worktrees of
//! one repository, share the values computed for them. Commands run
//! concurrently, in the same workspace or different ones; there is no
//! server lock.

mod build;
mod build_id;
mod command;
mod commands;
mod help;
mod memory;
mod output;
mod repos;
mod run;
mod runfiles;
mod running;
mod service;
mod test;
mod top_level;
mod workspace;
mod written;

pub use build_id::build_id;
pub use command::{Invocation, PRODUCT, VERSION, exit};
pub use running::{RunningCommand, RunningCommands};
pub use memory::configure_allocator;
pub use output::Output;
pub use service::BazelService;
pub use workspace::{Workspace, Workspaces, find_root};

use bazel_bzlmod::{BzlmodConfig, BzlmodRepositories};
use bazel_execution::{Backend, LocalActionCache, SourceDigests};
use persist::PersistedMap;
use skycodec::{Fingerprint, Fingerprinter};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::Notify;
use bazel_loading::{BuiltinsDir, Repositories};
use skyframe::{Engine, EngineConfig, Globals, Persistence};
use std::path::PathBuf;
use std::sync::Arc;

/// Where the server keeps what it fetches and builds.
#[derive(Clone)]
pub struct ServerConfig {
    pub bazel_dir: PathBuf,
    /// Where downloads are kept, which may be shared with other servers.
    pub repository_cache: PathBuf,
    pub home: Option<PathBuf>,
    /// Where actions run, or `None` if this server can't build.
    pub backend: Option<Arc<Backend>>,
}

/// The server's state: the shared engine, and each workspace's graph.
pub struct BazelServer {
    config: ServerConfig,
    workspaces: Workspaces,
    saved: Arc<SavedState>,
    /// The commands running; the engine is saved when none is.
    commands: Arc<RunningCommands>,
    started: Instant,
    shutdown: Shutdown,
}

/// Whether the server has been asked to stop.
#[derive(Default)]
struct Shutdown {
    requested: AtomicBool,
    notify: Notify,
}

/// Above this many bytes of saved values, the engine keeps only each key's
/// newest trace when it's saved.
const MAX_SAVED_ENGINE_BYTES: u64 = 2 << 30;

/// What the server keeps across restarts, besides the CAS and action
/// cache: small maps that save work, bounded since all of it can be
/// recomputed.
struct SavedState {
    source_digests: Arc<SourceDigests>,
    local_actions: Arc<LocalActionCache>,
}

/// The most source files whose digests are kept: about 100 bytes each.
const MAX_SOURCE_DIGESTS: usize = 500_000;

/// The most action results kept by their cheap key: a few hundred bytes
/// each.
const MAX_LOCAL_ACTIONS: usize = 200_000;

impl SavedState {
    fn open(dir: &Path) -> SavedState {
        SavedState {
            source_digests: Arc::new(SourceDigests(PersistedMap::open(dir.join("source-digests"), MAX_SOURCE_DIGESTS))),
            local_actions: Arc::new(LocalActionCache(PersistedMap::open(dir.join("local-actions"), MAX_LOCAL_ACTIONS))),
        }
    }

    /// Save what changed. Blocks on the filesystem.
    fn save(&self) {
        for (name, map) in [("source digests", &self.source_digests.0), ("local action results", &self.local_actions.0)] {
            if let Err(error) = map.save() {
                tracing::warn!("couldn't save {name}: {error}");
            }
        }
    }
}

/// The engine saved in `dir` by this build of bb, or a new one. Opened
/// before anything else is interned, so the saved ids are free.
fn open_engine(dir: &Path) -> Engine {
    let persistence = Persistence {
        dir: dir.to_path_buf(),
        version: build_identity(),
        max_disk_bytes: MAX_SAVED_ENGINE_BYTES,
    };
    Engine::open(engine_config(), persistence)
}

/// Identifies the running build of bb: a fingerprint of its executable's
/// contents, so a copy of the same build is the same build. About a quarter
/// of a second for bb's size, once per start.
fn build_identity() -> Fingerprint {
    let mut fingerprinter = Fingerprinter::new();
    fingerprinter.bytes(env!("CARGO_PKG_VERSION").as_bytes());
    let hashed = std::env::current_exe().and_then(std::fs::File::open).and_then(|mut file| {
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                return Ok(());
            }
            fingerprinter.bytes(&buffer[..read]);
        }
    });
    if let Err(error) = hashed {
        tracing::warn!("couldn't read bb's executable, so its saved engine won't be reused: {error}");
        fingerprinter.u64(std::process::id() as u64);
    }
    fingerprinter.finish()
}

/// Page every analyzed target's providers out of memory, while the server
/// is idle. They're read back when next used.
fn page_out_analysis(engine: &Engine) {
    let started = std::time::Instant::now();
    let targets = engine.live_values::<bazel_analysis::ConfiguredTargetKey>();
    match bazel_analysis::paging::page_out(targets.iter().map(|target| target.paged_value())) {
        Ok(0) => {}
        Ok(paged) => tracing::info!("paged {paged} analyzed targets out of memory in {:?}", started.elapsed()),
        Err(error) => tracing::warn!("couldn't page analysis out of memory: {error:#}"),
    }
}

/// Save the engine, logging what was saved.
fn checkpoint(engine: &Engine) {
    let started = std::time::Instant::now();
    match engine.checkpoint() {
        Ok(Some(stats)) => tracing::info!(
            "saved {} keys, {} traces and {} values ({} bytes of segments, {} of manifest) in {:?}",
            stats.keys,
            stats.traces,
            stats.values,
            stats.segment_bytes,
            stats.manifest_bytes,
            started.elapsed()
        ),
        Ok(None) => {}
        Err(error) => tracing::warn!("couldn't save the engine: {error}"),
    }
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
        let engine = open_engine(&config.bazel_dir.join("engine"));
        if let Err(error) = bazel_analysis::paging::set_up(&config.bazel_dir.join("paging")) {
            tracing::warn!("analysis won't be paged out of memory: {error}");
        }
        bazel_analysis::cc::register();
        let bazel_tools = repos::find_bazel_tools(config.home.as_deref(), Some(bazel_starlark::BAZEL_VERSION));
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
        let saved = Arc::new(SavedState::open(&config.bazel_dir.join("state")));
        let mut services = Globals::new()
            .with(Arc::new(Repositories(Arc::new(BzlmodRepositories))))
            .with(Arc::new(BzlmodConfig::new(config.bazel_dir.clone(), config.repository_cache.clone(), bazel_tools)))
            .with(Arc::new(BuiltinsDir(builtins)))
            .with(saved.source_digests.clone())
            .with(saved.local_actions.clone());
        if let Some(backend) = &config.backend {
            services = services.with(backend.clone());
        }
        let workspaces = Workspaces::new(engine, services);
        let commands = RunningCommands::new(after_command(saved.clone(), workspaces.engine().clone()));
        BazelServer {
            config,
            workspaces,
            saved,
            commands,
            started: Instant::now(),
            shutdown: Shutdown::default(),
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
    /// code. A command that's cancelled stops where it is, as its work is
    /// dropped, and ends as an interrupted command does.
    pub async fn run(&self, invocation: Invocation, out: &Output) -> i32 {
        let registration = self.commands.start(invocation.args.clone(), invocation.working_directory.clone());
        let command = registration.command().clone();
        out.started(&command.id).await;
        tokio::select! {
            code = command::run(self, invocation, out) => code,
            () = command.cancelled() => command::interrupted(&command, out).await,
        }
    }

    pub fn commands(&self) -> &Arc<RunningCommands> {
        &self.commands
    }

    /// How long the server has been running.
    pub fn uptime(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// Ask the server to stop: once its commands have finished, or at once,
    /// cancelling them. It runs no more commands.
    pub fn request_shutdown(&self, cancel_commands: bool) {
        self.shutdown.requested.store(true, Ordering::SeqCst);
        if cancel_commands {
            self.commands.cancel_all();
        }
        self.shutdown.notify.notify_one();
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutdown.requested.load(Ordering::SeqCst)
    }

    /// Wait until the server is asked to stop.
    pub async fn shutdown_requested(&self) {
        self.shutdown.notify.notified().await;
    }

    /// Save the engine now, as when the server stops. Blocks on the
    /// filesystem.
    pub fn save(&self) {
        self.saved.save();
        checkpoint(self.workspaces.engine());
    }
}

/// What to do after each command: save what changed, and once no command
/// is running, the engine, paging analysis out; then return freed memory.
fn after_command(saved: Arc<SavedState>, engine: Engine) -> impl Fn(bool) + Send + Sync + 'static {
    move |idle| {
        let saved = saved.clone();
        let engine = idle.then(|| engine.clone());
        tokio::task::spawn_blocking(move || {
            saved.save();
            if let Some(engine) = engine {
                checkpoint(&engine);
                page_out_analysis(&engine);
            }
            memory::release_free_memory();
        });
    }
}
