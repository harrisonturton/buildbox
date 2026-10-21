//! Assembles the stores, executor and gRPC services, and serves them.

use actioncache::{ActionCache, ActionCacheError};
use actioncache_fs::FsActionCache;
use admin_proto::buildbox::buildbox_server::BuildboxServer;
use bytestream_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use cas::{Cas, CasError};
use cas_fs::FsCas;
use config::Config;
use executor::Executor;
use executor_local::LocalExecutor;
use reapi::MAX_MESSAGE_SIZE_BYTES;
use remote_execution_proto::build::bazel::remote::execution::v2::action_cache_server::ActionCacheServer;
use remote_execution_proto::build::bazel::remote::execution::v2::capabilities_server::CapabilitiesServer;
use remote_execution_proto::build::bazel::remote::execution::v2::content_addressable_storage_server::ContentAddressableStorageServer;
use remote_execution_proto::build::bazel::remote::execution::v2::execution_server::ExecutionServer;
use scheduler::Scheduler;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal::unix::{signal, SignalKind};
use tonic::transport::Server;

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("failed to open the CAS: {error}")]
    Cas { error: CasError },

    #[error("failed to open the action cache: {error}")]
    ActionCache { error: ActionCacheError },

    #[error("failed to prepare the sandbox directory {}: {error}", path.display())]
    Sandbox { path: PathBuf, error: std::io::Error },

    #[error("failed to serve on {addr}: {error}")]
    Serve {
        addr: SocketAddr,
        error: tonic::transport::Error,
    },
}

/// Run the server until it receives SIGINT or SIGTERM.
pub async fn run(config: &Config) -> Result<(), ServerError> {
    let cas: Arc<dyn Cas> = Arc::new(open_cas(config).await?);
    let action_cache: Arc<dyn ActionCache> = Arc::new(open_action_cache(config).await?);
    let local_executor: Arc<dyn Executor> = Arc::new(open_executor(config, cas.clone()).await?);
    let scheduler = Arc::new(Scheduler::new(local_executor, config.max_concurrent_actions));

    let execution_service = reapi::ExecutionService::new(cas.clone(), action_cache.clone(), scheduler);
    let action_cache_service = reapi::ActionCacheService::new(cas.clone(), action_cache);
    let cas_service = reapi::ContentAddressableStorageService::new(cas.clone());
    let bytestream_service = reapi::ByteStreamService::new(cas);
    let capabilities_service = reapi::CapabilitiesService::default();
    let admin_service = admin::BuildboxService::default();

    tracing::info!("listening on {}", config.listen);
    tracing::info!("running up to {} actions at once", config.max_concurrent_actions);

    Server::builder()
        .add_service(
            ExecutionServer::new(execution_service)
                .max_decoding_message_size(MAX_MESSAGE_SIZE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_SIZE_BYTES),
        )
        .add_service(
            ActionCacheServer::new(action_cache_service)
                .max_decoding_message_size(MAX_MESSAGE_SIZE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_SIZE_BYTES),
        )
        .add_service(
            ContentAddressableStorageServer::new(cas_service)
                .max_decoding_message_size(MAX_MESSAGE_SIZE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_SIZE_BYTES),
        )
        .add_service(
            ByteStreamServer::new(bytestream_service)
                .max_decoding_message_size(MAX_MESSAGE_SIZE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_SIZE_BYTES),
        )
        .add_service(CapabilitiesServer::new(capabilities_service))
        .add_service(BuildboxServer::new(admin_service))
        .serve_with_shutdown(config.listen, shutdown_signal())
        .await
        .map_err(|error| ServerError::Serve {
            addr: config.listen,
            error,
        })?;

    tracing::info!("shut down");
    Ok(())
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

async fn open_executor(config: &Config, cas: Arc<dyn Cas>) -> Result<LocalExecutor, ServerError> {
    tracing::info!("using sandbox directory {}", config.sandbox_dir.display());
    LocalExecutor::open(cas, config.sandbox_dir.clone(), config.keep_sandboxes)
        .await
        .map_err(|error| ServerError::Sandbox {
            path: config.sandbox_dir.clone(),
            error,
        })
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
