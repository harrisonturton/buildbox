use crate::proto::remote_asset::{
    Fetch, FetchBlobRequest, FetchBlobResponse, FetchDirectoryRequest, FetchDirectoryResponse,
    FetchServer, Push, PushBlobRequest, PushBlobResponse, PushDirectoryRequest,
    PushDirectoryResponse, PushServer,
};
use crate::proto::remote_execution::{
    digest_function, ActionCache, ActionCacheServer, ActionCacheUpdateCapabilities, ActionResult,
    BatchReadBlobsRequest, BatchReadBlobsResponse, BatchUpdateBlobsRequest,
    BatchUpdateBlobsResponse, CacheCapabilities, Capabilities, CapabilitiesServer,
    ContentAddressableStorage, ContentAddressableStorageServer, ExecuteRequest, Execution,
    ExecutionCapabilities, ExecutionServer, FindMissingBlobsRequest, FindMissingBlobsResponse,
    GetActionResultRequest, GetCapabilitiesRequest, GetTreeRequest, GetTreeResponse,
    ServerCapabilities, UpdateActionResultRequest, WaitExecutionRequest,
};
use clap::Parser;
use operations_proto::google::longrunning::Operation;
use semver_proto::build::bazel::semver::SemVer;
use std::error::Error;
use std::path::PathBuf;
use std::pin::Pin;
use tokio_stream::Stream;
use tonic::transport::{Identity, Server, ServerTlsConfig};
use tonic::{Request, Response, Status};

mod proto;

// Utility type for streaming responses
type ResponseStream<T> = Pin<Box<dyn Stream<Item = T> + Send + 'static>>;

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

    let fetch_service = FetchService::default();
    let push_service = PushService::default();
    let execution_service = ExecutionService::default();
    let action_cache_service = ActionCacheService::default();
    let cas_service = ContentAddressableStorageService::default();
    let capabilities_service = CapabilitiesService::default();

    Server::builder()
        .trace_fn(|_| tracing::info_span!("build_server"))
        .accept_http1(true)
        .tls_config(tls)?
        .add_service(FetchServer::new(fetch_service))
        .add_service(PushServer::new(push_service))
        .add_service(ExecutionServer::new(execution_service))
        .add_service(ActionCacheServer::new(action_cache_service))
        .add_service(ContentAddressableStorageServer::new(cas_service))
        .add_service(CapabilitiesServer::new(capabilities_service))
        .serve(addr)
        .await?;

    Ok(())
}

#[derive(Default, Debug)]
struct FetchService {}

#[tonic::async_trait]
impl Fetch for FetchService {
    async fn fetch_blob(
        &self,
        _req: Request<FetchBlobRequest>,
    ) -> Result<Response<FetchBlobResponse>, Status> {
        tracing::info!("FetchService::fetch_blob");
        Ok(Response::new(FetchBlobResponse::default()))
    }

    async fn fetch_directory(
        &self,
        _req: Request<FetchDirectoryRequest>,
    ) -> Result<Response<FetchDirectoryResponse>, Status> {
        tracing::info!("FetchService::fetch_directory");
        Ok(Response::new(FetchDirectoryResponse::default()))
    }
}

#[derive(Default, Debug)]
struct PushService {}

#[tonic::async_trait]
impl Push for PushService {
    async fn push_blob(
        &self,
        _req: Request<PushBlobRequest>,
    ) -> Result<Response<PushBlobResponse>, Status> {
        tracing::info!("PushService::push_blob");
        Ok(Response::new(PushBlobResponse::default()))
    }

    async fn push_directory(
        &self,
        _req: Request<PushDirectoryRequest>,
    ) -> Result<Response<PushDirectoryResponse>, Status> {
        tracing::info!("PushService::push_directory");
        Ok(Response::new(PushDirectoryResponse::default()))
    }
}

#[derive(Default, Debug)]
struct ExecutionService {}

#[tonic::async_trait]
impl Execution for ExecutionService {
    type ExecuteStream = ResponseStream<Result<Operation, Status>>;

    async fn execute(
        &self,
        _req: Request<ExecuteRequest>,
    ) -> Result<Response<Self::ExecuteStream>, Status> {
        tracing::info!("Execution::execute");
        todo!()
    }

    type WaitExecutionStream = ResponseStream<Result<Operation, Status>>;

    async fn wait_execution(
        &self,
        _req: Request<WaitExecutionRequest>,
    ) -> Result<Response<Self::WaitExecutionStream>, Status> {
        tracing::info!("Execution::wait_execution");
        todo!()
    }
}

#[derive(Default, Debug)]
struct ActionCacheService {}

#[tonic::async_trait]
impl ActionCache for ActionCacheService {
    async fn get_action_result(
        &self,
        _req: Request<GetActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        tracing::info!("ActionCache::get_action_result");
        Err(Status::internal("not implemented"))
    }

    async fn update_action_result(
        &self,
        _req: Request<UpdateActionResultRequest>,
    ) -> Result<Response<ActionResult>, Status> {
        tracing::info!("ActionCache::update_action_result");
        Ok(Response::new(ActionResult::default()))
    }
}

#[derive(Default, Debug)]
struct ContentAddressableStorageService {}

#[tonic::async_trait]
impl ContentAddressableStorage for ContentAddressableStorageService {
    async fn find_missing_blobs(
        &self,
        _req: Request<FindMissingBlobsRequest>,
    ) -> Result<Response<FindMissingBlobsResponse>, Status> {
        tracing::info!("ContentAddressableStorage::find_missing_blobs");
        Ok(Response::new(FindMissingBlobsResponse::default()))
    }

    async fn batch_update_blobs(
        &self,
        _req: Request<BatchUpdateBlobsRequest>,
    ) -> Result<Response<BatchUpdateBlobsResponse>, Status> {
        tracing::info!("ContentAddressableStorage::batch_update_blobs");
        Ok(Response::new(BatchUpdateBlobsResponse::default()))
    }

    async fn batch_read_blobs(
        &self,
        _req: Request<BatchReadBlobsRequest>,
    ) -> Result<Response<BatchReadBlobsResponse>, Status> {
        tracing::info!("ContentAddressableStorage::batch_read_blobs");
        Ok(Response::new(BatchReadBlobsResponse::default()))
    }

    type GetTreeStream = ResponseStream<Result<GetTreeResponse, Status>>;

    async fn get_tree(
        &self,
        _req: Request<GetTreeRequest>,
    ) -> Result<Response<Self::GetTreeStream>, Status> {
        tracing::info!("ContentAddressableStorage::get_tree");
        todo!()
    }
}

#[derive(Default, Debug)]
struct CapabilitiesService {}

#[tonic::async_trait]
impl Capabilities for CapabilitiesService {
    async fn get_capabilities(
        &self,
        _req: Request<GetCapabilitiesRequest>,
    ) -> Result<Response<ServerCapabilities>, Status> {
        tracing::info!("Capabilities::get_capabilities");
        let cap = ServerCapabilities {
            cache_capabilities: Some(CacheCapabilities {
                digest_function: vec![digest_function::Value::Sha256.into()],
                action_cache_update_capabilities: Some(ActionCacheUpdateCapabilities {
                    update_enabled: true,
                }),
                ..Default::default()
            }),
            execution_capabilities: Some(ExecutionCapabilities {
                digest_function: digest_function::Value::Sha256.into(),
                exec_enabled: true,
                ..Default::default()
            }),
            low_api_version: Some(SemVer {
                major: 2,
                minor: 0,
                patch: 0,
                ..Default::default()
            }),
            high_api_version: Some(SemVer {
                major: 2,
                minor: 0,
                patch: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        Ok(Response::new(cap))
    }
}
