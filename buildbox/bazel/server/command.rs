//! Running one Bazel command line.

use crate::output::Output;
use crate::running::RunningCommand;
use crate::BazelServer;
use crate::workspace;
use bazel_bzlmod::command_overrides::CommandOverrides;
use bazel_fetch::CredentialHelpers;
use bazel_bzlmod::registry::Registries;
use bazel_bzlmod::repo_env::RepoEnvironment;
use bazel_loading::autoload::AutoloadSettings;
use bazel_options::{ParsedOptions, RcFile};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
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
/// environment, `--registry`, `--module_mirrors`, the overrides of
/// repositories and modules, and the files that changed. Returns the error
/// for an override the options give wrongly.
pub async fn refresh(context: &CommandContext<'_>, workspace: &workspace::Workspace, options: &ParsedOptions) -> Result<(), String> {
    check_lockfile_mode(options)?;
    let overrides = command_overrides(context, options)?;
    workspace.credentials.set(credential_helpers(context, options)?);
    workspace.refresh(repo_environment(context, options)).await;
    let urls = |name: &str| options.all(name).into_iter().map(|url| url.trim_end_matches('/').to_string()).collect();
    let mirrors = options.all("module_mirrors").into_iter().flat_map(|list| list.split(',')).filter(|url| !url.is_empty());
    let registries = Registries {
        registries: urls("registry"),
        mirrors: mirrors.map(|url| url.trim_end_matches('/').to_string()).collect(),
    };
    workspace.set_registries(registries).await;
    workspace.set_overrides(overrides).await;
    workspace.set_autoloads(autoload_settings(context, options)).await;
    Ok(())
}

/// Check `--lockfile_mode`. bb neither reads nor writes `MODULE.bazel.lock`,
/// which is what `off` asks for, and what `update` comes to without a
/// lockfile to keep; `refresh` and `error` it can't do.
fn check_lockfile_mode(options: &ParsedOptions) -> Result<(), String> {
    let mode = options.get("lockfile_mode").unwrap_or("update");
    match mode.to_ascii_lowercase().as_str() {
        "off" | "update" => Ok(()),
        "refresh" | "error" => Err(format!("--lockfile_mode={mode} :: bb doesn't support --lockfile_mode={mode} yet")),
        _ => Err(format!("While parsing option --lockfile_mode={mode}: Not a valid Lockfile mode: '{mode}' (should be off, update, refresh or error)")),
    }
}

/// The credential helpers `--credential_helper` configures, as Bazel finds
/// them: each `[scope=]path`, the path absolute, `%workspace%`-relative,
/// or a name on the client's `PATH`.
pub fn credential_helpers(context: &CommandContext<'_>, options: &ParsedOptions) -> Result<CredentialHelpers, String> {
    let mut helpers = Vec::new();
    for given in options.all("credential_helper") {
        let parsing = |message: &str| format!("While parsing option --credential_helper={given}: {message}");
        let (scope, path) = match given.split_once('=') {
            Some((scope, path)) => (Some(scope), path),
            None => (None, given),
        };
        if scope == Some("") {
            return Err(parsing("Credential helper scope must not be empty"));
        }
        if path.is_empty() {
            return Err(parsing("Credential helper path must not be empty"));
        }
        helpers.push((scope.map(str::to_string), helper_path(context, path)?));
    }
    let duration = |name: &str, default: &str| {
        let text = options.get(name).unwrap_or(default);
        bazel_options::convert_duration(text).map_err(|message| format!("While parsing option --{name}={text}: {message}"))
    };
    let env: Vec<(String, String)> = context.env.iter().map(|(name, value)| (name.clone(), value.clone())).collect();
    Ok(CredentialHelpers::new(
        helpers,
        env,
        context.workspace_root.clone(),
        duration("credential_helper_timeout", "10s")?,
        duration("credential_helper_cache_duration", "30m")?,
    ))
}

/// Where a credential helper is, as Bazel's `CommandLinePathFactory` finds
/// it.
fn helper_path(context: &CommandContext<'_>, path: &str) -> Result<PathBuf, String> {
    if path.split('/').any(|segment| segment == "..") {
        return Err(format!("Path '{path}' must not contain any uplevel references ('..')"));
    }
    if let Some(relative) = path.strip_prefix("%workspace%/") {
        return Ok(context.workspace_root.join(relative.trim_start_matches('/')));
    }
    if path.starts_with('/') {
        return Ok(PathBuf::from(path));
    }
    if path.contains('/') {
        return Err(format!("Path '{path}' must either be absolute or not contain any path separators"));
    }
    let search = context.env.get("PATH").cloned().unwrap_or_default();
    for dir in search.split(':').filter(|dir| dir.starts_with('/')) {
        let candidate = Path::new(dir).join(path);
        let executable = candidate.metadata().is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0);
        if executable {
            return Ok(candidate);
        }
    }
    Err(format!("Could not find file with name '{path}' on PATH '{search}'"))
}

/// The overrides of repositories and modules the options give.
fn command_overrides(context: &CommandContext<'_>, options: &ParsedOptions) -> Result<CommandOverrides, String> {
    Ok(CommandOverrides {
        repositories: path_overrides(context, options, "override_repository", "Repository overrides must be of the form 'repository-name=path'")?,
        modules: path_overrides(context, options, "override_module", "Module overrides must be of the form 'module-name=path'")?,
        injections: path_overrides(context, options, "inject_repository", "Repository injections must be of the form 'repository-name=path'")?,
        ignore_dev_dependency: options.flag("ignore_dev_dependency", false),
    })
}

/// The `name=path` pairs an option gives, as Bazel reads them: the last
/// path given for a name wins, and an empty one removes the name. Paths
/// may start with `~` or `%workspace%`, and relative ones are relative to
/// the working directory.
fn path_overrides(context: &CommandContext<'_>, options: &ParsedOptions, option: &str, form: &str) -> Result<Vec<(String, String)>, String> {
    let mut overrides: Vec<(String, String)> = Vec::new();
    for given in options.all(option) {
        let Some((name, path)) = given.split_once('=') else {
            return Err(format!("While parsing option --{option}={given}: {form}"));
        };
        if !is_valid_repo_name(name) {
            return Err(format!("While parsing option --{option}={given}: Invalid repository name given to override"));
        }
        overrides.retain(|(overridden, _)| overridden != name);
        if !path.is_empty() {
            overrides.push((name.to_string(), absolute_path(context, path)));
        }
    }
    Ok(overrides)
}

/// Whether a name given to an override can name a repository: an apparent
/// name, or a canonical one, which has a `+`.
fn is_valid_repo_name(name: &str) -> bool {
    let first = name.chars().next();
    let valid_start = first.is_some_and(|c| c.is_ascii_alphanumeric() || (name.contains('+') && c == '_'));
    valid_start && name.chars().all(|c| c.is_ascii_alphanumeric() || "_-.+~".contains(c))
}

/// A path an option gives, absolute: `~` is the client's home directory,
/// `%workspace%` the workspace, and a relative path is relative to the
/// working directory.
fn absolute_path(context: &CommandContext<'_>, path: &str) -> String {
    let home = context.env.get("HOME").cloned().unwrap_or_default();
    let path = match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{home}{rest}"),
        _ => path.to_string(),
    };
    let path = path.replace("%workspace%", &context.workspace_root.display().to_string());
    context.working_directory.join(path).display().to_string()
}

/// The autoloads the options ask for. A workspace whose `.bazelversion`
/// pins Bazel 7 or 8, which had the rules Bazel 9 moved out built in, gets
/// them autoloaded in every repository unless the options say otherwise,
/// as Bazel 8 autoloads them.
fn autoload_settings(context: &CommandContext<'_>, options: &ParsedOptions) -> AutoloadSettings {
    let pinned_before_9 = pinned_major_version(&context.workspace_root).is_some_and(|major| major < 9);
    let list = |text: &str| text.split(',').map(str::trim).filter(|item| !item.is_empty()).map(str::to_string).collect::<Vec<_>>();
    let symbols = match options.get("incompatible_autoload_externally") {
        Some(given) => list(given),
        None if pinned_before_9 => list(BAZEL_8_AUTOLOADS),
        None => Vec::new(),
    };
    AutoloadSettings {
        symbols,
        disable_in_main_repo: options.flag("incompatible_disable_autoloads_in_main_repo", !pinned_before_9),
        repositories_without: list(options.get("repositories_without_autoloads").unwrap_or_default()),
    }
}

/// What Bazel 8 autoloads by default, and the C++ rules, which Bazel 8 had
/// built in and Bazel 9 doesn't.
const BAZEL_8_AUTOLOADS: &str = "+@rules_cc,+@rules_python,+java_common,+JavaInfo,+JavaPluginInfo,ProguardSpecProvider,java_binary,java_import,java_library,java_plugin,java_test,java_runtime,java_toolchain,java_package_configuration,@com_google_protobuf,@rules_shell,+@rules_android";

/// The major version of Bazel the workspace's `.bazelversion` pins, if it
/// pins one.
fn pinned_major_version(workspace: &std::path::Path) -> Option<u32> {
    let text = std::fs::read_to_string(workspace.join(".bazelversion")).ok()?;
    let version = text.lines().next()?.trim();
    version.split(['.', '-']).next()?.parse().ok()
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
            let (reported, last) = bazel_loading::split_reported(error.message());
            if !reported.is_empty() {
                out.error(reported).await;
            }
            out.error(&main_mapping_error(last)).await;
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
    pub const LOCAL_ENVIRONMENTAL_ERROR: i32 = 36;
    pub const INTERNAL_ERROR: i32 = 37;
    /// Uploading the build events failed, but might work if tried again.
    pub const TRANSIENT_BES_UPLOAD_ERROR: i32 = 38;
    /// Uploading the build events failed, and would fail again.
    pub const PERSISTENT_BES_UPLOAD_ERROR: i32 = 45;
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

    if command != "info" {
        out.log_to(&crate::commands::output_base(&context).join(COMMAND_LOG));
    }
    let code = match command.as_str() {
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
    };
    crate::build_events::finish(code, out).await
}

/// The file in a workspace's output base that holds the latest command's
/// output, as Bazel's `command.log` does. `info` doesn't replace it, so it
/// can name the log of the command before.
pub const COMMAND_LOG: &str = "command.log";

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
    crate::build_events::finish(exit::INTERRUPTED, out).await
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
