//! `buildbox up`: run the server.

use crate::config_args::ConfigArgs;
use clap::Args;
use std::error::Error;
use telemetry::Telemetry;

#[derive(Args, Debug)]
pub struct UpArgs {
    #[command(flatten)]
    config: ConfigArgs,
}

pub async fn run(args: UpArgs) -> Result<(), Box<dyn Error>> {
    let config = args.config.load()?;
    let telemetry = Telemetry::init(config.trace_file.as_deref())?;
    if let Some(path) = &config.trace_file {
        tracing::info!("writing traces to {}", path.display());
    }

    let result = server::run(&config).await;
    telemetry.shutdown().await;
    result?;
    Ok(())
}
