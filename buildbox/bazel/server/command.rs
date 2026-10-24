//! Running one Bazel command line.

use crate::output::Output;
use crate::BazelServer;
use crate::workspace;
use bazel_bzlmod::repo_env::RepoEnvironment;
use bazel_options::{ParsedOptions, RcFile};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::Instrument;

/// What messages call this program, where Bazel's say `bazel`.
pub const PRODUCT: &str = "bb";

/// This build's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A command line to run, and where it was run from.
pub struct Invocation {
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub env: HashMap<String, String>,
}

/// The environment repository rules see in a command: the client's, with
/// `--repo_env` on top.
pub fn repo_environment(context: &CommandContext<'_>, options: &ParsedOptions) -> RepoEnvironment {
    let client = context.env.iter().map(|(name, value)| (name.clone(), value.clone()));
    RepoEnvironment::new(client, &options.all("repo_env"))
}

/// Bazel's exit codes.
pub mod exit {
    pub const SUCCESS: i32 = 0;
    pub const BUILD_FAILURE: i32 = 1;
    pub const COMMAND_LINE_ERROR: i32 = 2;
    pub const PARTIAL_ANALYSIS_FAILURE: i32 = 3;
    pub const ANALYSIS_FAILURE: i32 = 7;
    pub const INTERNAL_ERROR: i32 = 37;
}

/// The startup options given before the command, which affect how the
/// command's options are read.
#[derive(Default)]
struct StartupOptions {
    bazelrc: Vec<PathBuf>,
    ignore_all_rc_files: bool,
}

/// What a command needs to know about its workspace.
pub struct CommandContext<'a> {
    pub server: &'a BazelServer,
    pub workspace_root: PathBuf,
    pub working_directory: PathBuf,
    pub rc: RcFile,
    /// The client's environment.
    pub env: HashMap<String, String>,
}

/// Run a command line, returning its exit code.
pub async fn run(server: &BazelServer, invocation: Invocation, out: &Output) -> i32 {
    let command = split_startup_options(&invocation.args).1.first().cloned().unwrap_or_default();
    let span = tracing::info_span!("bazel.command", span.name = %format!("bazel {command}"), cwd = %invocation.working_directory.display());
    run_in_span(server, invocation, out).instrument(span).await
}

async fn run_in_span(server: &BazelServer, invocation: Invocation, out: &Output) -> i32 {
    let (startup, rest) = split_startup_options(&invocation.args);
    let Some((command, args)) = rest.split_first() else {
        out.stdout(&crate::help::index()).await;
        return exit::SUCCESS;
    };

    match command.as_str() {
        "version" => return crate::commands::version(args, out).await,
        "help" | "-h" | "-help" | "--help" => return crate::help::help(args, out).await,
        "license" => {
            out.stdout("bb is licensed under the Apache License 2.0.\n").await;
            return exit::SUCCESS;
        }
        _ => {}
    }
    if !crate::help::is_supported(command) {
        return unsupported(command, out).await;
    }

    let Some(workspace_root) = workspace::find_root(&invocation.working_directory) else {
        out.error(&format!(
            "The '{command}' command is only supported from within a workspace (below a directory having a MODULE.bazel file)."
        ))
        .await;
        return exit::COMMAND_LINE_ERROR;
    };
    let home = invocation.env.get("HOME").map(PathBuf::from);
    let rc = match bazel_options::parse_rc_files(&workspace_root, home.as_deref(), &startup.bazelrc, startup.ignore_all_rc_files) {
        Ok(rc) => rc,
        Err(error) => {
            out.error(&error.to_string()).await;
            return exit::COMMAND_LINE_ERROR;
        }
    };
    let context = CommandContext {
        server,
        workspace_root,
        working_directory: invocation.working_directory.clone(),
        rc,
        env: invocation.env.clone(),
    };

    match command.as_str() {
        "query" => crate::commands::query(&context, args, out).await,
        "info" => crate::commands::info(&context, args, out).await,
        "shutdown" => crate::commands::shutdown(&context, out).await,
        "clean" => crate::commands::clean(&context, args, out).await,
        "dump" => crate::commands::dump(&context, args, out).await,
        "page-check" => crate::commands::page_check(&context, out).await,
        "build" => crate::build::build(&context, args, out).await,
        "test" => crate::test::test(&context, args, out).await,
        "run" => crate::run::run(&context, args, out).await,
        other => unsupported(other, out).await,
    }
}

/// The commands Bazel has that bb doesn't support yet.
const UNSUPPORTED_COMMANDS: &[&str] = &[
    "analyze-profile",
    "aquery",
    "canonicalize-flags",
    "config",
    "coverage",
    "cquery",
    "fetch",
    "mobile-install",
    "mod",
    "print_action",
    "sync",
    "vendor",
];

/// Report a command bb can't run: one it doesn't support yet, or one Bazel
/// doesn't have either.
async fn unsupported(command: &str, out: &Output) -> i32 {
    if UNSUPPORTED_COMMANDS.contains(&command) {
        out.error(&format!("'{command}' isn't supported by bb yet")).await;
    } else {
        out.stderr(&format!("Command '{command}' not found. Try '{PRODUCT} help'.\n")).await;
    }
    exit::COMMAND_LINE_ERROR
}

/// Separate the startup options, which come before the command, from the
/// command and its arguments.
fn split_startup_options(args: &[String]) -> (StartupOptions, &[String]) {
    let mut startup = StartupOptions::default();
    let mut index = 0;
    while index < args.len() && args[index].starts_with("--") {
        let arg = &args[index];
        index += 1;
        let (name, value) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        match name {
            "--bazelrc" => {
                let value = value.or_else(|| {
                    index += 1;
                    args.get(index - 1).cloned()
                });
                if let Some(value) = value {
                    if value != "/dev/null" {
                        startup.bazelrc.push(PathBuf::from(value));
                    }
                }
            }
            "--ignore_all_rc_files" => startup.ignore_all_rc_files = true,
            "--output_base" | "--output_user_root" | "--install_base" | "--host_jvm_args" | "--server_javabase"
            | "--max_idle_secs" | "--connect_timeout_secs" | "--io_nice_level" | "--digest_function" => {
                if value.is_none() {
                    index += 1;
                }
            }
            _ => {}
        }
    }
    (startup, &args[index..])
}

/// The package a working directory is in, for relative target patterns.
pub fn working_package(workspace_root: &Path, working_directory: &Path) -> bazel_label::PackageId {
    let relative = working_directory.strip_prefix(workspace_root).unwrap_or(Path::new(""));
    let path = relative.to_str().and_then(|text| intern::PathId::parse(text).ok()).unwrap_or(intern::PathId::ROOT);
    bazel_label::PackageId::new(bazel_label::RepoName::MAIN, path)
}
