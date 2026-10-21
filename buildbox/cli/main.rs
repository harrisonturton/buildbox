//! The buildbox command-line interface.

mod actions;
mod client;
mod config_args;
mod format;
mod show_config;
mod status;
mod up;

use clap::{Parser, Subcommand};
use std::error::Error;
use std::process::ExitCode;
use std::time::Duration;
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

    /// Summarise a running server's storage and load.
    Status(status::StatusArgs),

    /// List the actions a running server is handling.
    Actions(actions::ActionsArgs),
}

/// How long to wait, on exit, for blocking work such as a large file copy,
/// which can't be cancelled.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    init_logging();
    let cli = Cli::parse();

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: failed to start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(run(cli.command));
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command) -> Result<(), Box<dyn Error>> {
    match command {
        Command::Up(args) => up::run(args).await,
        Command::Config(args) => show_config::run(args),
        Command::Status(args) => status::run(args).await,
        Command::Actions(args) => actions::run(args).await,
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
