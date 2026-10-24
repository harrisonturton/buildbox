//! Running one Bazel command line.

use crate::output::Output;
use crate::BazelServer;
use crate::workspace;
use bazel_bzlmod::repo_env::RepoEnvironment;
use bazel_options::{ParsedOptions, RcFile};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::Instrument;

pub use bazel_starlark::BAZEL_VERSION;

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
        out.stdout(&help_text()).await;
        return exit::SUCCESS;
    };

    match command.as_str() {
        "version" => return crate::commands::version(args, out).await,
        "help" => {
            out.stdout(&help_text()).await;
            return exit::SUCCESS;
        }
        "license" => {
            out.stdout("bb is licensed under the Apache License 2.0.\n").await;
            return exit::SUCCESS;
        }
        _ => {}
    }

    let Some(workspace_root) = workspace::find_root(&invocation.working_directory) else {
        out.error(&format!(
            "The '{command}' command is only supported from within a workspace (below a directory having a MODULE.bazel file).\nSee documentation at https://bazel.build/concepts/build-ref#workspace"
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
        "dump" => crate::commands::dump(&context, out).await,
        "page-check" => crate::commands::page_check(&context, out).await,
        "build" => crate::build::build(&context, args, out).await,
        "test" => crate::test::test(&context, args, out).await,
        "run" => crate::run::run(&context, args, out).await,
        "coverage" | "cquery" | "aquery" | "fetch" | "sync" | "mod" | "print_action"
        | "canonicalize-flags" | "config" | "mobile-install" | "vendor" => {
            out.error(&format!("'{command}' isn't supported by bb yet")).await;
            exit::COMMAND_LINE_ERROR
        }
        other => {
            out.error(&format!("Command '{other}' not found. Try 'bazel help'.")).await;
            exit::COMMAND_LINE_ERROR
        }
    }
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

fn help_text() -> String {
    format!(
        "Usage: bb <command> <options> ...\n\n\
         bb runs Bazel commands in a server shared by every workspace on this machine.\n\
         It behaves like Bazel {BAZEL_VERSION}.\n\n\
         Available commands:\n  \
           query     Executes a dependency graph query.\n  \
           info      Displays runtime info about the bazel server.\n  \
           version   Prints version information for bazel.\n  \
           shutdown  Frees the memory this workspace's build graph uses.\n  \
           clean     Removes output files.\n"
    )
}
