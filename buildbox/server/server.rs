pub mod config;

use actioncache::ActionCache;
use actioncache_fs::FsActionCache;
use cas::Cas;
use cas_fs::FsCas;
use common::{Error, Result};
use config::Config;
use executor::Executor;
use executor_local::LocalExecutor;
use admin_proto::buildbox::buildbox_server::BuildboxServer;
use bytestream_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use remote_execution_proto::build::bazel::remote::execution::v2::action_cache_server::ActionCacheServer;
use remote_execution_proto::build::bazel::remote::execution::v2::capabilities_server::CapabilitiesServer;
use remote_execution_proto::build::bazel::remote::execution::v2::content_addressable_storage_server::ContentAddressableStorageServer;
use remote_execution_proto::build::bazel::remote::execution::v2::execution_server::ExecutionServer;
use std::path::PathBuf;
use std::sync::Arc;
use tonic::transport::Server;

pub async fn launch(config: &Config) -> Result<()> {
    let addr = config
        .addr
        .parse()
        .map_err(Error::boxed_msg("invalid address"))?;
    tracing::info!("Starting server on {addr}");

    let cas_dir = PathBuf::from(&config.storage_dir).join("cas");
    let cas: Arc<dyn Cas> = Arc::new(FsCas::open(&cas_dir).await.map_err(Error::boxed)?);

    let action_cache_dir = PathBuf::from(&config.storage_dir).join("ac");
    let action_cache: Arc<dyn ActionCache> =
        Arc::new(FsActionCache::open(&action_cache_dir).await.map_err(Error::boxed)?);

    let executor: Arc<dyn Executor> = Arc::new(LocalExecutor::new(
        cas.clone(),
        PathBuf::from(&config.sandbox_dir),
        config.retain_sandboxes,
    ));

    let execution_service = reapi::ExecutionService::new(cas.clone(), executor);
    let action_cache_service = reapi::ActionCacheService::new(cas.clone(), action_cache);
    let cas_service = reapi::ContentAddressableStorageService::new(cas.clone());
    let bytestream_service = reapi::ByteStreamService::new(cas);
    let capabilities_service = reapi::CapabilitiesService::default();

    let buildbox_service = admin::BuildboxService::default();

    Server::builder()
        .trace_fn(|_| tracing::info_span!("buildbox"))
        .accept_http1(true)
        .add_service(ExecutionServer::new(execution_service))
        .add_service(ActionCacheServer::new(action_cache_service))
        .add_service(ContentAddressableStorageServer::new(cas_service))
        .add_service(ByteStreamServer::new(bytestream_service))
        .add_service(CapabilitiesServer::new(capabilities_service))
        .add_service(BuildboxServer::new(buildbox_service))
        .serve(addr)
        .await
        .map_err(Error::boxed_msg("failed to create server"))?;

    Ok(())
}
