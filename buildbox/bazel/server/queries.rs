//! The query commands: `query` over the package graph, and `cquery` and
//! `aquery` over the configured graph.

use crate::command::{CommandContext, exit, working_package};
use crate::output::Output;
use bazel_label::RepoName;
use bazel_loading::RepoMappingKey;
use bazel_options::{OptionKind, OptionSpec, ParsedOptions};
use bazel_query::{DotStyle, OrderOutput, OutputFormat, OutputOptions, QueryOptions, QueryRequest};
use bazel_ui::Phase;
use std::sync::Arc;

/// The options every query command takes.
const COMMON_OPTIONS: &[(&str, OptionKind)] = &[
    ("output", OptionKind::Value),
    ("keep_going", OptionKind::Bool),
    ("implicit_deps", OptionKind::Bool),
    ("tool_deps", OptionKind::Bool),
    ("host_deps", OptionKind::Bool),
    ("universe_scope", OptionKind::Value),
    ("infer_universe_scope", OptionKind::Bool),
    ("query_file", OptionKind::Value),
    ("line_terminator_null", OptionKind::Bool),
    ("null", OptionKind::Bool),
    ("output_file", OptionKind::Value),
    ("loading_phase_threads", OptionKind::Value),
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
        ("order_output", Value),
        ("order_results", Bool),
        ("relative_locations", Bool),
        ("proto:default_values", Bool),
        ("xml:default_values", Bool),
        ("xml:line_numbers", Bool),
        ("graph:factored", Bool),
        ("graph:node_limit", Int),
        ("graph:conditional_edges_limit", Int),
        ("strict_test_suite", Bool),
        ("experimental_graphless_query", Value),
    ]);
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
    workspace.refresh(crate::command::repo_environment(context, &options)).await;
    let session = workspace.graph.session().await;
    out.phase(Phase::RepoMapping).await;
    let main_mapping = match session.evaluate(&RepoMappingKey(RepoName::MAIN)).await {
        Ok(mapping) => Arc::new(mapping.0.clone()),
        Err(error) => {
            out.error(error.message()).await;
            return exit::ANALYSIS_FAILURE;
        }
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
