use super::{bazel, buildbox};
use common::config::Config;
use common::{Error, Result};
use executor::LocalExecutor;
use proto::bazel::asset::{FetchServer, PushServer};
use proto::bazel::exec::ActionCacheServer;
use proto::bazel::exec::CapabilitiesServer;
use proto::bazel::exec::ContentAddressableStorageServer;
use proto::bazel::exec::ExecutionServer;
use proto::buildbox::BuildboxServer;
use proto::google::bytestream::ByteStreamServer;
use storage::file::FileStore;
use tonic::transport::Server;

pub async fn launch(config: &Config) -> Result<()> {
    let addr = config
        .addr
        .parse()
        .map_err(Error::boxed_msg("invalid address"))?;
    tracing::info!("Starting server on {addr}");

    let storage = FileStore::new(config.storage_dir.clone().into());

    let executor = LocalExecutor::new(
        config.sandbox_dir.clone().into(),
        storage.clone(),
        config.retain_sandboxes,
    );

    let fetch_service = bazel::FetchService::default();
    let push_service = bazel::PushService::default();
    let execution_service = bazel::ExecutionService::new(storage.clone(), executor.clone());
    let action_cache_service = bazel::ActionCacheService::new(storage.clone());
    let cas_service = bazel::ContentAddressableStorageService::new(storage.clone());
    let bytestream_service = bazel::ByteStreamService::new(storage.clone());
    let capabilities_service = bazel::CapabilitiesService::default();

    let buildbox_service = buildbox::BuildboxService::new(storage.clone(), executor.clone());

    Server::builder()
        .trace_fn(|_| tracing::info_span!("buildbox"))
        .accept_http1(true)
        .add_service(FetchServer::new(fetch_service))
        .add_service(PushServer::new(push_service))
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
