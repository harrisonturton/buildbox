//! Running one Bazel command line.

use crate::output::Output;
use crate::running::RunningCommand;
use crate::BazelServer;
use crate::workspace;
use bazel_bzlmod::registry::Registries;
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

/// Give the workspace's graph the command's inputs from outside it: the
/// environment, `--registry`, `--module_mirrors` and the files that
/// changed.
pub async fn refresh(context: &CommandContext<'_>, workspace: &workspace::Workspace, options: &ParsedOptions) {
    workspace.refresh(repo_environment(context, options)).await;
    let urls = |name: &str| options.all(name).into_iter().map(|url| url.trim_end_matches('/').to_string()).collect();
    let mirrors = options.all("module_mirrors").into_iter().flat_map(|list| list.split(',')).filter(|url| !url.is_empty());
    let registries = Registries {
        registries: urls("registry"),
        mirrors: mirrors.map(|url| url.trim_end_matches('/').to_string()).collect(),
    };
    workspace.set_registries(registries).await;
}

/// The error for a main repository mapping that couldn't be computed, as
/// Bazel words it.
pub fn main_mapping_error(message: &str) -> String {
    format!("Error computing the main repository mapping: {message}")
}

/// The main repository's mapping, once the modules are resolved and what
/// Bazel reports of the resolution is reported: direct dependencies that
/// resolved to other versions, as `--check_direct_dependencies` says, and
/// yanked versions `--allow_yanked_versions` doesn't allow, which are
/// errors.
pub async fn main_mapping(context: &CommandContext<'_>, session: &skyframe::Session, options: &ParsedOptions, out: &Output) -> Result<std::sync::Arc<bazel_label::RepoMappingTable>, i32> {
    let mapping = match session.evaluate(&bazel_loading::RepoMappingKey(bazel_label::RepoName::MAIN)).await {
        Ok(mapping) => std::sync::Arc::new(mapping.0.clone()),
        Err(error) => {
            report_module_file_error(context, session, out).await;
            out.error(&main_mapping_error(error.message())).await;
            return Err(exit::EXTERNAL_DEPS_ERROR);
        }
    };
    let Ok(graph) = session.evaluate(&bazel_bzlmod::resolve::DepGraphKey).await else {
        return Ok(mapping);
    };
    for (line, column, message) in graph.modules.first().map(|root| root.file.warnings.as_slice()).unwrap_or_default() {
        out.warning(&format!("{}/MODULE.bazel:{line}:{column}: {message}", context.workspace_root.display())).await;
    }
    check_direct_deps(&graph, options, out).await?;
    check_bazel_compatibility(&graph, options, out).await?;
    check_yanked(context, &graph, options, out).await?;
    Ok(mapping)
}

/// Report why the root MODULE.bazel couldn't be evaluated, if it
/// couldn't, at the call that failed, as Bazel does before summarizing.
async fn report_module_file_error(context: &CommandContext<'_>, session: &skyframe::Session, out: &Output) {
    let Ok(root) = session.evaluate(&bazel_bzlmod::resolve::RootModuleKey).await else {
        return;
    };
    let Some(error) = &root.error else {
        return;
    };
    let path_of = |file: &str| context.workspace_root.join(file.trim_start_matches("//").replace(':', "/")).display().to_string();
    let located = match (&error.error.position, error.error.traceback.last()) {
        (None, Some(frame)) => format!("{}:{}:{}: {}", path_of(&frame.position.file), frame.position.line, frame.position.column, error.error.describe(&path_of)),
        _ => error.error.report(&path_of),
    };
    out.error(&located).await;
}

async fn check_direct_deps(graph: &bazel_bzlmod::resolve::DepGraph, options: &ParsedOptions, out: &Output) -> Result<(), i32> {
    let mode = options.get("check_direct_dependencies").unwrap_or("warning");
    if mode == "off" || graph.direct_dep_mismatches.is_empty() {
        return Ok(());
    }
    for (repo, wanted, got) in &graph.direct_dep_mismatches {
        let message = format!(
            "For repository '{repo}', the root module requires module version {wanted}, but got {got} in the resolved dependency graph. Please update the version in your MODULE.bazel or set --check_direct_dependencies=off"
        );
        match mode {
            "error" => out.error(&message).await,
            _ => out.warning(&message).await,
        }
    }
    if mode != "error" {
        return Ok(());
    }
    out.error(&main_mapping_error("Direct dependency check failed.")).await;
    Err(exit::EXTERNAL_DEPS_ERROR)
}

/// Whether every module works with the version of Bazel bb is, as their
/// `bazel_compatibility` says, which `--check_bazel_compatibility` makes
/// an error, a warning, or nothing.
async fn check_bazel_compatibility(graph: &bazel_bzlmod::resolve::DepGraph, options: &ParsedOptions, out: &Output) -> Result<(), i32> {
    let mode = options.get("check_bazel_compatibility").unwrap_or("error");
    if mode == "off" {
        return Ok(());
    }
    let version = bazel_starlark::BAZEL_VERSION;
    for (index, module) in graph.modules.iter().enumerate() {
        let compatible = module.file.bazel_compatibility.iter().all(|entry| bazel_bzlmod::module_file::satisfies_bazel_compatibility(version, entry));
        if compatible {
            continue;
        }
        let key = match index {
            0 => "<root>".to_string(),
            _ => format!("{}@{}", module.name, if module.version.is_empty() { "_" } else { &module.version }),
        };
        let message = format!("Bazel version {version} is not compatible with module \"{key}\" (bazel_compatibility: [{}])", module.file.bazel_compatibility.join(", "));
        if mode == "warning" {
            out.warning(&message).await;
            continue;
        }
        out.error(&message).await;
        out.error(&main_mapping_error("Bazel compatibility check failed")).await;
        return Err(exit::EXTERNAL_DEPS_ERROR);
    }
    Ok(())
}

async fn check_yanked(context: &CommandContext<'_>, graph: &bazel_bzlmod::resolve::DepGraph, options: &ParsedOptions, out: &Output) -> Result<(), i32> {
    let mut allowed: Vec<&str> = options.all("allow_yanked_versions");
    if let Some(from_env) = context.env.get("BZLMOD_ALLOW_YANKED_VERSIONS") {
        allowed.push(from_env);
    }
    let allowed: Vec<&str> = allowed.iter().flat_map(|list| list.split(',')).filter(|entry| !entry.is_empty()).collect();
    if allowed.contains(&"all") {
        return Ok(());
    }
    let Some((key, reason)) = graph.yanked.iter().find(|(key, _)| !allowed.contains(&key.as_str())) else {
        return Ok(());
    };
    let message = format!(
        "Yanked version detected in your resolved dependency graph: {key}, for the reason: {reason}.\nYanked versions may contain serious vulnerabilities and should not be used. To fix this, use a bazel_dep on a newer version of this module. To continue using this version, allow it using the --allow_yanked_versions flag or the BZLMOD_ALLOW_YANKED_VERSIONS env variable."
    );
    out.error(&main_mapping_error(&message)).await;
    Err(exit::EXTERNAL_DEPS_ERROR)
}

/// Bazel's exit codes.
pub mod exit {
    pub const SUCCESS: i32 = 0;
    pub const BUILD_FAILURE: i32 = 1;
    pub const COMMAND_LINE_ERROR: i32 = 2;
    pub const PARTIAL_ANALYSIS_FAILURE: i32 = 3;
    pub const ANALYSIS_FAILURE: i32 = 7;
    pub const INTERRUPTED: i32 = 8;
    pub const INTERNAL_ERROR: i32 = 37;
    /// The modules or repositories the build depends on couldn't be
    /// resolved or fetched.
    pub const EXTERNAL_DEPS_ERROR: i32 = 48;
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
    /// The command's unique ID, as Bazel's invocation ID.
    pub invocation_id: String,
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
        invocation_id: crate::invocation::new_invocation_id(),
    };

    match command.as_str() {
        "query" => crate::queries::query(&context, args, out).await,
        "cquery" => crate::queries::cquery(&context, args, out).await,
        "aquery" => crate::queries::aquery(&context, args, out).await,
        "info" => crate::commands::info(&context, args, out).await,
        "shutdown" => crate::commands::shutdown(&context, out).await,
        "clean" => crate::commands::clean(&context, args, out).await,
        "dump" => crate::commands::dump(&context, args, out).await,
        "page-check" => crate::commands::page_check(&context, out).await,
        "build" => crate::build::build(&context, args, out).await,
        "test" => crate::test::test(&context, args, out).await,
        "run" => crate::run::run(&context, args, out).await,
        "fetch" => crate::fetch::fetch(&context, args, out).await,
        "mod" => crate::modcmd::mod_command(&context, args, out).await,
        other => unsupported(other, out).await,
    }
}

/// The commands Bazel has that bb doesn't support yet.
const UNSUPPORTED_COMMANDS: &[&str] = &[
    "analyze-profile",
    "canonicalize-flags",
    "config",
    "coverage",
    "mobile-install",
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

/// End a command that was cancelled, as Bazel ends an interrupted one.
pub async fn interrupted(command: &RunningCommand, out: &Output) -> i32 {
    let name = split_startup_options(&command.args).1.first().cloned().unwrap_or_default();
    match name.as_str() {
        "build" | "test" | "run" => {
            out.error("build interrupted").await;
            crate::build::report_stats(command.started, out).await;
            out.error("Build did NOT complete successfully").await;
        }
        _ => out.error(&format!("{name} interrupted")).await,
    }
    exit::INTERRUPTED
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
