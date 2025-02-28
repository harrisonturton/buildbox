pub use common::{Error, Result};
use clap::{Parser, Subcommand, Args};
use std::path::PathBuf;
use config::Config;

mod agent;
mod config;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct CliArgs {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Launch the build agent.
    Launch(LaunchArgs),
}

#[derive(Args)]
pub struct LaunchArgs {
    /// Path to the configuration file.
    #[arg(value_name = "config")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();

    let args = CliArgs::parse();

    match args.cmd {
        Command::Launch(args) => launch(&args).await,
    }
}

async fn launch(args: &LaunchArgs) -> Result<()> {
    let config = Config::load(&args.config)?;
    agent::launch(&config).await
}