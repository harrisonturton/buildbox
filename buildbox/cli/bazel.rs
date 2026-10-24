//! Running Bazel commands in the server, as `bb build`, `bb query` and so on,
//! or as `bazel ...` when bb is installed under that name.

use bazel_rust_connect::buildbox::bazel::v1::{BazelClient, EnvironmentVariable, Exec, RunRequest, RunResponse, run_response};
use bazel_server::BAZEL_VERSION;
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

/// The commands bb has of its own. Bazel has no commands by these names,
/// so every other command line is a Bazel one.
const BB_COMMANDS: [&str; 4] = ["up", "status", "actions", "artifacts"];

/// Bazel's exit code for a command cancelled by an interrupt.
const INTERRUPTED: u8 = 8;

/// Whether a command line is a Bazel command rather than one of bb's own.
pub fn is_bazel_command(args: &[OsString]) -> bool {
    let first = args.first().and_then(|arg| arg.to_str());
    !first.is_some_and(|first| BB_COMMANDS.contains(&first))
}

/// Run a Bazel command line in the server, starting the server if it isn't
/// running, and exit as the command does.
pub async fn run(args: Vec<OsString>) -> Result<ExitCode, Box<dyn Error>> {
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| arg.into_string().map_err(|arg| format!("argument {arg:?} isn't valid UTF-8")))
        .collect::<Result<_, _>>()?;
    if args == ["--version"] {
        println!("bazel {BAZEL_VERSION}");
        return Ok(ExitCode::SUCCESS);
    }
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
        terminal_columns: terminal_columns().unwrap_or(0),
        ..Default::default()
    };
    let mut stream = client.run(request).await?;
    let forward = async {
        while let Some(message) = stream.message().await? {
            if let Some(code) = write_output(message.to_owned_message())? {
                return Ok(code);
            }
        }
        Err("the server closed the connection before the command finished".into())
    };
    tokio::select! {
        result = forward => result,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("\nBazel caught interrupt signal; cancelling pending invocation.\n");
            Ok(ExitCode::from(INTERRUPTED))
        }
    }
}

/// Write one message of a command's output to stdout or stderr, returning
/// the exit code once the command has finished.
fn write_output(message: RunResponse) -> Result<Option<ExitCode>, Box<dyn Error>> {
    match message.output {
        Some(run_response::Output::Stdout(bytes)) => std::io::stdout().write_all(&bytes)?,
        Some(run_response::Output::Stderr(bytes)) => std::io::stderr().write_all(&bytes)?,
        Some(run_response::Output::ExitCode(code)) => {
            std::io::stdout().flush()?;
            return Ok(Some(ExitCode::from(code.clamp(0, 255) as u8)));
        }
        Some(run_response::Output::Exec(exec)) => {
            std::io::stdout().flush()?;
            return Err(exec_program(*exec).into());
        }
        None => {}
    }
    Ok(None)
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

/// The width of the terminal stderr is on, if it is one.
fn terminal_columns() -> Option<u32> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes only to the winsize it's given.
    let result = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) };
    (result == 0 && size.ws_col > 0).then_some(u32::from(size.ws_col))
}

/// Run `bb up` detached from this process, logging to the bazel directory.
fn start_server() -> Result<(), Box<dyn Error>> {
    let config = config::load(None, Settings::default(), &Environment::from_process())?;
    std::fs::create_dir_all(&config.bazel_dir)?;
    let log_path = config.bazel_dir.join("server.log");
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let program = std::env::current_exe()?;
    eprintln!("Starting local Bazel server ({BAZEL_VERSION}) and connecting to it...");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<OsString> {
        text.split_whitespace().map(OsString::from).collect()
    }

    #[test]
    fn only_bb_commands_are_not_bazel_commands() {
        assert!(!is_bazel_command(&args("up --root /data")));
        assert!(!is_bazel_command(&args("status")));
        assert!(is_bazel_command(&args("")));
        assert!(is_bazel_command(&args("--help")));
        assert!(is_bazel_command(&args("config")));
        assert!(is_bazel_command(&args("--bazelrc=x build //...")));
    }
}
