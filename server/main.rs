mod blob;
mod error;
mod launcher;
mod proto;
mod rpc;

use error::{Result, Error};

#[tokio::main]
async fn main() -> Result<()> {
    launcher::main().await
}
