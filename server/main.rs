use crate::proto::remote_asset::{FetchServer, PushServer};
use crate::proto::remote_execution::ActionCacheServer;
use crate::proto::remote_execution::CapabilitiesServer;
use crate::proto::remote_execution::ContentAddressableStorageServer;
use crate::proto::remote_execution::ExecutionServer;

use bytestream_proto::google::bytestream::byte_stream_server::ByteStreamServer;

use clap::Parser;
use std::error::Error;
use std::path::PathBuf;
use tonic::transport::{Identity, Server, ServerTlsConfig};

mod proto;
mod rpc;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Args {
    /// Certificate path.
    #[arg(value_name = "cert")]
    cert: PathBuf,

    /// Certificate private key path.
    #[arg(value_name = "key")]
    key: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let cert = std::fs::read(&args.cert)?;
    let private_key = std::fs::read(&args.key)?;

    let identity = Identity::from_pem(&cert, &private_key);
    let tls = ServerTlsConfig::new().identity(identity);

    let addr = "127.0.0.1:50051".parse().unwrap();
    tracing::info!("Starting server on {addr}");

    let fetch_service = rpc::FetchService::default();
    let push_service = rpc::PushService::default();
    let execution_service = rpc::ExecutionService::default();
    let action_cache_service = rpc::ActionCacheService::default();
    let cas_service = rpc::ContentAddressableStorageService::default();
    let bytestream_service = rpc::ByteStreamService::default();
    let capabilities_service = rpc::CapabilitiesService::default();

    Server::builder()
        .trace_fn(|_| tracing::info_span!("build_server"))
        .accept_http1(true)
        .tls_config(tls)?
        .add_service(FetchServer::new(fetch_service))
        .add_service(PushServer::new(push_service))
        .add_service(ExecutionServer::new(execution_service))
        .add_service(ActionCacheServer::new(action_cache_service))
        .add_service(ContentAddressableStorageServer::new(cas_service))
        .add_service(ByteStreamServer::new(bytestream_service))
        .add_service(CapabilitiesServer::new(capabilities_service))
        .serve(addr)
        .await?;

    Ok(())
}
