//! The buildbox command-line interface.

mod actions;
mod artifacts;
mod bazel;
mod blobs;
mod client;
mod config_args;
mod daemon;
mod format;
mod image_layout;
mod server_command;
mod servers;
mod up;

use clap::{Parser, Subcommand};
use std::error::Error;
use std::ffi::OsString;
use std::process::ExitCode;
use std::time::Duration;

/// bb's own commands. Every other command line is run as Bazel would run
/// it, so these only take names Bazel doesn't use.
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

    /// Inspect and control the server: its status, commands and metrics,
    /// and starting and stopping it.
    Server(server_command::ServerArgs),

    /// Manage a running server's artifacts, such as container images.
    Artifacts(artifacts::ArtifactsArgs),
}

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

#[cfg(not(feature = "dhat-heap"))]
#[global_allocator]
static ALLOCATOR: jemalloc::Jemalloc = jemalloc::Jemalloc;

/// How long to wait, on exit, for blocking work such as a large file copy,
/// which can't be cancelled.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The stack each runtime thread gets. See [`new_runtime`].
const THREAD_STACK_SIZE: usize = 64 * 1024 * 1024;

fn main() -> ExitCode {
    // The server runs this binary as its container helper, which must stay
    // single-threaded, so this comes before anything that might start
    // threads.
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == container_namespaces::HELPER_COMMAND) {
        return container_namespaces::helper_main(&args[2..]);
    }

    if bazel::is_bazel_command(&args[1..]) {
        return run_bazel(args[1..].to_vec());
    }

    let (command_line, server_name) = match bazel::bb_command_line(&args[1..]) {
        Ok(command_line) => command_line,
        Err(message) => {
            eprintln!("ERROR: {message}");
            return ExitCode::from(2);
        }
    };
    if let Some(name) = server_name {
        std::env::set_var("BB_SERVER_NAME", name);
    }
    let cli = Cli::parse_from(args.iter().take(1).chain(&command_line));
    // The server sets up logging itself, once it has read its config, which
    // says where to send traces.
    if !matches!(cli.command, Command::Up(_)) {
        telemetry::init_logging();
    }

    let runtime = match new_runtime() {
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

/// The async runtime. Its threads evaluate Starlark, whose compiler
/// recurses once per level of a nested expression, so they get more stack
/// than the default 2 MiB; it's committed only as it's used.
fn new_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(THREAD_STACK_SIZE)
        .build()
}

/// Run a Bazel command in the server.
fn run_bazel(args: Vec<OsString>) -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: failed to start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(bazel::run(args));
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ERROR: {error}");
            ExitCode::from(37)
        }
    }
}

async fn run(command: Command) -> Result<(), Box<dyn Error>> {
    match command {
        Command::Up(args) => up::run(args).await,
        Command::Server(args) => server_command::run(args).await,
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
