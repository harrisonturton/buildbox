//! `buildbox config`: print the resolved configuration.

use crate::config_args::ConfigArgs;
use clap::Args;
use std::error::Error;

#[derive(Args, Debug)]
pub struct ShowConfigArgs {
    #[command(flatten)]
    config: ConfigArgs,
}

pub fn run(args: ShowConfigArgs) -> Result<(), Box<dyn Error>> {
    let config = args.config.load()?;
    print!("{}", toml::to_string(&config)?);
    Ok(())
}
