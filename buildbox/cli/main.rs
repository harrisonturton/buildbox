//! The buildbox command-line interface.

mod config_args;
mod show_config;
mod up;

use clap::{Parser, Subcommand};
use std::process::ExitCode;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// A Bazel remote execution server for running builds on this machine.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Up(up::UpArgs),

    /// Print the configuration the server would use.
    Config(show_config::ShowConfigArgs),
}

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();

    let cli = Cli::parse();
    let result = match cli.command {
        Command::Up(args) => up::run(args).await,
        Command::Config(args) => show_config::run(args),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Log to stderr, at the level set by the LOG_LEVEL environment variable
/// (e.g. LOG_LEVEL=debug), or at info by default.
fn init_logging() {
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var("LOG_LEVEL")
        .from_env_lossy();

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(filter)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_line_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
