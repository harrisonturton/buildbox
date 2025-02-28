use common::{Error, Result};
use sandbox::exec;
use storage::blob;
use proto::bazel::asset::{FetchServer, PushServer};
use proto::bazel::exec::ActionCacheServer;
use proto::bazel::exec::CapabilitiesServer;
use proto::bazel::exec::ContentAddressableStorageServer;
use proto::bazel::exec::ExecutionServer;
use proto::google::bytestream::ByteStreamServer;
use std::path::PathBuf;
use tonic::transport::{Identity, Server, ServerTlsConfig};

pub struct LaunchConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub cachedir: PathBuf,
    pub execdir: PathBuf,
}

pub async fn launch(args: LaunchConfig) -> Result<()> {
    let cert = std::fs::read(&args.cert).map_err(Error::io_msg("failed to read certificate"))?;
    let private_key = std::fs::read(&args.key).map_err(Error::io_msg("failed to read key"))?;

    let identity = Identity::from_pem(&cert, &private_key);
    let tls = ServerTlsConfig::new().identity(identity);

    let addr = "127.0.0.1:50051".parse().unwrap();
    tracing::info!("Starting server on {addr}");

    let store = blob::LocalBlobStore::new(args.cachedir.clone().into());
    let exec = exec::LocalExecService::new(args.execdir.into());
    let storage = storage::Storage::local(args.cachedir.into());
    
    let fetch_service = rpc::FetchService::default();
    let push_service = rpc::PushService::default();
    let execution_service = rpc::ExecutionService::new(store.clone(), exec);
    let action_cache_service = rpc::ActionCacheService::new(store.clone());
    let cas_service = rpc::ContentAddressableStorageService::new(store.clone(), storage.clone());
    let bytestream_service = rpc::ByteStreamService::new(store.clone());
    let capabilities_service = rpc::CapabilitiesService::default();

    Server::builder()
        .trace_fn(|_| tracing::info_span!("build_server"))
        .accept_http1(true)
        .tls_config(tls)
        .map_err(Error::boxed_msg("failed to construct tls_config"))?
        .add_service(FetchServer::new(fetch_service))
        .add_service(PushServer::new(push_service))
        .add_service(ExecutionServer::new(execution_service))
        .add_service(ActionCacheServer::new(action_cache_service))
        .add_service(ContentAddressableStorageServer::new(cas_service))
        .add_service(ByteStreamServer::new(bytestream_service))
        .add_service(CapabilitiesServer::new(capabilities_service))
        .serve(addr)
        .await
        .map_err(Error::boxed_msg("failed to create server"))?;

    Ok(())
}