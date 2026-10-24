//! The buildbox command-line interface.

mod actions;
mod artifacts;
mod blobs;
mod client;
mod config_args;
mod format;
mod image_layout;
mod show_config;
mod status;
mod up;

use clap::{Parser, Subcommand};
use std::error::Error;
use std::process::ExitCode;
use std::time::Duration;

/// A Bazel remote execution server for running builds on this machine.
#[derive(Parser)]
#[command(name = "bb", version)]
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

    /// Manage a running server's artifacts, such as container images.
    Artifacts(artifacts::ArtifactsArgs),
}

/// How long to wait, on exit, for blocking work such as a large file copy,
/// which can't be cancelled.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> ExitCode {
    let cli = Cli::parse();
    // The server sets up logging itself, once it has read its config, which
    // says where to send traces.
    if !matches!(cli.command, Command::Up(_)) {
        telemetry::init_logging();
    }

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
        Command::Artifacts(args) => artifacts::run(args).await,
    }
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
