mod blob;
mod error;
mod launcher;
mod proto;
mod rpc;

use error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    launcher::main().await
}
