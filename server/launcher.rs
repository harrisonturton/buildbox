use crate::error::{Error, Result};
use crate::proto::remote_asset::{FetchServer, PushServer};
use crate::proto::remote_execution::ActionCacheServer;
use crate::proto::remote_execution::CapabilitiesServer;
use crate::proto::remote_execution::ContentAddressableStorageServer;
use crate::proto::remote_execution::ExecutionServer;
use crate::{blob, rpc};

use bytestream_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use clap::Parser;
use std::path::PathBuf;
use tonic::transport::{Identity, Server, ServerTlsConfig};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// Certificate path.
    #[arg(value_name = "cert")]
    cert: PathBuf,

    /// Certificate private key path.
    #[arg(value_name = "key")]
    key: PathBuf,

    /// Cache blob storage directory.
    #[arg(value_name = "cachedir")]
    cachedir: PathBuf,
}

pub async fn main() -> Result<()> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let cert = std::fs::read(&args.cert).map_err(Error::io_msg("failed to read certificate"))?;
    let private_key = std::fs::read(&args.key).map_err(Error::io_msg("failed to read key"))?;

    let identity = Identity::from_pem(&cert, &private_key);
    let tls = ServerTlsConfig::new().identity(identity);

    let addr = "127.0.0.1:50051".parse().unwrap();
    tracing::info!("Starting server on {addr}");

    let store = blob::LocalBlobStore::new(
        "/home/harry/Documents/dev/2025/build-server/examples/blob".into(),
    );

    let fetch_service = rpc::FetchService::default();
    let push_service = rpc::PushService::default();
    let execution_service = rpc::ExecutionService::new(store.clone());
    let action_cache_service = rpc::ActionCacheService::default();
    let cas_service = rpc::ContentAddressableStorageService::new(store.clone());
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