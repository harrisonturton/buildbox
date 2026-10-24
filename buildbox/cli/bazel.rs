//! Running Bazel commands in the server, as `bb build`, `bb query` and so on,
//! or as `bazel ...` when bb is installed under that name.

use crate::daemon::Daemon;
use bazel_rust_connect::buildbox::bazel::v1::{BazelClient, CancelRequest, EnvironmentVariable, Exec, RunRequest, run_response};
use bazel_server::{PRODUCT, VERSION};
use connectrpc::client::HttpClient;
use std::error::Error;
use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::process::ExitCode;

/// The commands bb has of its own. Bazel has no commands by these names,
/// so every other command line is a Bazel one.
const BB_COMMANDS: [&str; 3] = ["up", "server", "artifacts"];

/// Bazel's exit code for a command cancelled by an interrupt.
const INTERRUPTED: u8 = 8;

/// How many interrupts abandon a command rather than waiting for it to
/// stop.
const ABANDONING_INTERRUPTS: u32 = 3;

/// Whether a command line is a Bazel command rather than one of bb's own.
pub fn is_bazel_command(args: &[OsString]) -> bool {
    let first = args.first().and_then(|arg| arg.to_str());
    !first.is_some_and(|first| BB_COMMANDS.contains(&first))
}

/// How a command ended.
enum Outcome {
    Exited(ExitCode),
    /// The server wouldn't run it, for this reason, and should be
    /// restarted.
    Restart(String),
}

/// Run a Bazel command line in the server, starting the server if it isn't
/// running, or isn't the same build as this client, and exit as the
/// command does.
pub async fn run(args: Vec<OsString>) -> Result<ExitCode, Box<dyn Error>> {
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| arg.into_string().map_err(|arg| format!("argument {arg:?} isn't valid UTF-8")))
        .collect::<Result<_, _>>()?;
    if args == ["--version"] {
        println!("{PRODUCT} {VERSION}");
        return Ok(ExitCode::SUCCESS);
    }
    let daemon = Daemon::new(None)?;
    let request = run_request(args)?;
    daemon.ensure_running().await?;
    if let Outcome::Exited(code) = run_once(&daemon.client()?, request.clone()).await? {
        return Ok(code);
    }
    daemon.stop(false).await?;
    daemon.ensure_running().await?;
    match run_once(&daemon.client()?, request).await? {
        Outcome::Exited(code) => Ok(code),
        Outcome::Restart(reason) => Err(format!("the server won't run the command: {reason}").into()),
    }
}

fn run_request(args: Vec<String>) -> Result<RunRequest, Box<dyn Error>> {
    Ok(RunRequest {
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
        build_id: bazel_server::build_id().unwrap_or_default().to_string(),
        ..Default::default()
    })
}

/// Run a command, copying its output to stdout and stderr until it ends.
/// An interrupt cancels it, and it ends as the server ends it; enough of
/// them abandon it.
async fn run_once(client: &BazelClient<HttpClient>, request: RunRequest) -> Result<Outcome, Box<dyn Error>> {
    let mut stream = client.run(request).await?;
    let mut invocation = None;
    let mut interrupts = 0;
    loop {
        tokio::select! {
            message = stream.message() => {
                let Some(message) = message? else {
                    return Err("the server closed the connection before the command finished".into());
                };
                match message.to_owned_message().output {
                    Some(run_response::Output::InvocationId(id)) => invocation = Some(id),
                    Some(run_response::Output::Restart(restart)) => return Ok(Outcome::Restart(restart.reason)),
                    output => {
                        if let Some(code) = write_output(output)? {
                            return Ok(Outcome::Exited(code));
                        }
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                interrupts += 1;
                if interrupts >= ABANDONING_INTERRUPTS {
                    eprintln!("\n{PRODUCT} caught third interrupt signal; abandoning the command.\n");
                    return Ok(Outcome::Exited(ExitCode::from(INTERRUPTED)));
                }
                eprintln!("\n{PRODUCT} caught interrupt signal; cancelling pending invocation.\n");
                if let Some(id) = &invocation {
                    cancel(client, id).await;
                }
            }
        }
    }
}

/// Ask the server to cancel a command. If it can't be asked, the command
/// is left to finish.
async fn cancel(client: &BazelClient<HttpClient>, id: &str) {
    let request = CancelRequest {
        invocation_id: id.to_string(),
        ..Default::default()
    };
    if let Err(error) = client.cancel(request).await {
        eprintln!("ERROR: couldn't cancel the command: {error}");
    }
}

/// Write one message of a command's output to stdout or stderr, returning
/// the exit code once the command has finished.
fn write_output(output: Option<run_response::Output>) -> Result<Option<ExitCode>, Box<dyn Error>> {
    match output {
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
        Some(run_response::Output::InvocationId(_) | run_response::Output::Restart(_)) | None => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Vec<OsString> {
        text.split_whitespace().map(OsString::from).collect()
    }

    #[test]
    fn only_bb_commands_are_not_bazel_commands() {
        assert!(!is_bazel_command(&args("up --root /data")));
        assert!(!is_bazel_command(&args("server status")));
        assert!(is_bazel_command(&args("status")));
        assert!(is_bazel_command(&args("")));
        assert!(is_bazel_command(&args("--help")));
        assert!(is_bazel_command(&args("config")));
        assert!(is_bazel_command(&args("--bazelrc=x build //...")));
    }
}
