//! `buildbox up`: run the server.

use crate::config_args::ConfigArgs;
use clap::Args;
use std::error::Error;

#[derive(Args, Debug)]
pub struct UpArgs {
    #[command(flatten)]
    config: ConfigArgs,
}

pub async fn run(args: UpArgs) -> Result<(), Box<dyn Error>> {
    let config = args.config.load()?;
    server::run(&config).await?;
    Ok(())
}
