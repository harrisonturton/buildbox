//! `bb build`: analyze targets, run the actions that build their files,
//! and write those files to the output tree.

use crate::command::{CommandContext, exit, working_package};
use crate::output::Output;
use bazel_analysis::{Artifact, ConfiguredTarget, ConfiguredTargetKey, FileValue, Incompatibility, Runfiles, TargetValue};
use bazel_config::{BuildOptions, ConfigId};
use bazel_execution::{ActionKey, OutputFile};
use bazel_label::{Label, LabelContext, RepoMappingTable, RepoName, TargetPattern};
use bazel_loading::RepoMappingKey;
use bazel_options::{OptionKind, OptionSpec, ParsedOptions};
use bazel_packages::{AttrValue, Target};
use bazel_query::{QueryOptions, TargetSet, Universe};
use bazel_starlark::depset::Depset;
use intern::Symbol;
use skyframe::Session;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// The options `build` and the commands built on it take.
pub fn build_spec() -> OptionSpec {
    use OptionKind::*;
    OptionSpec::new(&[
        ("compilation_mode", Value),
        ("cpu", Value),
        ("platforms", Value),
        ("host_platform", Value),
        ("define", Multi),
        ("action_env", Multi),
        ("copt", Multi),
        ("cxxopt", Multi),
        ("conlyopt", Multi),
        ("linkopt", Multi),
        ("host_copt", Multi),
        ("features", Multi),
        ("stamp", Bool),
        ("keep_going", Bool),
        ("symlink_prefix", Value),
        ("show_result", Value),
        ("output_groups", Multi),
        ("build_tests_only", Bool),
        ("verbose_failures", Bool),
        ("jobs", Value),
        ("test_output", Value),
        ("test_filter", Value),
        ("test_arg", Multi),
        ("test_env", Multi),
        ("test_timeout", Value),
        ("test_tag_filters", Value),
        ("build_tag_filters", Value),
        ("run_under", Value),
        ("script_path", Value),
        ("nocache_test_results", Bool),
        ("cache_test_results", Bool),
        ("runs_per_test", Value),
        ("flaky_test_attempts", Value),
        ("extra_toolchains", Multi),
        ("extra_execution_platforms", Multi),
        ("remote_executor", Value),
        ("remote_cache", Value),
        ("disk_cache", Value),
        ("color", Value),
        ("curses", Value),
        ("show_progress", Bool),
        ("announce_rc", Bool),
        ("enable_bzlmod", Bool),
        ("registry", Multi),
        ("lockfile_mode", Value),
    ])
}

/// What a build analyzed and built.
pub struct Built {
    pub main_mapping: Arc<RepoMappingTable>,
    pub config: ConfigId,
    /// Each top-level target, with the files it built.
    pub targets: Vec<(Label, Arc<ConfiguredTarget>, Vec<OutputFile>)>,
    pub exec_root: PathBuf,
    pub errors: bool,
}

pub async fn build(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    let started = Instant::now();
    let options = match parse(context, "build", args, out).await {
        Ok(options) => options,
        Err(code) => return code,
    };
    let workspace = context.server.workspaces().get(&context.workspace_root);
    workspace.refresh().await;
    let session = workspace.graph.session().await;
    let built = match run(context, &session, &options, out).await {
        Ok(built) => built,
        Err(code) => return failed(code, started, out).await,
    };
    report(context, &options, &built, out).await;
    out.info(&format!("Elapsed time: {:.3}s", started.elapsed().as_secs_f64())).await;
    if built.errors {
        out.error("Build did NOT complete successfully").await;
        return exit::BUILD_FAILURE;
    }
    out.info("Build completed successfully").await;
    exit::SUCCESS
}

/// End a command whose build failed before it finished, as Bazel does.
pub async fn failed(code: i32, started: Instant, out: &Output) -> i32 {
    if code == exit::BUILD_FAILURE {
        out.info(&format!("Elapsed time: {:.3}s", started.elapsed().as_secs_f64())).await;
        out.error("Build did NOT complete successfully").await;
    }
    code
}

/// Parse a build command's options.
pub async fn parse(context: &CommandContext<'_>, command: &str, args: &[String], out: &Output) -> Result<ParsedOptions, i32> {
    let all_args = bazel_options::command_args(&context.rc, command, args);
    match bazel_options::parse(&all_args, &build_spec(), &context.rc, command) {
        Ok(options) => Ok(options),
        Err(error) => {
            out.error(&error.to_string()).await;
            Err(exit::COMMAND_LINE_ERROR)
        }
    }
}

/// Analyze the targets the options name and build their files.
pub async fn run(context: &CommandContext<'_>, session: &Session, options: &ParsedOptions, out: &Output) -> Result<Built, i32> {
    run_command(context, session, options, false, out).await
}

/// [`run`], for a command that tests, which reports its test targets.
pub async fn run_command(
    context: &CommandContext<'_>,
    session: &Session,
    options: &ParsedOptions,
    testing: bool,
    out: &Output,
) -> Result<Built, i32> {
    let main_mapping = match session.evaluate(&RepoMappingKey(RepoName::MAIN)).await {
        Ok(mapping) => Arc::new(mapping.0.clone()),
        Err(error) => {
            out.error(error.message()).await;
            return Err(exit::BUILD_FAILURE);
        }
    };
    let keep_going = options.flag("keep_going", false);
    let (labels, explicit) = match targets(context, session, options, &main_mapping).await {
        Ok(requested) => requested,
        Err(message) => {
            out.error(&message).await;
            return Err(exit::BUILD_FAILURE);
        }
    };
    let config = match configuration(context, options, &main_mapping) {
        Ok(config) => config,
        Err(message) => {
            out.error(&message).await;
            return Err(exit::COMMAND_LINE_ERROR);
        }
    };
    let (analyzed, mut errors) = analyze(session, &labels, config, &main_mapping, out).await;
    let incompatible = report_explicitly_incompatible(&analyzed, &explicit, &main_mapping, out).await;
    if errors && !keep_going {
        out.error("Analysis of target failed; build aborted").await;
        return Err(exit::BUILD_FAILURE);
    }
    if incompatible && !keep_going {
        return Err(exit::BUILD_FAILURE);
    }
    errors |= incompatible;
    report_analysis(&analyzed, &main_mapping, testing, out).await;

    let exec_root = crate::commands::output_base_for(context.server, &context.workspace_root).join("execroot/_main");
    let backend = context.server.backend();
    let mut targets = Vec::new();
    for (label, target) in analyzed {
        if target.incompatible.is_some() {
            targets.push((label, target, Vec::new()));
            continue;
        }
        let artifacts = top_level_artifacts(&target);
        let (files, failed) = execute(session, &artifacts, out).await;
        errors |= failed;
        if let Some(backend) = &backend {
            if let Err(error) = write_outputs(session, backend, &target, &files, &exec_root, &context.workspace_root).await {
                out.error(error.message()).await;
                errors = true;
            }
        }
        targets.push((label, target, files));
        if errors && !keep_going {
            break;
        }
    }
    Ok(Built {
        main_mapping,
        config,
        targets,
        exec_root,
        errors,
    })
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
                config.options.platform, reason.constraint
            ));
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// Say what was analyzed, as Bazel does.
async fn report_analysis(analyzed: &[(Label, Arc<ConfiguredTarget>)], mapping: &RepoMappingTable, testing: bool, out: &Output) {
    match analyzed {
        [(label, _)] => out.info(&format!("Analyzed target {}.", bazel_query::display_label(*label, mapping))).await,
        _ => out.info(&format!("Analyzed {} targets.", analyzed.len())).await,
    }
    let tests = analyzed.iter().filter(|(_, target)| target.is_test).count();
    let others = analyzed.len() - tests;
    match testing {
        true => out.info(&format!("Found {others} {} and {tests} test {}...", plural(others, "target"), plural(tests, "target"))).await,
        false => out.info(&format!("Found {} {}...", analyzed.len(), plural(analyzed.len(), "target"))).await,
    }
}

/// Write a target's files to the output tree, and its executable's
/// runfiles tree.
async fn write_outputs(
    session: &Session,
    backend: &bazel_execution::Backend,
    target: &ConfiguredTarget,
    files: &[OutputFile],
    exec_root: &Path,
    workspace: &Path,
) -> Result<(), skyframe::SkyError> {
    bazel_execution::materialize(backend, exec_root, files).await?;
    if let Some(layout) = crate::runfiles::layout(session, target, exec_root, workspace).await? {
        bazel_execution::write_runfiles(layout).await?;
    }
    Ok(())
}

fn plural(count: usize, word: &str) -> String {
    match count {
        1 => word.to_string(),
        _ => format!("{word}s"),
    }
}

/// The targets the command line's patterns name, and those of them named
/// other than by a wildcard. Wildcards skip targets tagged `manual`;
/// patterns starting with `-` are subtracted.
async fn targets(
    context: &CommandContext<'_>,
    session: &Session,
    options: &ParsedOptions,
    main_mapping: &Arc<RepoMappingTable>,
) -> Result<(Vec<Label>, BTreeSet<Label>), String> {
    let package = working_package(&context.workspace_root, &context.working_directory);
    let query_options = QueryOptions {
        implicit_deps: true,
        tool_deps: true,
        keep_going: false,
    };
    let universe = Universe::new(session, query_options, package, main_mapping.clone());
    let mut included = TargetSet::new();
    let mut excluded = TargetSet::new();
    let mut explicit = BTreeSet::new();
    for pattern in &options.residue {
        let (negative, text) = match pattern.strip_prefix('-') {
            Some(text) => (true, text),
            None => (false, pattern.as_str()),
        };
        let mut labels = universe.resolve(text).await.map_err(|error| error.0)?;
        let wildcard = is_wildcard(text, package, main_mapping);
        if wildcard {
            labels = buildable_by_wildcard(&universe, labels).await;
        }
        if !wildcard && !negative {
            explicit.extend(labels.iter().copied());
        }
        labels = expand_test_suites(&universe, labels).await?;
        match negative {
            true => excluded.extend(labels),
            false => included.extend(labels),
        }
    }
    if options.residue.is_empty() {
        return Err("No targets specified".to_string());
    }
    let labels = included.into_iter().filter(|label| !excluded.contains(label)).collect();
    Ok((labels, explicit))
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
            if is_manual(rule) || NOT_BUILT_BY_WILDCARDS.contains(&kind.as_str()) {
                continue;
            }
        }
        kept.insert(label);
    }
    kept
}

fn is_manual(rule: &bazel_packages::Rule) -> bool {
    matches!(rule.attr(Symbol::intern("tags")), Some(AttrValue::StringList(tags)) if tags.iter().any(|tag| tag == "manual"))
}

/// Replace each `test_suite` with the tests it names, or with every test of
/// its package when it names none.
async fn expand_test_suites(universe: &Universe<&Session>, labels: TargetSet) -> Result<TargetSet, String> {
    let mut expanded = TargetSet::new();
    let mut pending: Vec<Label> = labels.into_iter().collect();
    let mut seen = BTreeSet::new();
    while let Some(label) = pending.pop() {
        if !seen.insert(label) {
            continue;
        }
        let target = universe.target(label).await.map_err(|error| error.0)?;
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
            _ => package_tests(universe, package).await,
        };
        pending.extend(tests);
    }
    Ok(expanded)
}

/// The tests of a package that aren't tagged `manual`.
async fn package_tests(universe: &Universe<&Session>, package: &bazel_packages::Package) -> Vec<Label> {
    let mut tests = Vec::new();
    for rule in package.rules() {
        let Ok(class) = universe.rule_class(package.rule_class(rule)).await else {
            continue;
        };
        if class.test && !is_manual(rule) {
            tests.push(package.label(rule.name));
        }
    }
    tests
}

/// The configuration targets are built in, from the options.
fn configuration(context: &CommandContext<'_>, options: &ParsedOptions, main_mapping: &RepoMappingTable) -> Result<ConfigId, String> {
    let parse_label = |text: &str| {
        let label_context = LabelContext::new(bazel_label::PackageId::new(RepoName::MAIN, intern::PathId::ROOT), main_mapping);
        Label::parse(text, &label_context).map_err(|error| error.to_string())
    };
    let host = match options.get("host_platform") {
        Some(text) => parse_label(text)?,
        None => Label::parse_canonical("@@platforms//host:host").map_err(|error| error.to_string())?,
    };
    let mut build = BuildOptions::host_default(host);
    if let Some(platform) = options.get("platforms").and_then(|platforms| platforms.split(',').next()) {
        build.platform = parse_label(platform)?;
    }
    if let Some(mode) = options.get("compilation_mode") {
        if !["fastbuild", "dbg", "opt"].contains(&mode) {
            return Err(format!("Not a valid compilation mode: '{mode}' (should be dbg, fastbuild or opt)"));
        }
        build.compilation_mode = mode.to_string();
    }
    if let Some(cpu) = options.get("cpu") {
        build.cpu = cpu.to_string();
    }
    for define in options.all("define") {
        let (name, value) = define.split_once('=').ok_or_else(|| format!("Variable definitions must be in the form of a 'name=value' assignment: '{define}'"))?;
        build.defines.retain(|(existing, _)| existing != name);
        build.defines.push((name.to_string(), value.to_string()));
    }
    build.defines.sort();
    build.action_env = action_env(context, options);
    for flag in options.all(bazel_options::STARLARK_FLAG) {
        let (label, value) = flag.split_once('=').unwrap_or((flag, "true"));
        let label = parse_label(label)?;
        build.starlark_flags.retain(|(existing, _)| *existing != label);
        build.starlark_flags.push((label, value.to_string()));
    }
    build.starlark_flags.sort();
    for name in ["copt", "cxxopt", "conlyopt", "linkopt", "features"] {
        let values = options.all(name);
        if !values.is_empty() {
            build.values.push((name.to_string(), values.join(" ")));
        }
    }
    if options.flag("stamp", false) {
        build.values.push(("stamp".to_string(), "true".to_string()));
    }
    build.values.sort();
    Ok(ConfigId::intern(&build))
}

/// The environment of actions that use the default shell environment:
/// `PATH` from the client, as Bazel passes it, and `--action_env`s.
fn action_env(context: &CommandContext<'_>, options: &ParsedOptions) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(path) = context.env.get("PATH") {
        env.push(("PATH".to_string(), path.clone()));
    }
    for entry in options.all("action_env") {
        let (name, value) = match entry.split_once('=') {
            Some((name, value)) => (name.to_string(), value.to_string()),
            None => (entry.to_string(), context.env.get(entry).cloned().unwrap_or_default()),
        };
        env.retain(|(existing, _)| *existing != name);
        env.push((name, value));
    }
    env.sort();
    env
}

/// Analyze targets, reporting the errors.
async fn analyze(
    session: &Session,
    labels: &[Label],
    config: ConfigId,
    mapping: &RepoMappingTable,
    out: &Output,
) -> (Vec<(Label, Arc<ConfiguredTarget>)>, bool) {
    let keys: Vec<ConfiguredTargetKey> = labels.iter().map(|label| ConfiguredTargetKey { label: *label, config }).collect();
    let results = session.evaluate_many(&keys).await;
    let mut analyzed = Vec::new();
    let mut errors = false;
    for (label, result) in labels.iter().zip(results) {
        match result {
            Ok(target) => analyzed.push((*label, target)),
            Err(error) => {
                errors = true;
                let shown = bazel_query::display_label(*label, mapping);
                out.error(&format!("{shown}: {}", error.message())).await;
            }
        }
    }
    (analyzed, errors)
}

/// The files building a target builds: its default outputs, and if it's
/// executable, its runfiles.
pub fn top_level_artifacts(target: &ConfiguredTarget) -> Vec<Artifact> {
    let Some(value) = target.target_value() else {
        return Vec::new();
    };
    let mut artifacts = depset_files(value.default_info("files"));
    let runfiles = value.default_info("default_runfiles").and_then(Runfiles::from_value);
    if let Some(runfiles) = runfiles.filter(|_| has_executable(value)) {
        artifacts.extend(runfiles.entries("_main").into_iter().map(|(_, artifact)| artifact));
    }
    let mut seen = BTreeSet::new();
    artifacts.retain(|artifact| seen.insert(*artifact));
    artifacts
}

fn has_executable(value: &TargetValue<'_>) -> bool {
    value.default_info("executable").is_some()
}

fn depset_files(value: Option<starlark::values::Value<'_>>) -> Vec<Artifact> {
    let Some(depset) = value.and_then(Depset::from_value) else {
        return Vec::new();
    };
    depset.to_list().into_iter().filter_map(FileValue::artifact).collect()
}

/// Run the actions that generate artifacts, returning their outputs and
/// whether any failed.
pub async fn execute(session: &Session, artifacts: &[Artifact], out: &Output) -> (Vec<OutputFile>, bool) {
    let mut keys = Vec::new();
    for artifact in artifacts {
        let Some(key) = generating_action(session, *artifact).await else {
            continue;
        };
        keys.push((key, *artifact));
    }
    let unique: Vec<ActionKey> = keys.iter().map(|(key, _)| *key).collect::<BTreeSet<_>>().into_iter().collect();
    let results = session.evaluate_many(&unique).await;
    let mut files = Vec::new();
    let mut errors = false;
    for (key, result) in unique.iter().zip(results) {
        match result {
            Ok(outputs) => {
                let wanted = keys.iter().filter(|(candidate, _)| candidate == key).map(|(_, artifact)| artifact.exec_path);
                let wanted: BTreeSet<_> = wanted.collect();
                files.extend(outputs.files.iter().filter(|file| wanted.contains(&file.path)).copied());
            }
            Err(error) => {
                errors = true;
                out.error(error.message()).await;
            }
        }
    }
    (files, errors)
}

async fn generating_action(session: &Session, artifact: Artifact) -> Option<ActionKey> {
    let config = artifact.config?;
    let owner = ConfiguredTargetKey {
        label: artifact.owner,
        config,
    };
    let target = session.evaluate(&owner).await.ok()?;
    let (index, _) = target.generating_action(artifact.exec_path)?;
    Some(ActionKey { owner, index })
}

/// Print what each target built, and make the convenience symlinks.
pub async fn report(context: &CommandContext<'_>, options: &ParsedOptions, built: &Built, out: &Output) {
    let prefix = options.get("symlink_prefix").unwrap_or("bazel-");
    let config = built.config.get();
    if let Err(error) = convenience_symlinks(&context.workspace_root, &built.exec_root, prefix, &config.output_directory) {
        out.warning(&format!("failed to create convenience symlinks: {error}")).await;
    }
    let show_result: usize = options.get("show_result").and_then(|value| value.parse().ok()).unwrap_or(1);
    if built.targets.len() > show_result {
        return;
    }
    for (label, target, files) in &built.targets {
        if target.incompatible.is_some() {
            continue;
        }
        let shown = bazel_query::display_label(*label, &built.main_mapping);
        let defaults: Vec<Artifact> = default_files(target).into_iter().filter(|artifact| !artifact.is_source()).collect();
        if defaults.is_empty() {
            out.stderr(&format!("Target {shown} up-to-date (nothing to build)\n")).await;
            continue;
        }
        out.stderr(&format!("Target {shown} up-to-date:\n")).await;
        for artifact in defaults {
            let built = files.iter().any(|file| file.path == artifact.exec_path);
            if built {
                out.stderr(&format!("  {}\n", display_path(&artifact, prefix, &config.output_directory))).await;
            }
        }
    }
}

fn default_files(target: &ConfiguredTarget) -> Vec<Artifact> {
    match target.target_value() {
        Some(value) => depset_files(value.default_info("files")),
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
