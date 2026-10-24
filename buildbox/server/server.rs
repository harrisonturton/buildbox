//! Assembles the stores, executor and RPC services, and serves them.

use actioncache::{ActionCache, ActionCacheError};
use actioncache_fs::FsActionCache;
use admin::{AdminService, Components};
use bazel_server::{BazelServer, BazelService, ServerConfig};
use artifacts::{ArtifactError, ArtifactStore};
use artifacts_fs::FsArtifactStore;
use cas::{Cas, CasError};
use cas_fs::FsCas;
use config::{Config, ExecutionEnvironment};
use connectrpc::{ConnectRpcService, Limits, Router, Server};
use container::{ContainerError, ContainerRuntime, Network};
use container_namespaces::{Helper, NamespaceRuntime};
use executor::Executor;
use executor_container::ContainerRunner;
use executor_local::HostRunner;
use executor_sandboxed::{Runner, SandboxedExecutor};
use reapi::{Invocations, RequestTracing, INVOCATION_IDLE_TIMEOUT, MAX_MESSAGE_SIZE_BYTES};
use scheduler::Scheduler;
use std::collections::BTreeMap;
use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::oneshot;

/// How long requests may keep running after the server is asked to stop,
/// before they are stopped too.
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("failed to open the CAS: {error}")]
    Cas { error: CasError },

    #[error("failed to open the action cache: {error}")]
    ActionCache { error: ActionCacheError },

    #[error("failed to open the artifact store: {error}")]
    Artifacts { error: ArtifactError },

    #[error("environment {name:?} can't run containers: {error}")]
    Containers { name: String, error: ContainerError },

    #[error("environment {name:?}: {error}")]
    InvalidImage { name: String, error: ArtifactError },

    #[error("failed to prepare the sandbox directory {}: {error}", path.display())]
    Sandbox { path: PathBuf, error: std::io::Error },

    #[error("failed to listen on {addr}: {error}")]
    Listen {
        addr: SocketAddr,
        error: Box<dyn Error + Send + Sync>,
    },

    #[error("failed to serve on {addr}: {error}")]
    Serve {
        addr: SocketAddr,
        error: Box<dyn Error + Send + Sync>,
    },
}

/// Run the server until it receives SIGINT or SIGTERM.
///
/// On a signal the server stops accepting requests and gives running ones
/// [`SHUTDOWN_GRACE_PERIOD`] to finish. Requests still running after that,
/// and the actions they started, are stopped when the async runtime shuts
/// down.
pub async fn run(config: &Config) -> Result<(), ServerError> {
    let cas: Arc<dyn Cas> = Arc::new(open_cas(config).await?);
    let action_cache: Arc<dyn ActionCache> = Arc::new(open_action_cache(config).await?);
    let artifacts: Arc<dyn ArtifactStore> = Arc::new(open_artifacts(config, cas.clone()).await?);
    let environments = open_environments(config, cas.clone(), artifacts.clone()).await?;
    let scheduler = Arc::new(Scheduler::new(environments, config.max_concurrent_actions));

    let execution_service = reapi::ExecutionService::new(cas.clone(), action_cache.clone(), scheduler.clone());
    let action_cache_service = reapi::ActionCacheService::new(cas.clone(), action_cache.clone());
    let cas_service = reapi::ContentAddressableStorageService::new(cas.clone());
    let bytestream_service = reapi::ByteStreamService::new(cas.clone());
    let capabilities_service = reapi::CapabilitiesService::default();
    let bazel_server = Arc::new(BazelServer::new(ServerConfig {
        bazel_dir: config.bazel_dir.clone(),
        home: std::env::var_os("HOME").map(PathBuf::from),
    }));
    if std::env::var_os("BB_KEEP_STATE_ON_EXIT").is_some() {
        // For heap profiling: keep the build graphs alive at exit, so the
        // profile written then shows what they hold.
        std::mem::forget(bazel_server.clone());
    }
    let bazel_service = BazelService::new(bazel_server);
    let admin_service = AdminService::new(Components {
        cas: cas.clone(),
        cas_location: config.cas_dir.display().to_string(),
        action_cache: action_cache.clone(),
        action_cache_location: config.action_cache_dir.display().to_string(),
        artifacts: artifacts.clone(),
        artifacts_location: config.artifacts_dir.display().to_string(),
        scheduler: scheduler.clone(),
    });

    // The services speak gRPC, which Bazel uses, as well as the Connect
    // protocol and gRPC-Web.
    let router = Router::new()
        .add_service(Arc::new(execution_service))
        .add_service(Arc::new(action_cache_service))
        .add_service(Arc::new(cas_service))
        .add_service(Arc::new(bytestream_service))
        .add_service(Arc::new(capabilities_service))
        .add_service(Arc::new(admin_service))
        .add_service(Arc::new(bazel_service));
    let limits = Limits::default()
        .with_max_message_size(MAX_MESSAGE_SIZE_BYTES)
        .with_max_request_body_size(MAX_MESSAGE_SIZE_BYTES);
    let invocations = Invocations::new(INVOCATION_IDLE_TIMEOUT);
    tokio::spawn(invocations.clone().close_idle_periodically());
    let service = ConnectRpcService::new(router)
        .with_limits(limits)
        .with_interceptor(RequestTracing::new(invocations.clone()));

    let listener = Server::bind(config.listen)
        .await
        .map_err(|error| ServerError::Listen {
            addr: config.listen,
            error,
        })?;
    tracing::info!("listening on {}", config.listen);
    tracing::info!("running up to {} actions at once", config.max_concurrent_actions);

    let (stop, stopped) = oneshot::channel::<()>();
    let serve = listener.serve_with_service_and_shutdown(service, async {
        let _ = stopped.await;
    });
    tokio::pin!(serve);

    tokio::select! {
        result = &mut serve => {
            invocations.close_all();
            return serve_result(config, result);
        }
        () = shutdown_signal() => {}
    }

    // The receiver is held by the server, so this can't fail.
    let _ = stop.send(());
    let result = tokio::time::timeout(SHUTDOWN_GRACE_PERIOD, serve).await;
    invocations.close_all();
    match result {
        Ok(result) => serve_result(config, result)?,
        Err(_) => tracing::warn!("requests still running after {SHUTDOWN_GRACE_PERIOD:?}; stopping them"),
    }

    tracing::info!("shut down");
    Ok(())
}

fn serve_result(config: &Config, result: Result<(), Box<dyn Error + Send + Sync>>) -> Result<(), ServerError> {
    result.map_err(|error| ServerError::Serve {
        addr: config.listen,
        error,
    })
}

async fn open_cas(config: &Config) -> Result<FsCas, ServerError> {
    tracing::info!("using CAS directory {}", config.cas_dir.display());
    FsCas::open(&config.cas_dir)
        .await
        .map_err(|error| ServerError::Cas { error })
}

async fn open_action_cache(config: &Config) -> Result<FsActionCache, ServerError> {
    tracing::info!("using action cache directory {}", config.action_cache_dir.display());
    FsActionCache::open(&config.action_cache_dir)
        .await
        .map_err(|error| ServerError::ActionCache { error })
}

async fn open_artifacts(config: &Config, cas: Arc<dyn Cas>) -> Result<FsArtifactStore, ServerError> {
    tracing::info!("using artifact directory {}", config.artifacts_dir.display());
    FsArtifactStore::open(&config.artifacts_dir, cas)
        .await
        .map_err(|error| ServerError::Artifacts { error })
}

/// Create an executor for each configured environment. Unless configured
/// otherwise, the default environment runs actions locally.
async fn open_environments(
    config: &Config,
    cas: Arc<dyn Cas>,
    artifacts: Arc<dyn ArtifactStore>,
) -> Result<BTreeMap<String, Arc<dyn Executor>>, ServerError> {
    tracing::info!("using sandbox directory {}", config.sandbox_dir.display());
    executor_sandboxed::prepare_sandbox_root(&config.sandbox_dir, config.keep_sandboxes)
        .await
        .map_err(|error| ServerError::Sandbox {
            path: config.sandbox_dir.clone(),
            error,
        })?;

    let mut environments = config.environments.clone();
    environments
        .entry(scheduler::DEFAULT_ENVIRONMENT.to_string())
        .or_insert(ExecutionEnvironment::Local);

    let mut builder = EnvironmentBuilder {
        config,
        cas,
        artifacts,
        container_runtime: None,
    };
    let mut executors = BTreeMap::new();
    for (name, environment) in environments {
        tracing::info!("environment {name:?}: {environment:?}");
        let executor = builder.executor(&name, environment).await?;
        executors.insert(name, executor);
    }
    Ok(executors)
}

/// Creates the executors for environments, sharing what they can.
struct EnvironmentBuilder<'a> {
    config: &'a Config,
    cas: Arc<dyn Cas>,
    artifacts: Arc<dyn ArtifactStore>,

    /// Opened when the first container environment needs it.
    container_runtime: Option<Arc<dyn ContainerRuntime>>,
}

impl EnvironmentBuilder<'_> {
    async fn executor(&mut self, name: &str, environment: ExecutionEnvironment) -> Result<Arc<dyn Executor>, ServerError> {
        match environment {
            ExecutionEnvironment::Local => Ok(self.sandboxed(Arc::new(HostRunner), "buildbox-local")),
            ExecutionEnvironment::Container { image, network } => self.container_executor(name, image, network).await,
        }
    }

    async fn container_executor(&mut self, name: &str, image: String, network: bool) -> Result<Arc<dyn Executor>, ServerError> {
        artifacts::validate_name(&image).map_err(|error| ServerError::InvalidImage {
            name: name.to_string(),
            error,
        })?;
        // The image can be imported later, but say so now.
        if let Err(ArtifactError::NotFound(_)) = self.artifacts.container_image(&image).await {
            tracing::warn!("environment {name:?} uses the container image {image:?}, which hasn't been imported");
        }

        let runtime = self.container_runtime(name).await?;
        let network = if network { Network::Host } else { Network::Isolated };
        let runner = ContainerRunner::new(self.artifacts.clone(), image, network, runtime);
        Ok(self.sandboxed(Arc::new(runner), "buildbox-container"))
    }

    async fn container_runtime(&mut self, name: &str) -> Result<Arc<dyn ContainerRuntime>, ServerError> {
        if let Some(runtime) = &self.container_runtime {
            return Ok(runtime.clone());
        }

        // Inside the sandbox directory, whose cleanup leaves it alone.
        let state_dir = self.config.sandbox_dir.join("containers");
        let runtime = NamespaceRuntime::open(Helper::this_binary(), &state_dir)
            .await
            .map_err(|error| ServerError::Containers {
                name: name.to_string(),
                error,
            })?;
        let runtime: Arc<dyn ContainerRuntime> = Arc::new(runtime);
        self.container_runtime = Some(runtime.clone());
        Ok(runtime)
    }

    fn sandboxed(&self, runner: Arc<dyn Runner>, worker_name: &str) -> Arc<dyn Executor> {
        let config = self.config;
        let executor = SandboxedExecutor::new(
            self.cas.clone(),
            config.sandbox_dir.clone(),
            config.keep_sandboxes,
            runner,
            worker_name.to_string(),
        );
        Arc::new(executor)
    }
}

/// Resolves when the process receives SIGINT (Ctrl-C) or SIGTERM. Running
/// actions are stopped when the server shuts down.
async fn shutdown_signal() {
    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        tracing::warn!("failed to listen for SIGTERM; only Ctrl-C will stop the server");
        let _ = tokio::signal::ctrl_c().await;
        return;
    };

    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("received SIGINT, shutting down"),
        _ = terminate.recv() => tracing::info!("received SIGTERM, shutting down"),
    }
}
