use std::error::Error;
use tonic::{transport::Server, Request, Response, Status};
use remote_asset_proto::build::bazel::remote::asset::v1::{FetchBlobRequest, FetchBlobResponse, FetchDirectoryRequest, FetchDirectoryResponse};
use remote_asset_proto::build::bazel::remote::asset::v1::fetch_server::{Fetch, FetchServer};

#[derive(Default, Debug)]
struct FetchService {}

#[tonic::async_trait]
impl Fetch for FetchService {
  async fn fetch_blob(&self, _req: Request<FetchBlobRequest>) -> Result<Response<FetchBlobResponse>, Status> {
    Ok(Response::new(FetchBlobResponse::default()))
  }

  async fn fetch_directory(&self, _req: Request<FetchDirectoryRequest>) -> Result<Response<FetchDirectoryResponse>, Status> {
    Ok(Response::new(FetchDirectoryResponse::default()))
  }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
  let addr = "[::1]:50051".parse().unwrap();
  let fetch_service = FetchService::default();
  println!("Running fetch service on {addr}");

  Server::builder()
    .add_service(FetchServer::new(fetch_service))
    .serve(addr)
    .await
    .expect("server failed");

  Ok(())
}