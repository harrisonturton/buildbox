//! Assembles the stores, executor and RPC services, and serves them.

use actioncache::{ActionCache, ActionCacheError};
use actioncache_fs::FsActionCache;
use admin::AdminService;
use cas::{Cas, CasError};
use cas_fs::FsCas;
use config::Config;
use connectrpc::{ConnectRpcService, Limits, Router, Server};
use executor::Executor;
use executor_local::HostRunner;
use executor_sandboxed::SandboxedExecutor;
use reapi::{Invocations, RequestTracing, INVOCATION_IDLE_TIMEOUT, MAX_MESSAGE_SIZE_BYTES};
use scheduler::Scheduler;
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
    let local_executor: Arc<dyn Executor> = Arc::new(open_executor(config, cas.clone()).await?);
    let scheduler = Arc::new(Scheduler::new(local_executor, config.max_concurrent_actions));

    let execution_service = reapi::ExecutionService::new(cas.clone(), action_cache.clone(), scheduler.clone());
    let action_cache_service = reapi::ActionCacheService::new(cas.clone(), action_cache.clone());
    let cas_service = reapi::ContentAddressableStorageService::new(cas.clone());
    let bytestream_service = reapi::ByteStreamService::new(cas.clone());
    let capabilities_service = reapi::CapabilitiesService::default();
    let admin_service = AdminService::new(
        cas.clone(),
        config.cas_dir.display().to_string(),
        action_cache.clone(),
        config.action_cache_dir.display().to_string(),
        scheduler.clone(),
    );

    // The services speak gRPC, which Bazel uses, as well as the Connect
    // protocol and gRPC-Web.
    let router = Router::new()
        .add_service(Arc::new(execution_service))
        .add_service(Arc::new(action_cache_service))
        .add_service(Arc::new(cas_service))
        .add_service(Arc::new(bytestream_service))
        .add_service(Arc::new(capabilities_service))
        .add_service(Arc::new(admin_service));
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

async fn open_executor(config: &Config, cas: Arc<dyn Cas>) -> Result<SandboxedExecutor, ServerError> {
    tracing::info!("using sandbox directory {}", config.sandbox_dir.display());
    executor_sandboxed::prepare_sandbox_root(&config.sandbox_dir, config.keep_sandboxes)
        .await
        .map_err(|error| ServerError::Sandbox {
            path: config.sandbox_dir.clone(),
            error,
        })?;

    let runner = Arc::new(HostRunner);
    let worker_name = "buildbox-local".to_string();
    Ok(SandboxedExecutor::new(cas, config.sandbox_dir.clone(), config.keep_sandboxes, runner, worker_name))
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
