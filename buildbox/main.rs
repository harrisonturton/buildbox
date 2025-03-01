use clap::{Args, Parser, Subcommand};
use common::{config::Config, Error, Result};
use proto::buildbox::{BuildboxClient, FindSandboxesRequest, FindBlobsRequest};
use std::process::ExitCode;
use std::{path::PathBuf, str::FromStr};
use tracing_subscriber::{EnvFilter, filter::LevelFilter, layer::SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tonic::{transport::Endpoint, Request};

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
    Up(UpCmd),
    /// List the sandboxes
    #[clap(name = "sandboxes")]
    ListSandboxes(ListSandboxesCmd),
    /// List the blobs
    #[clap(name = "blobs")]
    ListBlobs(ListBlobsCmd),
}

#[derive(Args, Debug)]
pub struct UpCmd {
    /// Path to the configuration file.
    #[arg(short, long)]
    pub config: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ListSandboxesCmd {
    #[arg(short, long)]
    pub addr: Option<String>,
}

#[derive(Args, Debug)]
pub struct ListBlobsCmd {
    #[arg(short, long)]
    pub addr: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing_or_die();
    
    let res = match CliArgs::parse().cmd {
        Command::Up(cmd) => up(&cmd).await,
        Command::ListSandboxes(cmd) => list_sandboxes(&cmd).await,
        Command::ListBlobs(cmd) => list_blobs(&cmd).await,
    };

    if let Err(err) = res {
        eprintln!("failed: {}", err.to_string());
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn init_tracing_or_die() {
    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var("LOG_LEVEL")
        .from_env_lossy();

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(env_filter)
        .init();
}

async fn up(cmd: &UpCmd) -> Result<()> {
    let config = Config::load(cmd.config.as_ref())?;
    rpc::launch(&config).await
}

async fn list_sandboxes(args: &ListSandboxesCmd) -> Result<()> {

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

async fn list_blobs(args: &ListBlobsCmd) -> Result<()> {
    let endpoint = {
        let addr = args.addr.clone().unwrap_or(DEFAULT_SERVER_ADDR.to_string());
        Endpoint::from_str(&addr).map_err(Error::boxed_msg("invalid address"))
    }?;

    let mut client = BuildboxClient::connect(endpoint).await.map_err(|err| {
        eprintln!("failed to create client: {err:?}");
        Error::boxed(err)
    })?;

    let req = Request::new(FindBlobsRequest {});
    let res = client.find_blobs(req).await.map_err(Error::boxed)?;

    println!("NAME");
    for blob in &res.into_inner().blobs {
        println!("{blob}");
    }

    Ok(())
}
