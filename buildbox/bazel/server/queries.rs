//! The query commands: `query` over the package graph, and `cquery` and
//! `aquery` over the configured graph.

use crate::build::{self, Built, Purpose};
use crate::command::{CommandContext, exit, working_package};
use crate::output::Output;
use bazel_cquery::{AqueryFormat, AqueryOutput, ConfiguredGraph, ConfiguredNode, ConfiguredOptions, CqueryFormat, CqueryOutput, Formatter, Language, Printed};
use bazel_analysis::ConfiguredTargetKey;
use bazel_options::{OptionKind, OptionSpec, ParsedOptions};
use bazel_query::{DotStyle, Expr, OrderOutput, OutputFormat, OutputOptions, QueryOptions, QueryRequest, Universe};
use skyframe::Session;
use bazel_ui::Phase;
use std::time::Instant;

/// The options every query command takes.
const COMMON_OPTIONS: &[(&str, OptionKind)] = &[
    ("output", OptionKind::Value),
    ("keep_going", OptionKind::Bool),
    ("implicit_deps", OptionKind::Bool),
    ("tool_deps", OptionKind::Bool),
    ("host_deps", OptionKind::Bool),
    ("universe_scope", OptionKind::Value),
    ("query_file", OptionKind::Value),
    ("line_terminator_null", OptionKind::Bool),
    ("output_file", OptionKind::Value),
    ("loading_phase_threads", OptionKind::Value),
    ("experimental_repository_cache_hardlinks", OptionKind::Bool),
    ("repo_contents_cache", OptionKind::Value),
    ("repository_cache", OptionKind::Value),
    ("enable_platform_specific_config", OptionKind::Bool),
    ("registry", OptionKind::Multi),
    ("module_mirrors", OptionKind::Multi),
    ("check_direct_dependencies", OptionKind::Value),
    ("check_bazel_compatibility", OptionKind::Value),
    ("allow_yanked_versions", OptionKind::Multi),
    ("repo_env", OptionKind::Multi),
    ("color", OptionKind::Value),
    ("curses", OptionKind::Value),
    ("ui_actions_shown", OptionKind::Int),
    ("show_progress", OptionKind::Bool),
];

pub(crate) fn query_spec() -> OptionSpec {
    use OptionKind::*;
    let mut options = COMMON_OPTIONS.to_vec();
    options.extend_from_slice(&[
        ("null", Bool),
        ("order_output", Value),
        ("order_results", Bool),
        ("relative_locations", Bool),
        ("xml:default_values", Bool),
        ("graph:factored", Bool),
        ("graph:node_limit", Int),
        ("graph:conditional_edges_limit", Int),
    ]);
    // `query` loads packages without configuring them, so it takes the
    // configuration options Bazel's does, which `.bazelrc` files often give
    // in `common` lines, and ignores them.
    let takes = |name: &str| bazel_options::bazel_option(name).is_some_and(|option| option.takes("query"));
    let native = bazel_config::command_line_options().filter(|option| takes(option.name));
    options.extend(native.map(|option| (option.name, build::option_kind(option))));
    options.extend(build::NO_OP_OPTIONS.iter().filter(|(name, _)| takes(name)));
    OptionSpec::new(&options)
}

pub async fn query(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    let options = match parse_options(context, "query", args, &query_spec(), out).await {
        Ok(options) => options,
        Err(code) => return code,
    };
    let expression = match query_expression(&options) {
        Ok(expression) => expression,
        Err(message) => {
            out.error(&message).await;
            return exit::COMMAND_LINE_ERROR;
        }
    };
    let output = match output_options(&options) {
        Ok(output) => output,
        Err(message) => {
            out.error(&message).await;
            return exit::COMMAND_LINE_ERROR;
        }
    };
    let universe_scope = universe_scope(&options);
    let functions = bazel_query::query_functions(universe_scope.is_some());
    if let Err(error) = bazel_query::parse(&expression, &functions) {
        out.error(&format!("Error while parsing '{}': {}", bazel_query::truncate(&expression), error.0)).await;
        return exit::COMMAND_LINE_ERROR;
    }
    let workspace = context.server.workspaces().get(&context.workspace_root);
    out.track(workspace.progress.track()).await;
    crate::command::refresh(context, &workspace, &options).await;
    let session = workspace.graph.session().await;
    out.phase(Phase::RepoMapping).await;
    let main_mapping = match crate::command::main_mapping(context, &session, &options, out).await {
        Ok(mapping) => mapping,
        Err(code) => return code,
    };
    let request = QueryRequest {
        expression,
        output,
        options: query_options(&options),
        universe_scope,
        working_package: working_package(&context.workspace_root, &context.working_directory),
        main_mapping,
        workspace_root: context.workspace_root.clone(),
    };
    out.phase(Phase::Loading).await;
    let result = bazel_query::run(session, request).await;
    out.phase(Phase::Idle).await;
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            out.error(&error.describe()).await;
            return exit::ANALYSIS_FAILURE;
        }
    };
    for error in result.errors.iter().chain(&result.failure) {
        out.error(error).await;
    }
    let Some(printed) = result.output else {
        return exit::ANALYSIS_FAILURE;
    };
    out.stdout_bytes(&printed).await;
    let partial = result.kept_going || result.failure.is_some();
    if partial {
        out.warning("--keep_going specified, ignoring errors. Results may be inaccurate").await;
    }
    if result.targets == 0 {
        out.info("Empty results").await;
    }
    match partial {
        true => exit::PARTIAL_ANALYSIS_FAILURE,
        false => exit::SUCCESS,
    }
}

/// Parse a query command's options, warning of deprecated ones as Bazel
/// does.
pub(crate) async fn parse_options(context: &CommandContext<'_>, command: &str, args: &[String], spec: &OptionSpec, out: &Output) -> Result<ParsedOptions, i32> {
    let all_args = bazel_options::command_args(&context.rc, command, args);
    let options = match bazel_options::parse(&all_args, spec, &context.rc, command) {
        Ok(options) => options,
        Err(error) => {
            out.error(&error.to_string()).await;
            return Err(exit::COMMAND_LINE_ERROR);
        }
    };
    out.configure(&options).await;
    if options.get("host_deps").is_some() {
        out.warning("Option 'host_deps' is deprecated: Use --tool_deps instead").await;
    }
    Ok(options)
}

/// Which dependencies the options say queries follow.
pub(crate) fn query_options(options: &ParsedOptions) -> QueryOptions {
    QueryOptions {
        implicit_deps: options.flag("implicit_deps", true),
        tool_deps: options.flag("tool_deps", true) && options.flag("host_deps", true),
        keep_going: options.flag("keep_going", false),
    }
}

/// The patterns `--universe_scope` gives, if any.
pub(crate) fn universe_scope(options: &ParsedOptions) -> Option<Vec<String>> {
    let scope = options.get("universe_scope")?;
    Some(scope.split(',').filter(|pattern| !pattern.is_empty()).map(str::to_string).collect())
}

/// How `query` prints results, as the options say.
fn output_options(options: &ParsedOptions) -> Result<OutputOptions, String> {
    let name = options.get("output").unwrap_or("label");
    let Some(format) = OutputFormat::parse(name) else {
        return Err(format!("Invalid output format '{name}'. Valid values are: {}", OutputFormat::NAMES.join(", ")));
    };
    let order = order_output(options)?;
    let limit = |name: &str, default: i64| match options.int(name, default) {
        limit if limit < 0 => None,
        limit => Some(limit as usize),
    };
    Ok(OutputOptions {
        format,
        order,
        graph: DotStyle {
            factored: options.flag("graph:factored", true),
            node_limit: limit("graph:node_limit", 512),
            conditional_edges_limit: limit("graph:conditional_edges_limit", 4),
        },
        relative_locations: options.flag("relative_locations", false),
        xml_default_values: options.flag("xml:default_values", false),
        null_terminated: null_terminated(options),
    })
}

/// The order `--order_output` gives, or `--noorder_results`, which asks
/// for none.
pub(crate) fn order_output(options: &ParsedOptions) -> Result<OrderOutput, String> {
    if !options.flag("order_results", true) {
        return Ok(OrderOutput::No);
    }
    let name = options.get("order_output").unwrap_or("auto");
    OrderOutput::parse(name).ok_or_else(|| format!("While parsing option --order_output={name}: Not a valid Order output setting: '{name}' (should be no, deps, auto or full)"))
}

/// Whether the options ask for lines ended by NULs.
pub(crate) fn null_terminated(options: &ParsedOptions) -> bool {
    options.flag("null", false) || options.flag("line_terminator_null", false)
}

/// The query expression: the command line's, or `--query_file`'s.
pub(crate) fn query_expression(options: &ParsedOptions) -> Result<String, String> {
    if let Some(file) = options.get("query_file") {
        if !options.residue.is_empty() {
            return Err("Command-line query and --query_file cannot both be specified".into());
        }
        return std::fs::read_to_string(file).map_err(|error| format!("failed to read --query_file {file}: {error}"));
    }
    if options.residue.is_empty() {
        return Err("missing query expression. Type 'bb help query' for syntax and help.".into());
    }
    Ok(options.residue.join(" "))
}

/// The options `cquery` takes: those of a build, and its own.
pub(crate) fn cquery_spec() -> OptionSpec {
    use OptionKind::*;
    let mut options = build::build_spec().options();
    options.extend_from_slice(COMMON_OPTIONS);
    options.extend_from_slice(&[
        ("starlark:expr", Value),
        ("starlark:file", Value),
        ("include_aspects", Bool),
        ("transitions", Value),
        ("graph:factored", Bool),
        ("graph:node_limit", Int),
    ]);
    OptionSpec::new(&options)
}

pub async fn cquery(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    configured_query(context, args, Language::Cquery, out).await
}

pub async fn aquery(context: &CommandContext<'_>, args: &[String], out: &Output) -> i32 {
    configured_query(context, args, Language::Aquery, out).await
}

/// How a query of the configured graph prints its result.
enum ConfiguredOutput {
    Cquery(CqueryOutput),
    Aquery(AqueryOutput),
}

/// Run `cquery` or `aquery`: analyze the universe as a build that doesn't
/// execute anything would, then query what it configured.
async fn configured_query(context: &CommandContext<'_>, args: &[String], language: Language, out: &Output) -> i32 {
    let started = Instant::now();
    let (command, spec) = match language {
        Language::Cquery => ("cquery", cquery_spec()),
        Language::Aquery => ("aquery", aquery_spec()),
    };
    let options = match parse_options(context, command, args, &spec, out).await {
        Ok(options) => options,
        Err(code) => return code,
    };
    let output = match language {
        Language::Cquery => cquery_output(&options).map(ConfiguredOutput::Cquery),
        Language::Aquery => aquery_output(&options, crate::commands::output_base(context).join("execroot/_main")).map(ConfiguredOutput::Aquery),
    };
    let (expression, output) = match query_expression(&options).and_then(|expression| Ok((expression, output?))) {
        Ok(found) => found,
        Err(message) => {
            out.error(&message).await;
            return exit::COMMAND_LINE_ERROR;
        }
    };
    let expr = match bazel_query::parse(&expression, &language.functions()) {
        Ok(expr) => expr,
        Err(error) => {
            out.error(&format!("Error while parsing '{}': {}", bazel_query::truncate(&expression), error.0)).await;
            return exit::COMMAND_LINE_ERROR;
        }
    };
    let workspace = context.server.workspaces().get(&context.workspace_root);
    crate::command::refresh(context, &workspace, &options).await;
    let session = workspace.graph.session().await;
    let purpose = match language {
        Language::Cquery => Purpose::Query,
        Language::Aquery => Purpose::Build,
    };
    let built = match analyze_universe(context, &session, &options, &expr, purpose, out).await {
        Ok(built) => built,
        Err(code) => return build::failed(code, started, out).await,
    };
    let graph = configured_graph(context, &session, &options, language, &built);
    let printed = match &output {
        ConfiguredOutput::Cquery(output) => bazel_cquery::run(&graph, &expr, output).await,
        ConfiguredOutput::Aquery(output) => bazel_cquery::run_aquery(&graph, &expr, output).await.map(|output| Printed { output, errors: Vec::new() }),
    };
    match printed {
        Ok(printed) => {
            for error in &printed.errors {
                out.error(error).await;
            }
            out.stdout_bytes(&printed.output).await;
        }
        Err(error) => {
            out.error(&format!("Error doing post analysis query: {}", error.describe())).await;
            return build::failed(exit::BUILD_FAILURE, started, out).await;
        }
    }
    build::report_stats(started, out).await;
    build::completed(0, out).await;
    exit::SUCCESS
}

/// Analyze a configured query's universe: the targets `--universe_scope`
/// names, or else those the expression's patterns do, as a build that
/// doesn't execute anything would. `aquery`'s patterns name what a build's
/// would, `cquery`'s every target they match.
async fn analyze_universe(context: &CommandContext<'_>, session: &Session, options: &ParsedOptions, expr: &Expr, purpose: Purpose, out: &Output) -> Result<Built, i32> {
    let mut universe = options.clone();
    universe.residue = match universe_scope(options) {
        Some(scope) => scope,
        None => {
            let mut patterns = Vec::new();
            expr.target_patterns(&mut patterns);
            let mut unique = Vec::with_capacity(patterns.len());
            for pattern in patterns {
                if !unique.contains(&pattern) {
                    unique.push(pattern);
                }
            }
            unique
        }
    };
    universe.set("build", "false");
    let built = build::run_command(context, session, &universe, purpose, out).await?;
    out.info(&built.found).await;
    Ok(built)
}

/// The configured graph of what analyzing the universe configured.
fn configured_graph<'a>(context: &CommandContext<'_>, session: &'a Session, options: &ParsedOptions, language: Language, built: &Built) -> ConfiguredGraph<&'a Session> {
    let packages = Universe::new(
        session,
        query_options(options),
        working_package(&context.workspace_root, &context.working_directory),
        built.main_mapping.clone(),
        context.workspace_root.clone(),
    );
    let configured = ConfiguredOptions {
        implicit_deps: options.flag("implicit_deps", true),
        tool_deps: options.flag("tool_deps", true) && options.flag("host_deps", true),
        include_aspects: options.flag("include_aspects", true),
    };
    let roots = built.targets.iter().map(|(_, target, _)| ConfiguredNode { label: target.label, config: Some(target.config) }).collect();
    let aspects = built.aspects.iter().map(|aspect| ConfiguredTargetKey { label: aspect.target.label, config: aspect.target.config }).collect();
    ConfiguredGraph::new(packages, configured, language, built.config, roots, aspects)
}

/// The options `aquery` takes: those of a build, and its own.
pub(crate) fn aquery_spec() -> OptionSpec {
    use OptionKind::*;
    let mut options = build::build_spec().options();
    options.extend_from_slice(COMMON_OPTIONS);
    options.extend_from_slice(&[
        ("include_commandline", Bool),
        ("include_artifacts", Bool),
        ("include_file_write_contents", Bool),
        ("include_aspects", Bool),
    ]);
    OptionSpec::new(&options)
}

/// How `aquery` prints results, as the options say.
fn aquery_output(options: &ParsedOptions, exec_root: std::path::PathBuf) -> Result<AqueryOutput, String> {
    let name = options.get("output").unwrap_or("text");
    let Some(format) = AqueryFormat::parse(name) else {
        return Err(format!("Invalid output format '{name}'. Valid values are: {}", AqueryFormat::NAMES.join(", ")));
    };
    Ok(AqueryOutput {
        format,
        include_commandline: options.flag("include_commandline", true),
        include_artifacts: options.flag("include_artifacts", true),
        include_file_write_contents: options.flag("include_file_write_contents", false),
        exec_root,
    })
}

/// How `cquery` prints results, as the options say.
fn cquery_output(options: &ParsedOptions) -> Result<CqueryOutput, String> {
    let name = options.get("output").unwrap_or("label");
    if name == "transitions" {
        return Err("--output=transitions isn't supported by bb yet".to_string());
    }
    let Some(format) = CqueryFormat::parse(name) else {
        return Err(format!("Invalid output format '{name}'. Valid values are: {}", CqueryFormat::NAMES.join(", ")));
    };
    let starlark = match (options.get("starlark:file"), options.get("starlark:expr")) {
        (Some(_), Some(expr)) if !expr.is_empty() => return Err("You must not specify both --starlark:expr and --starlark:file".to_string()),
        (Some(file), _) if !file.is_empty() => {
            let source = std::fs::read_to_string(file).map_err(|error| format!("invalid --starlark:file: failed to read {file} ({error})"))?;
            Formatter::File(source)
        }
        (_, expr) => Formatter::Expr(expr.unwrap_or_default().to_string()),
    };
    let limit = |name: &str, default: i64| match options.int(name, default) {
        limit if limit < 0 => None,
        limit => Some(limit as usize),
    };
    Ok(CqueryOutput {
        format,
        graph: DotStyle {
            factored: options.flag("graph:factored", true),
            node_limit: limit("graph:node_limit", 512),
            conditional_edges_limit: limit("graph:conditional_edges_limit", 4),
        },
        output_groups: crate::top_level::output_groups(options),
        starlark,
        null_terminated: null_terminated(options),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every option bb takes is one Bazel takes for the same command, so
    /// that the checks of what Bazel takes come first.
    #[test]
    fn commands_take_only_bazels_options() {
        let commands = [
            ("build", build::build_spec()),
            ("test", build::build_spec()),
            ("run", build::command_spec("run")),
            ("info", build::build_spec()),
            ("query", query_spec()),
            ("cquery", cquery_spec()),
            ("aquery", aquery_spec()),
        ];
        let mut missing = Vec::new();
        for (command, spec) in commands {
            for (name, _) in spec.options() {
                if !bazel_options::bazel_option(name).is_some_and(|option| option.takes(command)) {
                    missing.push(format!("{command} --{name}"));
                }
            }
        }
        assert_eq!(missing, Vec::<String>::new());
    }
}
