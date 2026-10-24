//! Running Bazel commands in the server, as `bb build`, `bb query` and so on,
//! or as `bazel ...` when bb is installed under that name.

use bazel_rust_connect::buildbox::bazel::v1::{BazelClient, EnvironmentVariable, Exec, RunRequest, run_response};
use config::{Environment, Settings};
use connectrpc::client::{ClientConfig, HttpClient};
use std::error::Error;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};

/// How long to wait for a server this command started to begin listening.
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a connection attempt may take before the server is presumed
/// not to be running.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// The commands bb has of its own; any other first argument is a Bazel
/// command.
const BB_COMMANDS: [&str; 6] = ["up", "config", "status", "actions", "artifacts", "help"];

/// Whether a command line is a Bazel command rather than one of bb's own.
pub fn is_bazel_command(program: &OsString, args: &[OsString]) -> bool {
    let invoked_as = std::path::Path::new(program).file_name().and_then(|name| name.to_str()).unwrap_or("");
    if invoked_as == "bazel" || invoked_as == "bazelisk" {
        return true;
    }
    let Some(first) = args.first().and_then(|arg| arg.to_str()) else {
        return false;
    };
    if matches!(first, "-h" | "--help" | "-V" | "--version") {
        return false;
    }
    !BB_COMMANDS.contains(&first)
}

/// Run a Bazel command line in the server, starting the server if it isn't
/// running, and exit as the command does.
pub async fn run(args: Vec<OsString>) -> Result<ExitCode, Box<dyn Error>> {
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| arg.into_string().map_err(|arg| format!("argument {arg:?} isn't valid UTF-8")))
        .collect::<Result<_, _>>()?;
    let config = config::load(None, Settings::default(), &Environment::from_process())?;
    ensure_server(config.listen).await?;

    let uri = crate::client::url_for(config.listen).parse()?;
    let client = BazelClient::new(HttpClient::plaintext(), ClientConfig::new(uri));
    let request = RunRequest {
        args,
        working_directory: std::env::current_dir()?.display().to_string(),
        env: std::env::vars()
            .map(|(name, value)| EnvironmentVariable {
                name,
                value,
                ..Default::default()
            })
            .collect(),
        interactive: std::io::stderr().is_terminal(),
        ..Default::default()
    };
    let mut stream = client.run(request).await?;
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    while let Some(message) = stream.message().await? {
        let message = message.to_owned_message();
        match message.output {
            Some(run_response::Output::Stdout(bytes)) => stdout.write_all(&bytes)?,
            Some(run_response::Output::Stderr(bytes)) => stderr.write_all(&bytes)?,
            Some(run_response::Output::ExitCode(code)) => {
                stdout.flush()?;
                return Ok(ExitCode::from(code.clamp(0, 255) as u8));
            }
            Some(run_response::Output::Exec(exec)) => {
                stdout.flush()?;
                return Err(exec_program(*exec).into());
            }
            None => {}
        }
    }
    Err("the server closed the connection before the command finished".into())
}

/// Replace this process with the program `bazel run` built.
fn exec_program(exec: Exec) -> std::io::Error {
    let Some((program, args)) = exec.argv.split_first() else {
        return std::io::Error::other("the server asked to run an empty command");
    };
    let mut command = std::process::Command::new(program);
    command.args(args).current_dir(&exec.working_directory);
    for var in exec.env {
        command.env(var.name, var.value);
    }
    command.exec()
}

/// Start the server in the background, unless it's already listening.
async fn ensure_server(listen: SocketAddr) -> Result<(), Box<dyn Error>> {
    let address = loopback(listen);
    if is_listening(address).await {
        return Ok(());
    }
    start_server()?;
    let deadline = Instant::now() + SERVER_START_TIMEOUT;
    while Instant::now() < deadline {
        if is_listening(address).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(format!("the server didn't start listening on {address} within {SERVER_START_TIMEOUT:?}").into())
}

async fn is_listening(address: SocketAddr) -> bool {
    matches!(tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(address)).await, Ok(Ok(_)))
}

fn loopback(listen: SocketAddr) -> SocketAddr {
    crate::client::url_for(listen)
        .trim_start_matches("http://")
        .parse()
        .unwrap_or(listen)
}

/// Run `bb up` detached from this process, logging to the bazel directory.
fn start_server() -> Result<(), Box<dyn Error>> {
    let config = config::load(None, Settings::default(), &Environment::from_process())?;
    std::fs::create_dir_all(&config.bazel_dir)?;
    let log_path = config.bazel_dir.join("server.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let program = std::env::current_exe()?;
    eprintln!("Starting local bb server and connecting to it... (log: {})", log_path.display());
    let mut command = std::process::Command::new(program);
    command
        .arg("up")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0);
    command.spawn()?;
    Ok(())
}
