use clap::{Args, Parser, Subcommand};
use common::{config::Config, Error, Result};
use proto::buildbox::FindBlobsRequest;
use std::{path::PathBuf, str::FromStr};

const DEFAULT_SERVER_ADDR: &'static str = "http://[::1]:50051";

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct CliArgs {
    /// Subcommand to execute
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Launch the build agent.
    Up(UpArgs),
    /// List the sandboxes
    #[clap(name = "sandboxes")]
    ListSandboxes(SandboxesArgs),
    /// List the blobs
    #[clap(name = "blobs")]
    ListBlobs(BlobsArgs),
}

#[derive(Args, Debug)]
pub struct UpArgs {
    /// Path to the configuration file.
    #[arg(short, long)]
    pub config: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct SandboxesArgs {
    #[arg(short, long)]
    pub addr: Option<String>,
}

#[derive(Args, Debug)]
pub struct BlobsArgs {
    #[arg(short, long)]
    pub addr: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();

    let cli_args = CliArgs::parse();

    match cli_args.cmd {
        Command::Up(args) => {
            let config = Config::load(args.config.as_ref())?;
            up(&config).await
        }
        Command::ListSandboxes(args) => list_sandboxes(&args).await,
        Command::ListBlobs(args) => list_blobs(&args).await,
    }
}

async fn up(config: &Config) -> Result<()> {
    rpc::launch(&config).await
}

async fn list_sandboxes(args: &SandboxesArgs) -> Result<()> {
    use proto::buildbox::{BuildboxClient, FindSandboxesRequest};
    use tonic::{transport::Endpoint, Request};

    let endpoint = {
        let addr = args.addr.clone().unwrap_or(DEFAULT_SERVER_ADDR.to_string());
        Endpoint::from_str(&addr).map_err(Error::boxed_msg("invalid address"))
    }?;

    let mut client = BuildboxClient::connect(endpoint).await.map_err(|err| {
        eprintln!("failed to create client: {err:?}");
        Error::boxed(err)
    })?;

    let req = Request::new(FindSandboxesRequest {});
    let res = client.find_sandboxes(req).await.unwrap().into_inner();

    println!("NAME");
    for sandbox in &res.sandboxes {
        println!("{sandbox}");
    }

    Ok(())
}

async fn list_blobs(args: &BlobsArgs) -> Result<()> {
    use proto::buildbox::{BuildboxClient, FindSandboxesRequest};
    use tonic::{transport::Endpoint, Request};

    let endpoint = {
        let addr = args.addr.clone().unwrap_or(DEFAULT_SERVER_ADDR.to_string());
        Endpoint::from_str(&addr).map_err(Error::boxed_msg("invalid address"))
    }?;

    let mut client = BuildboxClient::connect(endpoint).await.map_err(|err| {
        eprintln!("failed to create client: {err:?}");
        Error::boxed(err)
    })?;

    let req = Request::new(FindBlobsRequest {});
    let res = client.find_blobs(req).await.unwrap().into_inner();

    println!("NAME");
    for blob in &res.blobs {
        println!("{blob}");
    }

    Ok(())
}
