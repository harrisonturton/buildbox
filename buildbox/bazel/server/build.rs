//! `bb build`: analyze targets, run the actions that build their files,
//! and write those files to the output tree.

use crate::build_events::TargetAttrs;
use crate::command::{CommandContext, exit, working_package};
use crate::output::Output;
use crate::top_level::{RunfilesTree, TopLevel, TopLevelKey, output_groups};
use crate::workspace::Workspace;
use crate::written::Written;
use bazel_analysis::conflicts::ActionConflictsKey;
use bazel_analysis::{Artifact, ConfiguredTarget, ConfiguredTargetKey, Incompatibility};
use bazel_analysis::settings::BuildSetting;
use bazel_analysis::workspace_status::StatusValues;
use bazel_config::{AspectDescriptor, BuildOptions, ConfigId, FlagValue, LabelParser, NativeOption, OptionType, OptionValue};
use bazel_execution::{ActionKey, OutputFile};
use bazel_label::{Label, LabelContext, RepoMappingTable, RepoName, TargetPattern};
use bazel_loading::RepoMappingKey;
use bazel_options::{OptionKind, OptionSpec, ParsedOptions};
use bazel_packages::{AttrValue, Target, TestFilter, is_test_kind, passes_tag_filters};
use bazel_query::{QueryOptions, TargetSet, Universe};
use bazel_ui::Phase;
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use futures::stream::FuturesUnordered;
use futures::future::BoxFuture;
use intern::{PathId, Symbol};
use skyframe::Session;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tracing::Instrument;

/// The options `build` and the commands built on it take: the native
/// options configurations are made of, and the build's own.
pub fn build_spec() -> OptionSpec {
    use OptionKind::*;
    let mut options = vec![
        ("action_env", Multi),
        ("repo_env", Multi),
        ("build", Bool),
        ("keep_going", Bool),
        ("symlink_prefix", Value),
        ("show_result", Value),
        ("output_groups", Multi),
        ("aspects", Multi),
        ("aspects_parameters", Multi),
        ("incompatible_autoload_externally", Value),
        ("incompatible_disable_autoloads_in_main_repo", Bool),
        ("repositories_without_autoloads", Value),
        ("build_event_json_file", Value),
        ("build_event_binary_file", Value),
        ("bes_backend", Value),
        ("bes_check_preceding_lifecycle_events", Bool),
        ("bes_header", Multi),
        ("bes_instance_name", Value),
        ("bes_keywords", Multi),
        ("bes_lifecycle_events", Bool),
        ("bes_results_url", Value),
        ("bes_system_keywords", Multi),
        ("bes_timeout", Value),
        ("bes_upload_mode", Value),
        ("build_request_id", Value),
        ("tls_certificate", Value),
        ("verbose_failures", Bool),
        ("jobs", Jobs),
        ("test_output", Value),
        ("test_filter", Value),
        ("test_arg", Multi),
        ("test_env", Multi),
        ("runs_per_test", Multi),
        ("flaky_test_attempts", Multi),
        ("test_sharding_strategy", Value),
        ("zip_undeclared_test_outputs", Bool),
        ("test_summary", Value),
        ("test_keep_going", Bool),
        ("test_timeout", Value),
        ("test_tag_filters", Value),
        ("test_size_filters", Value),
        ("test_timeout_filters", Value),
        ("test_lang_filters", Value),
        ("build_tag_filters", Value),
        ("build_tests_only", Bool),
        ("cache_test_results", TriState),
        ("color", Value),
        ("curses", Value),
        ("ui_actions_shown", Int),
        ("show_progress", Bool),
        ("enable_platform_specific_config", Bool),
        ("registry", Multi),
        ("override_repository", Multi),
        ("override_module", Multi),
        ("inject_repository", Multi),
        ("ignore_dev_dependency", Bool),
        ("lockfile_mode", Value),
        ("module_mirrors", Multi),
        ("workspace_status_command", Value),
        ("embed_label", Value),
        ("check_direct_dependencies", Value),
        ("check_bazel_compatibility", Value),
        ("allow_yanked_versions", Multi),
    ];
    options.extend_from_slice(NO_OP_OPTIONS);
    let native = bazel_config::command_line_options().filter(|option| !UNSUPPORTED_NATIVE_OPTIONS.contains(&option.name));
    options.extend(native.map(|option| (option.name, option_kind(option))));
    OptionSpec::new(&options)
}

/// The options a command built on `build` takes: `build`'s, and those of
/// its own.
pub fn command_spec(command: &str) -> OptionSpec {
    let mut options = build_spec().options();
    match command {
        "run" => options.push(("run_env", OptionKind::Multi)),
        "test" => options.push(("test_verbose_timeout_warnings", OptionKind::Bool)),
        _ => {}
    }
    OptionSpec::new(&options)
}

/// Options bb takes and ignores, since they only tune how Bazel itself
/// runs, which bb does its own way. bb's server keeps downloads and fetched
/// repositories in its root, shared by every workspace, as Bazel's
/// repository caches do, and uses no remote cache, so whether to upload to
/// one doesn't arise.
pub const NO_OP_OPTIONS: &[(&str, OptionKind)] = &[
    ("bes_oom_finish_upload_timeout", OptionKind::Value),
    ("bes_outerr_buffer_size", OptionKind::Value),
    ("bes_outerr_chunk_size", OptionKind::Value),
    ("experimental_check_external_repository_files", OptionKind::Bool),
    ("experimental_repository_cache_hardlinks", OptionKind::Bool),
    ("heap_dump_on_oom", OptionKind::Bool),
    ("loading_phase_threads", OptionKind::Value),
    ("local_resources", OptionKind::Multi),
    ("progress_report_interval", OptionKind::Value),
    ("remote_upload_local_results", OptionKind::Bool),
    ("repo_contents_cache", OptionKind::Value),
    ("repository_cache", OptionKind::Value),
    ("show_progress_rate_limit", OptionKind::Value),
    ("watchfs", OptionKind::Bool),
];

/// Native options configurations have that bb doesn't act on yet, so
/// they may only be given their defaults. `--build_runfile_links` isn't
/// among them though bb doesn't act on it either: bb always builds runfile
/// links, which `--nobuild_runfile_links` only saves the work of.
const UNSUPPORTED_NATIVE_OPTIONS: &[&str] = &[
    "build_runfile_manifests",
    "check_licenses",
    "enable_runfiles",
    "enforce_constraints",
    "experimental_debug_selects_always_succeed",
    "experimental_output_paths",
    "experimental_strict_fileset_output",
    "experimental_writable_outputs",
    "incompatible_auto_exec_groups",
    "platform_mappings",
    "shell_executable",
    "strict_filesets",
    "use_target_platform_for_tests",
];

/// How the command line gives a native option.
pub(crate) fn option_kind(option: &NativeOption) -> OptionKind {
    match option.ty {
        OptionType::Bool => OptionKind::Bool,
        OptionType::Int => OptionKind::Int,
        _ if option.multiple => OptionKind::Multi,
        _ => OptionKind::Value,
    }
}

/// What a build analyzed and built.
pub struct Built {
    pub main_mapping: Arc<RepoMappingTable>,
    pub config: ConfigId,
    /// Each top-level target, with the files it built.
    pub targets: Vec<(Label, Arc<ConfiguredTarget>, Vec<OutputFile>)>,
    /// Each aspect `--aspects` applies to each top-level target, with the
    /// files it built.
    pub aspects: Vec<BuiltAspect>,
    /// The output groups built, as `--output_groups` requests.
    pub groups: Vec<String>,
    /// How many of the targets didn't build every file they build at the
    /// top level.
    pub unbuilt: usize,
    /// How many of the aspects didn't.
    pub unbuilt_aspects: usize,
    /// How many top-level targets the command line names.
    pub requested: usize,
    /// The warnings for the targets that failed to analyze, which
    /// `--keep_going` builds without, as Bazel words them.
    pub skipped: Vec<String>,
    /// How many targets were analyzed, as Bazel says it.
    pub found: String,
    pub exec_root: PathBuf,
    pub errors: bool,
    /// Whether the build's only errors were top-level targets, or aspects
    /// applied to them, that failed to analyze, as `--keep_going` allows.
    pub analysis_incomplete: bool,
    /// Whether the build's only errors, but those of analysis, were target
    /// patterns `--keep_going` went past.
    pub patterns_failed: bool,
}

/// An aspect applied to a top-level target, and the files it built.
pub struct BuiltAspect {
    /// The aspect, as Bazel shows it: `//pkg:defs.bzl%name`.
    pub aspect: String,
    pub label: Label,
    pub target: Arc<ConfiguredTarget>,
    pub files: Vec<OutputFile>,
}

pub async fn build(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    let started = Instant::now();
    let options = match parse(context, "build", args, out).await {
        Ok(options) => options,
        Err(code) => return code,
    };
    let workspace = context.server.workspaces().get(&context.workspace_root);
    if let Err(message) = crate::command::refresh(context, &workspace, &options).await {
        out.error(&message).await;
        return crate::command::exit::COMMAND_LINE_ERROR;
    }
    if let Err(code) = update_status(context, &workspace, &options, out).await {
        return failed(code, started, out).await;
    }
    let session = workspace.graph.session().await;
    let built = match run(context, &session, &options, out).await {
        Ok(built) => built,
        Err(code) => return failed(code, started, out).await,
    };
    report(context, &options, &built, out).await;
    if built.patterns_failed {
        out.error("command succeeded, but there were errors parsing the target pattern").await;
    } else if built.analysis_incomplete {
        out.error("command succeeded, but not all targets were analyzed").await;
    }
    report_stats(started, out).await;
    if built.errors {
        out.error("Build did NOT complete successfully").await;
        return exit::BUILD_FAILURE;
    }
    completed(0, out).await;
    exit::SUCCESS
}

/// Give the workspace's status files the values of this build: what
/// `--workspace_status_command` prints, if the build runs actions, and
/// `--embed_label`.
pub async fn update_status(context: &CommandContext<'_>, workspace: &Workspace, options: &ParsedOptions, out: &Output) -> Result<(), i32> {
    let command = options.get("workspace_status_command").unwrap_or_default();
    let output = match command.is_empty() || !options.flag("build", true) {
        true => String::new(),
        false => run_status_command(context, command, out).await?,
    };
    let values = StatusValues::new(options.get("embed_label").unwrap_or_default(), &output);
    if let Some(events) = out.build_events() {
        events.workspace_status(&values);
    }
    workspace.set_status(values).await;
    Ok(())
}

/// Run `--workspace_status_command` in the workspace, with the client's
/// environment, as Bazel does, returning what it printed.
async fn run_status_command(context: &CommandContext<'_>, command: &str, out: &Output) -> Result<String, i32> {
    let ran = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(&context.workspace_root)
        .env_clear()
        .envs(&context.env)
        .stdin(std::process::Stdio::null())
        .output()
        .await;
    let failure = match ran {
        Ok(output) if output.status.success() => return Ok(String::from_utf8_lossy(&output.stdout).into_owned()),
        Ok(output) => format!("{}\n{}", exit_description(output.status), String::from_utf8_lossy(&output.stderr).trim_end()),
        Err(error) => error.to_string(),
    };
    out.error(&format!("<builtin>: BazelWorkspaceStatusAction stable-status.txt failed: Failed to determine workspace status: {failure}")).await;
    Err(exit::BUILD_FAILURE)
}

/// How a process ended, as Bazel describes it.
fn exit_description(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("Process exited with status {code}"),
        (None, Some(signal)) => format!("Process terminated by signal {signal}"),
        (None, None) => "Process exited abnormally".to_string(),
    }
}

/// End a command whose build failed before it finished, as Bazel does.
pub async fn failed(code: i32, started: Instant, out: &Output) -> i32 {
    if code == exit::BUILD_FAILURE {
        report_stats(started, out).await;
        out.error("Build did NOT complete successfully").await;
    }
    code
}

/// Say how long the command took and how its actions ran, with the
/// critical path if it got as far as analysis, when actions may run.
pub async fn report_stats(started: Instant, out: &Output) {
    let snapshot = out.snapshot().await;
    let mut elapsed = format!("Elapsed time: {:.3}s", started.elapsed().as_secs_f64());
    if out.analysis_started().await {
        elapsed.push_str(&format!(", Critical Path: {:.2}s", snapshot.critical_path.as_secs_f64()));
    }
    out.info(&elapsed).await;
    out.info(&bazel_ui::processes(&snapshot)).await;
}

/// Say that the build completed, with how many tests failed, and how many
/// actions it ran.
pub async fn completed(failed_tests: usize, out: &Output) {
    let outcome = match failed_tests {
        0 => " successfully".to_string(),
        1 => ", 1 test FAILED".to_string(),
        count => format!(", {count} tests FAILED"),
    };
    let actions = bazel_ui::total_actions(&out.snapshot().await);
    out.info(&format!("Build completed{outcome}, {actions}")).await;
}

/// Parse a build command's options.
pub async fn parse(context: &CommandContext<'_>, command: &str, args: &[String], out: &Output) -> Result<ParsedOptions, i32> {
    let all_args = bazel_options::command_args(&context.rc, command, args);
    match bazel_options::parse(&all_args, &command_spec(command), &context.rc, command) {
        Ok(options) => {
            if let Err(message) = parse_test_filter(&options) {
                out.error(&message).await;
                return Err(exit::COMMAND_LINE_ERROR);
            }
            out.configure(&options).await;
            crate::build_events::start(context, command, args, &options, out).await?;
            Ok(options)
        }
        Err(error) => {
            out.error(&error.to_string()).await;
            Err(exit::COMMAND_LINE_ERROR)
        }
    }
}

/// What a command analyzes the targets its patterns name for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// To build them.
    Build,
    /// To test them, reporting its test targets.
    Test,
    /// As the universe of a query of the configured graph: every target
    /// the patterns name, as they name them, test suites and all.
    Query,
}

/// Analyze the targets the options name and build their files.
pub async fn run(context: &CommandContext<'_>, session: &Session, options: &ParsedOptions, out: &Output) -> Result<Built, i32> {
    run_command(context, session, options, Purpose::Build, out).await
}

/// [`run`], for a command with another [`Purpose`].
///
/// Each target's files start building as soon as it's analyzed, while
/// other targets are still being analyzed, as Bazel's do. The build waits
/// for those evaluations in the order it reports them, so the head start
/// changes only when actions run; and the command ends once they're done,
/// even if it fails before it would have waited for them.
pub async fn run_command(
    context: &CommandContext<'_>,
    session: &Session,
    options: &ParsedOptions,
    purpose: Purpose,
    out: &Output,
) -> Result<Built, i32> {
    let workspace = context.server.workspaces().get(&context.workspace_root);
    workspace.jobs.set(options.get("jobs").and_then(|jobs| jobs.parse().ok()));
    let tracker = workspace.progress.track();
    out.track(tracker).await;
    let exec_root = crate::commands::output_base_for(context.server, &context.workspace_root).join("execroot/_main");
    if let Some(events) = out.build_events() {
        events.workspace_info(&exec_root);
    }
    out.set_exec_root(exec_root).await;
    let (analyzed, head_starts) = futures::channel::mpsc::unbounded();
    let execute_actions = options.flag("build", true);
    let analyzed = execute_actions.then_some(analyzed);
    let groups = output_groups(options);
    let (built, ()) = futures::join!(analyze_and_build(context, session, options, purpose, analyzed, out), head_starts_of(session, head_starts, &groups));
    built
}

/// Give each target received a [`head_start`], until there are no more.
async fn head_starts_of(session: &Session, mut targets: UnboundedReceiver<Arc<ConfiguredTarget>>, groups: &[String]) {
    let mut running: FuturesUnordered<BoxFuture<'_, ()>> = FuturesUnordered::new();
    let mut receiving = true;
    loop {
        tokio::select! {
            target = targets.next(), if receiving => match target {
                Some(target) => running.push(Box::pin(head_start(session, target, groups))),
                None => receiving = false,
            },
            Some(()) = running.next(), if !running.is_empty() => {}
            else => return,
        }
    }
}

/// Start building the files of an analyzed target.
async fn head_start(session: &Session, target: Arc<ConfiguredTarget>, groups: &[String]) {
    let key = TopLevelKey {
        target: ConfiguredTargetKey {
            label: target.label,
            config: target.config,
        },
        groups: groups.to_vec(),
    };
    let Ok(top) = session.evaluate(&key).await else {
        return;
    };
    let actions: BTreeSet<ActionKey> = generating_actions(session, &top.artifacts).await.into_iter().map(|(key, _)| key).collect();
    let actions: Vec<ActionKey> = actions.into_iter().collect();
    session.evaluate_many(&actions).await;
}

async fn analyze_and_build(
    context: &CommandContext<'_>,
    session: &Session,
    options: &ParsedOptions,
    purpose: Purpose,
    analyzed: Option<UnboundedSender<Arc<ConfiguredTarget>>>,
    out: &Output,
) -> Result<Built, i32> {
    out.phase(Phase::RepoMapping).await;
    let main_mapping = crate::command::main_mapping(context, session, options, out).await?;
    let keep_going = options.flag("keep_going", false);
    out.phase(Phase::Loading).await;
    let Requested { labels, explicit, patterns_failed } = match targets(context, session, options, &main_mapping, purpose, out).await {
        Ok(requested) => requested,
        Err(code) => return Err(code),
    };
    let config = match configuration(context, session, options, &main_mapping).await {
        Ok(config) => config,
        Err(message) => {
            out.error(&message).await;
            return Err(exit::COMMAND_LINE_ERROR);
        }
    };
    let requested = match requested_aspects(options, &main_mapping) {
        Ok(requested) => requested,
        Err(message) => {
            out.error(&message).await;
            return Err(exit::BUILD_FAILURE);
        }
    };
    let what = analyzing(&labels, &main_mapping);
    out.phase(Phase::Analyzing(what.clone())).await;
    let aspects = match bazel_analysis::aspect::top_level(session, &requested.aspects, &requested.parameters).await {
        Ok(aspects) => aspects,
        Err(error) => return Err(aspects_failed(options, &requested, &labels, &main_mapping, error.message(), out).await),
    };
    let (analyzed, failed) = analyze(session, &labels, config, &main_mapping, analyzed, out).await;
    if let Some(events) = out.build_events() {
        for (label, target) in &analyzed {
            events.target_configured(&bazel_query::display_label(*label, &main_mapping), target);
        }
    }
    let (applied, failed_aspects) = apply_aspects(session, &analyzed, &aspects, &main_mapping, out).await;
    let incompatible = report_explicitly_incompatible(&analyzed, &explicit, &main_mapping, out).await;
    if let Some(first) = failed.first().filter(|_| !keep_going) {
        out.error(&build_aborted(first)).await;
        return Err(exit::BUILD_FAILURE);
    }
    if let Some(first) = failed_aspects.first().filter(|_| !keep_going) {
        return Err(aspect_analysis_aborted(options, &requested, &analyzed, first, config, &main_mapping, out).await);
    }
    if incompatible && !keep_going {
        return Err(exit::BUILD_FAILURE);
    }
    let conflicted = report_conflicts(session, &analyzed, &applied, out).await;
    if conflicted && !keep_going {
        return Err(exit::BUILD_FAILURE);
    }
    let mut errors = incompatible || conflicted;
    let execute_actions = options.flag("build", true);
    let skipped = skipped_warnings(&failed, &failed_aspects, &main_mapping);
    if !execute_actions {
        report_skipped(&skipped, keep_going, analyzed.len(), labels.len(), "Analysis", out).await;
    }
    if summarizes_analysis(execute_actions, &labels) {
        out.info(&bazel_ui::analyzed(&what, &out.snapshot().await)).await;
    }
    let found = found(&analyzed, purpose == Purpose::Test);
    let analysis_failed = !failed.is_empty() || !failed_aspects.is_empty();
    out.phase(Phase::Executing).await;

    let exec_root = crate::commands::output_base_for(context.server, &context.workspace_root).join("execroot/_main");
    let backend = context.server.backend();
    let groups = output_groups(options);
    let applied_targets: Vec<Arc<ConfiguredTarget>> = applied.iter().map(|(_, _, target)| target.clone()).collect();
    let (target_tops, failed) = top_levels(session, analyzed.iter().map(|(_, target)| target), &groups, execute_actions, out).await;
    errors |= failed;
    let (aspect_tops, failed) = top_levels(session, applied_targets.iter(), &groups, execute_actions, out).await;
    errors |= failed;
    let all: Vec<Artifact> = target_tops
        .iter()
        .chain(&aspect_tops)
        .flatten()
        .flat_map(|top| top.artifacts.iter().copied())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let (built, failed) = execute(session, &all, out).await;
    errors |= failed;
    let built_by_path: HashMap<PathId, OutputFile> = built.iter().map(|file| (file.path, *file)).collect();
    let mut targets = Vec::new();
    let mut unbuilt = 0;
    let mut unbuilt_aspects = 0;
    for ((label, target), top) in analyzed.into_iter().zip(&target_tops) {
        let files = built_files(top.as_deref(), &built_by_path);
        unbuilt += usize::from(files.is_none());
        if let Some(events) = out.build_events().filter(|_| execute_actions) {
            events.target_completed(&bazel_query::display_label(label, &main_mapping), &target, files.as_deref(), &exec_root);
        }
        targets.push((label, target, files.unwrap_or_default()));
    }
    let mut built_aspects = Vec::new();
    for ((aspect, label, target), top) in applied.into_iter().zip(&aspect_tops) {
        let files = built_files(top.as_deref(), &built_by_path);
        unbuilt_aspects += usize::from(files.is_none());
        let files = files.unwrap_or_default();
        built_aspects.push(BuiltAspect { aspect, label, target, files });
    }
    if let Some(backend) = &backend {
        let written = context.server.workspaces().get(&context.workspace_root).written.clone();
        let mut results = vec![write_files(backend, &written, &exec_root, built).await];
        let trees: Vec<_> = targets
            .iter()
            .zip(&target_tops)
            .filter_map(|((_, target, files), top)| Some((target, files, top.as_ref()?.runfiles.as_ref()?)))
            .map(|(target, files, runfiles)| Box::pin(write_runfiles_tree(session, &written, target, files, runfiles, &exec_root, &context.workspace_root)))
            .collect();
        results.extend(futures::stream::iter(trees).buffer_unordered(OUTPUT_WRITERS).collect::<Vec<_>>().await);
        for error in results.into_iter().filter_map(Result::err) {
            out.error(error.message()).await;
            errors = true;
        }
    }
    Ok(Built {
        main_mapping,
        config,
        targets,
        aspects: built_aspects,
        groups,
        unbuilt,
        unbuilt_aspects,
        requested: labels.len(),
        skipped,
        found,
        exec_root,
        errors: errors || analysis_failed || patterns_failed,
        analysis_incomplete: analysis_failed && !errors && !patterns_failed,
        patterns_failed: patterns_failed && !errors,
    })
}

/// Report the conflicts among the actions of the analyzed targets, and
/// of the aspects applied to them, returning whether there were any.
async fn report_conflicts(
    session: &Session,
    analyzed: &[(Label, Arc<ConfiguredTarget>)],
    applied: &[(String, Label, Arc<ConfiguredTarget>)],
    out: &Output,
) -> bool {
    let targets = analyzed.iter().map(|(_, target)| target).chain(applied.iter().map(|(_, _, target)| target));
    let mut tops: Vec<ConfiguredTargetKey> = targets
        .map(|target| ConfiguredTargetKey {
            label: target.label,
            config: target.config,
        })
        .collect();
    tops.sort();
    tops.dedup();
    let conflicts = match session.evaluate(&ActionConflictsKey(tops)).await {
        Ok(conflicts) => conflicts,
        Err(error) => {
            out.error(error.message()).await;
            return true;
        }
    };
    for conflict in &conflicts.0 {
        out.error(conflict).await;
    }
    !conflicts.0.is_empty()
}

/// The files of a top-level target's that were built, or `None` if any
/// wasn't.
fn built_files(top: Option<&TopLevel>, built_by_path: &HashMap<PathId, OutputFile>) -> Option<Vec<OutputFile>> {
    let artifacts = top.map(|top| top.artifacts.as_slice()).unwrap_or_default();
    let files: Vec<OutputFile> = artifacts.iter().filter_map(|artifact| built_by_path.get(&artifact.exec_path).copied()).collect();
    match files.len() < artifacts.len() {
        true => None,
        false => Some(files),
    }
}

/// The aspects `--aspects` names, in order, each as its `.bzl` file and
/// the name it's exported as, and the `--aspects_parameters` given them.
struct RequestedAspects {
    /// Each as the command line gives it.
    given: Vec<String>,
    aspects: Vec<(Label, String)>,
    parameters: Vec<(String, String)>,
}

fn requested_aspects(options: &ParsedOptions, main_mapping: &RepoMappingTable) -> Result<RequestedAspects, String> {
    let mut parameters = Vec::new();
    for text in options.all("aspects_parameters") {
        let Some((name, value)) = text.split_once('=') else {
            return Err(format!("Invalid aspects_parameters: '{text}': expected a 'name=value' assignment"));
        };
        parameters.push((name.to_string(), value.to_string()));
    }
    let label_context = LabelContext::new(bazel_label::PackageId::new(RepoName::MAIN, intern::PathId::ROOT), main_mapping);
    let given: Vec<String> = options.all("aspects").iter().flat_map(|text| text.split(',')).filter(|text| !text.is_empty()).map(str::to_string).collect();
    let mut aspects = Vec::with_capacity(given.len());
    for text in &given {
        let Some((file, name)) = text.rsplit_once('%') else {
            return Err(format!("Aspect '{text}' is unknown"));
        };
        let file = Label::parse(file, &label_context).map_err(|error| format!("Invalid aspect '{text}': {error}"))?;
        aspects.push((file, name.to_string()));
    }
    Ok(RequestedAspects { given, aspects, parameters })
}

/// Report that the aspects `--aspects` names can't be applied, as Bazel
/// does: as the failure of every top-level target, and of the aspects'
/// analysis on the first.
async fn aspects_failed(options: &ParsedOptions, requested: &RequestedAspects, labels: &[Label], mapping: &RepoMappingTable, message: &str, out: &Output) -> i32 {
    out.error(message).await;
    let show_result: usize = options.get("show_result").and_then(|value| value.parse().ok()).unwrap_or(1);
    if labels.len() <= show_result {
        let failed: String = labels.iter().map(|label| format!("Target {} failed to build\n", bazel_query::display_label(*label, mapping))).collect();
        out.stderr(&failed).await;
    }
    if !options.flag("verbose_failures", false) {
        out.stderr("Use --verbose_failures to see the command lines of failed build steps.\n").await;
    }
    let on = labels.first().map(|label| bazel_query::display_label(*label, mapping)).unwrap_or_default();
    out.error(&format!("Analysis of aspects '{}' failed; build aborted: {message}", aspects_description(requested, &on))).await;
    exit::BUILD_FAILURE
}

/// Apply the aspects to each compatible top-level target, each after
/// those before it. Returns each aspect as Bazel shows it, with the
/// target, and its application; and the applications that failed, whose
/// errors it reports but for their last, which ends the build.
async fn apply_aspects(
    session: &Session,
    analyzed: &[(Label, Arc<ConfiguredTarget>)],
    aspects: &[AspectDescriptor],
    mapping: &RepoMappingTable,
    out: &Output,
) -> (Vec<(String, Label, Arc<ConfiguredTarget>)>, Vec<FailedAspect>) {
    let mut wanted = Vec::new();
    let mut failed = Vec::new();
    for (label, target) in analyzed.iter().filter(|(_, target)| target.incompatible.is_none()) {
        let plain = ConfiguredTargetKey {
            label: target.label,
            config: target.config,
        };
        let keys = match bazel_analysis::aspect::application_keys(session, plain, aspects).await {
            Ok(keys) => keys,
            Err(error) => {
                failed.push(FailedAspect { label: *label, error });
                continue;
            }
        };
        for (aspect, key) in aspects.iter().zip(keys) {
            let shown = format!("{}%{}", bazel_query::display_label(aspect.file, mapping), aspect.name);
            wanted.push((shown, *label, key));
        }
    }
    let keys: Vec<ConfiguredTargetKey> = wanted.iter().map(|(_, _, key)| *key).collect();
    let results = session.evaluate_many(&keys).await;
    let mut applied = Vec::new();
    for ((shown, label, _), result) in wanted.into_iter().zip(results) {
        match result {
            Ok(target) => applied.push((shown, label, target)),
            Err(error) => failed.push(FailedAspect { label, error }),
        }
    }
    let mut reported = BTreeSet::new();
    for failure in &failed {
        let (reported_errors, _) = failure.split();
        if !reported_errors.is_empty() && reported.insert(reported_errors.to_string()) {
            out.error(reported_errors).await;
        }
    }
    (applied, failed)
}

/// A top-level aspect's application that failed, to a top-level target.
struct FailedAspect {
    label: Label,
    error: skyframe::SkyError,
}

impl FailedAspect {
    /// Its errors, as Bazel reports them: those reported as they happen,
    /// and the last, which Bazel gives as why the build was aborted.
    fn split(&self) -> (&str, &str) {
        bazel_loading::split_reported(self.error.message())
    }
}

/// End a build whose top-level aspect failed, as Bazel does: with what
/// each top-level target would have built, and why the aspect failed.
async fn aspect_analysis_aborted(
    options: &ParsedOptions,
    requested: &RequestedAspects,
    analyzed: &[(Label, Arc<ConfiguredTarget>)],
    failure: &FailedAspect,
    config: ConfigId,
    mapping: &RepoMappingTable,
    out: &Output,
) -> i32 {
    let groups = output_groups(options);
    let targets: Vec<Reported> = analyzed
        .iter()
        .filter(|(_, target)| target.kind.as_str() != "source file")
        .map(|(label, target)| target_result(*label, target, &[], mapping, &groups))
        .collect();
    let show_result: usize = options.get("show_result").and_then(|value| value.parse().ok()).unwrap_or(1);
    let prefix = options.get("symlink_prefix").unwrap_or("bazel-");
    report_results(&targets, &[], show_result, prefix, &config.get().output_directory, out).await;
    if targets.iter().any(|target| matches!(target.outcome, Outcome::Failed)) && !options.flag("verbose_failures", false) {
        out.stderr("Use --verbose_failures to see the command lines of failed build steps.\n").await;
    }
    let (_, why) = failure.split();
    let on = bazel_query::display_label(failure.label, mapping);
    out.error(&format!("Analysis of aspects '{}' failed; build aborted: {why}", aspects_description(requested, &on))).await;
    exit::BUILD_FAILURE
}

/// The aspects `--aspects` names, applied to a target, as Bazel describes
/// them.
fn aspects_description(requested: &RequestedAspects, on: &str) -> String {
    let parameters: Vec<String> = requested.parameters.iter().map(|(name, value)| format!("{name}={value}")).collect();
    format!("[{}] with parameters {{{}}} on {on}", requested.given.join(", "), parameters.join(", "))
}

/// Report the targets that can't be built for the target platform but
/// were named rather than matched by a wildcard, which is an error.
async fn report_explicitly_incompatible(
    analyzed: &[(Label, Arc<ConfiguredTarget>)],
    explicit: &BTreeSet<Label>,
    mapping: &RepoMappingTable,
    out: &Output,
) -> bool {
    let mut errors = false;
    for (label, target) in analyzed {
        let Some(reason) = target.incompatible.as_ref().filter(|_| explicit.contains(label)) else {
            continue;
        };
        errors = true;
        let shown = bazel_query::display_label(*label, mapping);
        let message = format!(
            "Analysis of target '{shown}' failed; build aborted: Target {shown} is incompatible and cannot be built, but was explicitly requested.\nDependency chain:\n{}",
            dependency_chain(reason, mapping)
        );
        out.error(&message).await;
    }
    errors
}

/// An incompatibility's chain of dependencies, as Bazel prints it.
fn dependency_chain(reason: &Incompatibility, mapping: &RepoMappingTable) -> String {
    let mut lines = Vec::with_capacity(reason.chain.len());
    for (index, key) in reason.chain.iter().enumerate() {
        let config = key.config.get();
        let short = &config.checksum[..config.checksum.len().min(6)];
        let mut line = format!("    {} ({short})", bazel_query::display_label(key.label, mapping));
        if index + 1 == reason.chain.len() {
            line.push_str(&format!(
                "   <-- target platform ({}) didn't satisfy constraint {}",
                config.platform, reason.constraint
            ));
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// What the analysis phase is said to analyze: `target //pkg:name`, or
/// `3 targets`.
fn analyzing(labels: &[Label], mapping: &RepoMappingTable) -> String {
    match labels {
        [label] => format!("target {}", bazel_query::display_label(*label, mapping)),
        _ => format!("{} targets", bazel_ui::format_count(labels.len() as u64)),
    }
}

/// Whether Bazel summarizes analysis: unless analysis and execution run
/// together, as they do when actions run, when analysis ends once the last
/// top-level target is analyzed, so never if there are none.
fn summarizes_analysis(execute_actions: bool, labels: &[Label]) -> bool {
    !execute_actions || !labels.is_empty()
}

/// How many targets were analyzed, and of them how many are tests, as
/// Bazel says it.
fn found(analyzed: &[(Label, Arc<ConfiguredTarget>)], testing: bool) -> String {
    let tests = analyzed.iter().filter(|(_, target)| target.is_test).count();
    let others = analyzed.len() - tests;
    match (testing, others) {
        (true, 0) => format!("Found {tests} test {}...", plural(tests, "target")),
        (true, _) => format!("Found {others} {} and {tests} test {}...", plural(others, "target"), plural(tests, "target")),
        (false, _) => format!("Found {} {}...", analyzed.len(), plural(analyzed.len(), "target")),
    }
}

/// Report the targets `--keep_going` skipped, with their warnings, and how
/// many of those `requested` succeeded in the `phase` that just ended, as
/// Bazel does: as analysis ends, or when analysis and execution run
/// together, as the build does.
async fn report_skipped(skipped: &[String], keep_going: bool, succeeded: usize, requested: usize, phase: &str, out: &Output) {
    for warning in skipped {
        out.warning(warning).await;
    }
    if keep_going && 0 < succeeded && succeeded < requested {
        out.info(&format!("{phase} succeeded for only {succeeded} of {requested} top-level targets")).await;
    }
}

/// The warnings for the top-level targets and aspects that failed to
/// analyze, which `--keep_going` lets the build go on without, as Bazel
/// words them: with why, for an aspect.
fn skipped_warnings(failed: &[Failed], failed_aspects: &[FailedAspect], mapping: &RepoMappingTable) -> Vec<String> {
    let warning = |target: &str| format!("errors encountered while analyzing target '{target}', it will not be built.");
    let mut warnings: Vec<String> = failed.iter().map(|failure| warning(&failure.target)).collect();
    for failure in failed_aspects {
        let (_, why) = failure.split();
        warnings.push(format!("{}\n{why}", warning(&bazel_query::display_label(failure.label, mapping))));
    }
    warnings
}

/// What each analyzed target builds at the top level, and whether working
/// it out failed. Incompatible targets, and every target under --nobuild,
/// build nothing.
async fn top_levels(
    session: &Session,
    analyzed: impl Iterator<Item = &Arc<ConfiguredTarget>>,
    groups: &[String],
    execute_actions: bool,
    out: &Output,
) -> (Vec<Option<Arc<TopLevel>>>, bool) {
    let analyzed: Vec<&Arc<ConfiguredTarget>> = analyzed.collect();
    let key = |target: &ConfiguredTarget| TopLevelKey {
        target: ConfiguredTargetKey {
            label: target.label,
            config: target.config,
        },
        groups: groups.to_vec(),
    };
    let keys: Vec<TopLevelKey> = analyzed.iter().map(|target| key(target)).collect();
    let wanted: Vec<bool> = analyzed.iter().map(|target| execute_actions && target.incompatible.is_none()).collect();
    let to_compute: Vec<TopLevelKey> = keys.iter().zip(&wanted).filter(|(_, wanted)| **wanted).map(|(key, _)| key.clone()).collect();
    let mut computed = session.evaluate_many(&to_compute).await.into_iter();
    let mut failed = false;
    let mut top_levels = Vec::with_capacity(keys.len());
    for wanted in wanted {
        let top = match wanted {
            false => None,
            true => computed.next(),
        };
        let top = match top {
            Some(Ok(top)) => Some(top),
            Some(Err(error)) => {
                out.error(error.message()).await;
                failed = true;
                None
            }
            None => None,
        };
        top_levels.push(top);
    }
    (top_levels, failed)
}

/// Write the built files to the output tree, but those already there as
/// they were written.
async fn write_files(backend: &bazel_execution::Backend, written: &Arc<Written>, exec_root: &Path, files: Vec<OutputFile>) -> Result<(), skyframe::SkyError> {
    let span = tracing::info_span!("write_files", files = files.len());
    async {
        let changed = blocking({
            let (written, exec_root) = (written.clone(), exec_root.to_path_buf());
            move || written.changed_outputs(&exec_root, &files)
        })
        .await?;
        bazel_execution::materialize(backend, exec_root, &changed).await?;
        blocking({
            let (written, exec_root) = (written.clone(), exec_root.to_path_buf());
            move || written.record_outputs(&exec_root, &changed)
        })
        .await
    }
    .instrument(span)
    .await
}

/// Run blocking filesystem work off the async runtime.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, skyframe::SkyError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| skyframe::SkyError::transient(format!("output tree task failed: {error}")))
}

/// How many targets' outputs are written to the output tree at once.
const OUTPUT_WRITERS: usize = 16;

/// Write a target's runfiles tree, once its executable is built, unless
/// it's already in place.
async fn write_runfiles_tree(
    session: &Session,
    written: &Arc<Written>,
    target: &ConfiguredTarget,
    files: &[OutputFile],
    tree: &RunfilesTree,
    exec_root: &Path,
    workspace: &Path,
) -> Result<(), skyframe::SkyError> {
    let RunfilesTree { executable, fingerprint } = *tree;
    if !files.iter().any(|file| file.path == executable.exec_path) {
        return Ok(());
    }
    let executable_path = exec_root.join(executable.exec_path.to_string());
    let unchanged = blocking({
        let (written, executable_path) = (written.clone(), executable_path.clone());
        move || written.runfiles_unchanged(&executable_path, fingerprint)
    })
    .await?;
    if unchanged {
        return Ok(());
    }
    let Some(layout) = crate::runfiles::layout(session, target, exec_root, workspace).await? else {
        return Ok(());
    };
    let span = tracing::info_span!("write_runfiles", label = %target.label, entries = layout.entries.len());
    bazel_execution::write_runfiles(layout).instrument(span).await?;
    blocking({
        let written = written.clone();
        move || written.record_runfiles(&executable_path, fingerprint)
    })
    .await
}

fn plural(count: usize, word: &str) -> String {
    match count {
        1 => word.to_string(),
        _ => format!("{word}s"),
    }
}

/// Report a target pattern that couldn't be resolved, as Bazel does: the
/// errors of the packages it loaded and those resolving found, then why
/// the pattern failed.
async fn pattern_failed(universe: &Universe<&Session>, pattern: &str, message: &str, out: &Output) -> i32 {
    for error in universe.errors().await {
        out.error(&error).await;
    }
    let (_, message) = bazel_loading::split_reported(message);
    out.warning("Target pattern parsing failed.").await;
    out.error(&format!("Skipping '{pattern}': {message}")).await;
    out.error(message).await;
    exit::BUILD_FAILURE
}

/// The targets the command line's patterns name, and those of them named
/// other than by a wildcard. Wildcards skip targets tagged `manual`, and
/// test suites are their tests, but for a query's universe; patterns
/// starting with `-` are subtracted.
async fn targets(
    context: &CommandContext<'_>,
    session: &Session,
    options: &ParsedOptions,
    main_mapping: &Arc<RepoMappingTable>,
    purpose: Purpose,
    out: &Output,
) -> Result<Requested, i32> {
    let package = working_package(&context.workspace_root, &context.working_directory);
    let keep_going = options.flag("keep_going", false);
    let mut skipped = Vec::new();
    let query_options = QueryOptions {
        implicit_deps: true,
        tool_deps: true,
        keep_going: false,
    };
    let universe = Universe::new(session, query_options, package, main_mapping.clone(), context.workspace_root.clone());
    let mut included = TargetSet::new();
    let mut excluded = TargetSet::new();
    let mut explicit = BTreeSet::new();
    for pattern in &options.residue {
        let (negative, text) = match pattern.strip_prefix('-') {
            Some(text) => (true, text),
            None => (false, pattern.as_str()),
        };
        let resolved = universe.resolve(text).await.map_err(|error| error.message);
        let mut labels = match (resolved, universe.pattern_failure()) {
            (Ok(labels), None) => labels,
            (Err(message), _) | (Ok(_), Some(message)) if keep_going => {
                skipped.push((pattern.clone(), message));
                continue;
            }
            (Err(message), _) | (Ok(_), Some(message)) => return Err(pattern_failed(&universe, pattern, &message, out).await),
        };
        let wildcard = is_wildcard(text, package, main_mapping);
        if wildcard && purpose != Purpose::Query {
            labels = buildable_by_wildcard(&universe, labels).await;
        }
        if !wildcard && !negative {
            explicit.extend(labels.iter().copied());
        }
        if purpose != Purpose::Query {
            labels = match expand_test_suites(&universe, labels).await {
                Ok(labels) => labels,
                Err(message) => return Err(pattern_failed(&universe, pattern, &message, out).await),
            };
        }
        match negative {
            true => excluded.extend(labels),
            false => included.extend(labels),
        }
    }
    if options.residue.is_empty() {
        out.error("No targets specified").await;
        return Err(exit::BUILD_FAILURE);
    }
    if !skipped.is_empty() {
        patterns_skipped(&universe, &skipped, out).await;
    }
    let labels: Vec<Label> = included.into_iter().filter(|label| !excluded.contains(label)).collect();
    let labels = match purpose {
        Purpose::Query => labels,
        Purpose::Build | Purpose::Test => filter_by_tags(&universe, labels, options, purpose).await,
    };
    if let Some(events) = out.build_events() {
        events.patterns_expanded(expanded_targets(&universe, &labels, main_mapping).await);
    }
    Ok(Requested {
        labels,
        explicit,
        patterns_failed: !skipped.is_empty(),
    })
}

/// The targets the patterns expanded to, as build events name them, with
/// what the events say of them.
async fn expanded_targets(universe: &Universe<&Session>, labels: &[Label], mapping: &RepoMappingTable) -> Vec<(String, TargetAttrs)> {
    let mut expanded = Vec::with_capacity(labels.len());
    for &label in labels {
        let shown = bazel_query::display_label(label, mapping);
        let attrs = match universe.target(label).await {
            Ok(target) => match target.target() {
                Target::Rule(rule) => TargetAttrs::of(Some(rule), is_test_kind(target.package().rule_class(rule).kind.as_str())),
                _ => TargetAttrs::default(),
            },
            Err(_) => TargetAttrs::default(),
        };
        expanded.push((shown, attrs));
    }
    expanded
}

/// The targets to build of those the patterns name: those
/// `--build_tag_filters` selects, and the tests to run, or with
/// `--build_tests_only` only the tests.
async fn filter_by_tags(universe: &Universe<&Session>, labels: Vec<Label>, options: &ParsedOptions, purpose: Purpose) -> Vec<Label> {
    let build_filters = options.get("build_tag_filters").unwrap_or_default();
    let tests_only = options.flag("build_tests_only", false);
    if build_filters.is_empty() && !tests_only {
        return labels;
    }
    let mut selected = Vec::with_capacity(labels.len());
    for label in labels {
        let Ok(target) = universe.target(label).await else {
            selected.push(label);
            continue;
        };
        let rule = target.target().as_rule();
        let kind = rule.map(|rule| target.package().rule_class(rule).kind);
        let is_test = kind.is_some_and(|kind| is_test_kind(kind.as_str()));
        let tested = is_test && test_filter(options).passes(rule, kind.map(|kind| kind.as_str()).unwrap_or_default());
        let built = match tests_only {
            true => tested,
            false => passes_tag_filters(rule, build_filters) || (purpose == Purpose::Test && tested),
        };
        if built {
            selected.push(label);
        }
    }
    selected
}

/// The tests the test filters select, or why the filters are invalid.
pub fn parse_test_filter(options: &ParsedOptions) -> Result<TestFilter, String> {
    let value = |name: &str| options.get(name).unwrap_or_default();
    TestFilter::new(value("test_tag_filters"), value("test_size_filters"), value("test_timeout_filters"), value("test_lang_filters"))
}

/// The tests the test filters select, which [`parse`] checked.
pub fn test_filter(options: &ParsedOptions) -> TestFilter {
    parse_test_filter(options).unwrap_or_default()
}

/// The targets the command line's patterns name.
struct Requested {
    labels: Vec<Label>,
    /// Those named other than by a wildcard.
    explicit: BTreeSet<Label>,
    /// Whether `--keep_going` went past patterns that couldn't be resolved.
    patterns_failed: bool,
}

/// Report the target patterns `--keep_going` went past, as Bazel does: the
/// errors that led to each, then that it's skipped.
async fn patterns_skipped(universe: &Universe<&Session>, skipped: &[(String, String)], out: &Output) {
    for error in universe.errors().await {
        out.error(&error).await;
    }
    out.warning("Target pattern parsing failed.").await;
    for (pattern, message) in skipped {
        let (_, message) = bazel_loading::split_reported(message);
        out.error(&format!("Skipping '{pattern}': {message}")).await;
    }
}

fn is_wildcard(text: &str, package: bazel_label::PackageId, mapping: &RepoMappingTable) -> bool {
    let context = LabelContext::new(package, mapping);
    !matches!(TargetPattern::parse(text, &context), Ok(TargetPattern::Target(_)))
}

/// Rules that wildcard patterns skip, as Bazel's do: they configure the
/// build rather than produce anything.
const NOT_BUILT_BY_WILDCARDS: &[&str] = &["config_setting", "constraint_setting", "constraint_value", "platform", "toolchain"];

/// The targets a wildcard pattern builds: not those tagged `manual`, nor
/// the rules that only configure the build.
async fn buildable_by_wildcard(universe: &Universe<&Session>, labels: TargetSet) -> TargetSet {
    let mut kept = TargetSet::new();
    for label in labels {
        let Ok(target) = universe.target(label).await else {
            continue;
        };
        if let Target::Rule(rule) = target.target() {
            let kind = target.package().rule_class(rule).kind;
            if bazel_packages::is_manual(rule) || NOT_BUILT_BY_WILDCARDS.contains(&kind.as_str()) {
                continue;
            }
        }
        kept.insert(label);
    }
    kept
}



/// Replace each `test_suite` with the tests it names, or with its
/// package's tests that its tags select when it names none.
async fn expand_test_suites(universe: &Universe<&Session>, labels: TargetSet) -> Result<TargetSet, String> {
    let mut expanded = TargetSet::new();
    let mut pending: Vec<Label> = labels.into_iter().collect();
    let mut seen = BTreeSet::new();
    while let Some(label) = pending.pop() {
        if !seen.insert(label) {
            continue;
        }
        let target = universe.target(label).await.map_err(|error| error.message)?;
        let Target::Rule(rule) = target.target() else {
            expanded.insert(label);
            continue;
        };
        let package = target.package();
        if package.rule_class(rule).kind.as_str() != "test_suite" {
            expanded.insert(label);
            continue;
        }
        let tests = match rule.attr(Symbol::intern("tests")) {
            Some(AttrValue::LabelList(tests)) if !tests.is_empty() => tests.clone(),
            _ => package.implicit_tests(rule),
        };
        pending.extend(tests);
    }
    Ok(expanded)
}

/// The directory, such as `k8-fastbuild`, that the options' top-level
/// configuration puts its outputs in under `bazel-out`.
pub async fn output_directory(context: &CommandContext<'_>, session: &Session, options: &ParsedOptions) -> Result<String, String> {
    let main_mapping = session.evaluate(&RepoMappingKey(RepoName::MAIN)).await.map_err(|error| error.message().to_string())?;
    let config = configuration(context, session, options, &main_mapping.0).await?;
    Ok(config.get().output_directory.clone())
}

/// The configuration targets are built in, from the options. As in Bazel,
/// targets are built for the host platform unless `--platforms` says.
async fn configuration(context: &CommandContext<'_>, session: &Session, options: &ParsedOptions, main_mapping: &RepoMappingTable) -> Result<ConfigId, String> {
    let parse_label = |text: &str| {
        let label_context = LabelContext::new(bazel_label::PackageId::new(RepoName::MAIN, intern::PathId::ROOT), main_mapping);
        Label::parse(text, &label_context).map_err(|error| error.to_string())
    };
    let mut build = BuildOptions::default();
    for option in bazel_config::command_line_options() {
        for text in options.all(option.name) {
            build.apply(option.name, text, &parse_label).map_err(|error| error.to_string())?;
        }
    }
    if build.labels("platforms").is_empty() {
        let host = build.label("host_platform").into_iter().collect();
        build.set("platforms", OptionValue::Labels(host)).map_err(|error| error.to_string())?;
    }
    build.action_env = action_env(context, options, "action_env");
    build.host_action_env = action_env(context, options, "host_action_env");
    set_starlark_flags(context, session, &mut build, options, main_mapping, &parse_label).await?;
    Ok(ConfigId::intern(&build))
}

/// Set the Starlark build settings the command line gives, `--//pkg:flag=value`,
/// as their types read them. Each use of a setting that takes many values
/// adds to it; for others, the last use wins.
async fn set_starlark_flags(
    context: &CommandContext<'_>,
    session: &Session,
    build: &mut BuildOptions,
    options: &ParsedOptions,
    main_mapping: &RepoMappingTable,
    parse_label: LabelParser<'_>,
) -> Result<(), String> {
    let mut given: Vec<(BuildSetting, FlagValue)> = Vec::new();
    for flag in options.all(bazel_options::STARLARK_FLAG) {
        let (name, text) = flag.split_once('=').unwrap_or((flag, "true"));
        let label = parse_label(name)?;
        let Ok(Some(setting)) = bazel_analysis::settings::build_setting(session, label).await else {
            return Err(unrecognized_flag(context, session, main_mapping, name, label).await);
        };
        if !setting.spec.flag {
            return Err(format!("Unrecognized option: {name}={text}"));
        }
        let parsed = FlagValue::parse(text, &setting.default, setting.spec.multiple, parse_label);
        let value = parsed.map_err(|_| format!("While parsing option {name}={text}: '{text}' is not a {}", setting.default.type_name()))?;
        add_flag(&mut given, setting, value);
    }
    for (setting, value) in given {
        build.set_flag(setting.actual, value, &setting.default);
    }
    Ok(())
}

/// The error for a flag that names no build setting: why the target it
/// names doesn't exist, or that it isn't one, as Bazel says.
async fn unrecognized_flag(context: &CommandContext<'_>, session: &Session, main_mapping: &RepoMappingTable, name: &str, label: Label) -> String {
    let package = working_package(&context.workspace_root, &context.working_directory);
    let query_options = QueryOptions {
        implicit_deps: true,
        tool_deps: true,
        keep_going: false,
    };
    let universe = Universe::new(session, query_options, package, Arc::new(main_mapping.clone()), context.workspace_root.clone());
    match universe.target(label).await {
        Err(error) => format!("Skipping '{name}': {}", error.message),
        Ok(_) => format!("Unrecognized option: {name}"),
    }
}

/// Give a build setting another value: added to its others if it takes
/// many, or else replacing them.
fn add_flag(given: &mut Vec<(BuildSetting, FlagValue)>, setting: BuildSetting, value: FlagValue) {
    let existing = given.iter_mut().find(|(existing, _)| existing.actual == setting.actual);
    match (existing, value) {
        (Some((existing, FlagValue::Strings(values))), FlagValue::Strings(more)) if existing.spec.multiple => values.extend(more),
        (Some((_, current)), value) => *current = value,
        (None, value) => given.push((setting, value)),
    }
}

/// The environment of actions that use the default shell environment, as
/// Bazel gives it: a fixed `PATH`, or with
/// `--noincompatible_strict_action_env` the client's, and the variables of
/// `option`, `action_env` or `host_action_env`. A variable taken from the
/// client that the client doesn't set is left out.
fn action_env(context: &CommandContext<'_>, options: &ParsedOptions, option: &str) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    let path = match options.flag("incompatible_strict_action_env", true) {
        true => Some(STRICT_PATH.to_string()),
        false => context.env.get("PATH").cloned(),
    };
    if let Some(path) = path {
        env.push(("PATH".to_string(), path));
    }
    for entry in options.all(option) {
        let (name, value) = match entry.split_once('=') {
            Some((name, value)) => (name.to_string(), Some(value.to_string())),
            None => (entry.to_string(), context.env.get(entry).cloned()),
        };
        env.retain(|(existing, _)| *existing != name);
        if let Some(value) = value {
            env.push((name, value));
        }
    }
    env.sort();
    env
}

/// The `PATH` actions get with `--incompatible_strict_action_env`, as
/// Bazel gives them, rather than the client's.
const STRICT_PATH: &str = "/bin:/usr/bin:/usr/local/bin";

/// Analyze the targets, sending each compatible one to `analyzed` as soon
/// as it's done, and reporting each error once, however many targets it
/// fails. Returns those analyzed, and those that failed to, as messages
/// show them.
async fn analyze(
    session: &Session,
    labels: &[Label],
    config: ConfigId,
    mapping: &RepoMappingTable,
    analyzed: Option<UnboundedSender<Arc<ConfiguredTarget>>>,
    out: &Output,
) -> (Vec<(Label, Arc<ConfiguredTarget>)>, Vec<Failed>) {
    let mut analyses: Vec<BoxFuture<'_, _>> = Vec::with_capacity(labels.len());
    for label in labels {
        analyses.push(Box::pin(analyze_one(session, ConfiguredTargetKey { label: *label, config }, analyzed.clone())));
    }
    drop(analyzed);
    let results = futures::future::join_all(analyses).await;
    let mut analyzed = Vec::new();
    let mut failed = Vec::new();
    let mut reported = BTreeSet::new();
    for (label, result) in labels.iter().zip(results) {
        match result {
            Ok(target) => analyzed.push((*label, target)),
            Err(error) => {
                failed.push(Failed {
                    target: bazel_query::display_label(*label, mapping),
                    in_dependency: error.origin().is_some_and(|origin| origin != label.to_string()),
                });
                if reported.insert(error.message().to_string()) {
                    out.error(error.message()).await;
                }
            }
        }
    }
    (analyzed, failed)
}

/// A top-level target whose analysis failed, as messages show it, and
/// whether a dependency's analysis is what failed.
struct Failed {
    target: String,
    in_dependency: bool,
}

/// The error that ends a build when a top-level target's analysis fails,
/// as Bazel words it: with why only if a dependency failed.
fn build_aborted(failed: &Failed) -> String {
    let aborted = format!("Analysis of target '{}' failed; build aborted", failed.target);
    match failed.in_dependency {
        true => format!("{aborted}: Analysis failed"),
        false => aborted,
    }
}

async fn analyze_one(
    session: &Session,
    key: ConfiguredTargetKey,
    analyzed: Option<UnboundedSender<Arc<ConfiguredTarget>>>,
) -> Result<Arc<ConfiguredTarget>, skyframe::SkyError> {
    let key = bazel_analysis::transition::configured_key(session, key.label, key.config).await?;
    let result = session.evaluate(&key).await;
    if let (Ok(target), Some(analyzed)) = (&result, analyzed) {
        if target.incompatible.is_none() {
            let _ = analyzed.unbounded_send(target.clone());
        }
    }
    result
}


/// Run the actions that generate artifacts, returning their outputs and
/// whether any failed.
pub async fn execute(session: &Session, artifacts: &[Artifact], out: &Output) -> (Vec<OutputFile>, bool) {
    let span = tracing::info_span!("generating_actions", artifacts = artifacts.len());
    let keys = generating_actions(session, artifacts).instrument(span).await;
    let mut wanted: BTreeMap<ActionKey, BTreeSet<PathId>> = BTreeMap::new();
    for (key, artifact) in &keys {
        wanted.entry(*key).or_default().insert(artifact.exec_path);
    }
    let unique: Vec<ActionKey> = wanted.keys().copied().collect();
    let results = session.evaluate_many(&unique).await;
    let mut files = Vec::new();
    let mut errors = false;
    // A failed action fails every action that needs it, with its error.
    let mut reported = BTreeSet::new();
    for (key, result) in unique.iter().zip(results) {
        match result {
            Ok(outputs) => {
                let paths = &wanted[key];
                files.extend(outputs.files.iter().filter(|file| paths.contains(&file.path)).copied());
            }
            Err(error) => {
                errors = true;
                if reported.insert(error.message().to_string()) {
                    out.error(error.message()).await;
                }
            }
        }
    }
    (files, errors)
}

/// The actions that generate derived artifacts, with each artifact.
async fn generating_actions(session: &Session, artifacts: &[Artifact]) -> Vec<(ActionKey, Artifact)> {
    let owner = |artifact: &Artifact| {
        artifact.config.map(|config| ConfiguredTargetKey {
            label: artifact.owner,
            config,
        })
    };
    let owners: Vec<ConfiguredTargetKey> = artifacts.iter().filter_map(owner).collect::<BTreeSet<_>>().into_iter().collect();
    let targets = session.evaluate_many(&owners).await;
    let targets: HashMap<ConfiguredTargetKey, Arc<ConfiguredTarget>> =
        owners.into_iter().zip(targets).filter_map(|(key, target)| Some((key, target.ok()?))).collect();
    let mut keys = Vec::with_capacity(artifacts.len());
    for artifact in artifacts {
        let Some(owner) = owner(artifact) else {
            continue;
        };
        let Some((index, _)) = targets.get(&owner).and_then(|target| target.generating_action(artifact.exec_path)) else {
            continue;
        };
        keys.push((ActionKey { owner, index }, *artifact));
    }
    keys
}

/// Print what the build found and what each target built, as Bazel does
/// once it's built them, and make the convenience symlinks.
pub async fn report(context: &CommandContext<'_>, options: &ParsedOptions, built: &Built, out: &Output) {
    let prefix = options.get("symlink_prefix").unwrap_or("bazel-");
    let config = built.config.get();
    if let Err(error) = convenience_symlinks(&context.workspace_root, &built.exec_root, prefix, &config.output_directory) {
        out.warning(&format!("failed to create convenience symlinks: {error}")).await;
    }
    let executed = options.flag("build", true);
    let (failed, failed_aspects) = match executed {
        true => (built.unbuilt, built.unbuilt_aspects),
        false => (0, 0),
    };
    let keep_going = options.flag("keep_going", false);
    if executed {
        report_skipped(&built.skipped, keep_going, built.targets.len() - failed, built.requested, "Build", out).await;
    }
    let any_failed = failed + failed_aspects > 0;
    if keep_going || !any_failed {
        out.info(&built.found).await;
    }
    let show_result: usize = options.get("show_result").and_then(|value| value.parse().ok()).unwrap_or(1);
    if executed {
        let targets: Vec<Reported> = built
            .targets
            .iter()
            .filter(|(_, target, _)| target.kind.as_str() != "source file")
            .map(|(label, target, files)| target_result(*label, target, files, &built.main_mapping, &built.groups))
            .collect();
        let aspects: Vec<Reported> = built.aspects.iter().map(|aspect| aspect_result(aspect, built)).collect();
        report_results(&targets, &aspects, show_result, prefix, &config.output_directory, out).await;
    }
    if any_failed && !options.flag("verbose_failures", false) {
        out.stderr("Use --verbose_failures to see the command lines of failed build steps.\n").await;
    }
}

/// How building a top-level target, or an aspect applied to one, went.
enum Outcome {
    /// It's incompatible with the target platform.
    Skipped,
    /// Its files of the requested output groups were built, those shown.
    Built(Vec<Artifact>),
    Failed,
}

/// What a top-level target, or an aspect applied to one, is reported as:
/// `Target //pkg:name` or `Aspect //pkg:defs.bzl%name of //pkg:name`.
struct Reported {
    shown: String,
    outcome: Outcome,
}

/// Report what each top-level target and aspect built, as Bazel does:
/// unless more than `show_result` have files to show or failed, in which
/// case none are; and without those with nothing to build, unless every
/// one fits.
async fn report_results(targets: &[Reported], aspects: &[Reported], show_result: usize, prefix: &str, output_directory: &str, out: &Output) {
    let essential = targets.iter().chain(aspects).filter(|result| !matches!(&result.outcome, Outcome::Built(files) if files.is_empty())).count();
    if essential > show_result {
        return;
    }
    let omit_nothing_to_build = targets.len() + aspects.len() > show_result;
    let mut text = String::new();
    for result in targets.iter().filter(|result| matches!(result.outcome, Outcome::Skipped)) {
        text.push_str(&format!("{} was skipped\n", result.shown));
    }
    for results in [targets, aspects] {
        for result in results.iter() {
            let Outcome::Built(files) = &result.outcome else {
                continue;
            };
            if files.is_empty() && !omit_nothing_to_build {
                text.push_str(&format!("{} up-to-date (nothing to build)\n", result.shown));
            }
            if files.is_empty() {
                continue;
            }
            text.push_str(&format!("{} up-to-date:\n", result.shown));
            for artifact in files {
                text.push_str(&format!("  {}\n", display_path(artifact, prefix, output_directory)));
            }
        }
        for result in results.iter().filter(|result| matches!(result.outcome, Outcome::Failed)) {
            text.push_str(&format!("{} failed to build\n", result.shown));
        }
    }
    out.stderr(&text).await;
}

fn target_result(label: Label, target: &ConfiguredTarget, files: &[OutputFile], mapping: &RepoMappingTable, groups: &[String]) -> Reported {
    let shown = format!("Target {}", bazel_query::display_label(label, mapping));
    let outcome = match target.incompatible {
        Some(_) => Outcome::Skipped,
        None => outcome(target, files, groups),
    };
    Reported { shown, outcome }
}

fn aspect_result(aspect: &BuiltAspect, built: &Built) -> Reported {
    let shown = format!("Aspect {} of {}", aspect.aspect, bazel_query::display_label(aspect.label, &built.main_mapping));
    Reported {
        shown,
        outcome: outcome(&aspect.target, &aspect.files, &built.groups),
    }
}

/// Whether every file of a target's requested output groups was built,
/// and those generated, which a build shows.
fn outcome(target: &ConfiguredTarget, files: &[OutputFile], groups: &[String]) -> Outcome {
    let generated: Vec<Artifact> = shown_files(target, groups).into_iter().filter(|artifact| !artifact.is_source()).collect();
    match generated.iter().all(|artifact| files.iter().any(|file| file.path == artifact.exec_path)) {
        true => Outcome::Built(generated),
        false => Outcome::Failed,
    }
}

/// The files of a target's requested output groups.
fn shown_files(target: &ConfiguredTarget, groups: &[String]) -> Vec<Artifact> {
    let providers = target.providers().ok();
    match providers.as_ref().and_then(|providers| providers.get()) {
        Some(value) => crate::top_level::shown(value, groups),
        None => Vec::new(),
    }
}

/// An output's path as Bazel prints it: through the `bazel-bin` symlink.
pub fn display_path(artifact: &Artifact, prefix: &str, output_directory: &str) -> String {
    let path = artifact.exec_path.to_string();
    let bin = format!("bazel-out/{output_directory}/bin/");
    match path.strip_prefix(&bin) {
        Some(rest) if prefix != "/" => format!("{prefix}bin/{rest}"),
        _ => path,
    }
}

/// Make `bazel-bin` and the other symlinks in the workspace, unless the
/// prefix is `/`.
fn convenience_symlinks(workspace: &Path, exec_root: &Path, prefix: &str, output_directory: &str) -> std::io::Result<()> {
    if prefix == "/" {
        return Ok(());
    }
    let output_path = exec_root.join("bazel-out");
    std::fs::create_dir_all(output_path.join(output_directory).join("bin"))?;
    std::fs::create_dir_all(output_path.join(output_directory).join("testlogs"))?;
    let workspace_name = workspace.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let links = [
        ("bin".to_string(), output_path.join(output_directory).join("bin")),
        ("testlogs".to_string(), output_path.join(output_directory).join("testlogs")),
        ("out".to_string(), output_path.clone()),
        (workspace_name, exec_root.to_path_buf()),
    ];
    for (name, target) in links {
        let link = workspace.join(format!("{prefix}{name}"));
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::read_link(&link).is_ok_and(|existing| existing == target) {
            continue;
        }
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&target, &link)?;
    }
    Ok(())
}
