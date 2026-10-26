//! Bazel's query language: parsing it, evaluating it over a graph of
//! targets, and printing results.
//!
//! The language and its functions are shared by every kind of query: an
//! environment ([`QueryEnvironment`]) says what a query's nodes are and how
//! they depend on each other. `query` reads the package graph, which
//! [`Universe`] loads; `cquery` and `aquery` read the configured graph.

mod build_output;
mod engine;
mod functions;
mod graph;
mod lexer;
mod nodes;
mod output;
mod parse;
pub mod proto;
mod target_graph;
mod target_proto;
mod universe;
mod xml_output;

pub use build_output::{AttrValues, build_value, write_rule_with};
pub use engine::{Context, QueryEnvironment, QueryError, Vars, evaluate};
pub use functions::{TagFilter, breadth_first, closure_edges, expr_arg, is_test_rule, is_test_suite, kind_of, regex_arg, word_arg};
pub use graph::{Digraph, DotStyle};
pub use nodes::NodeSet;
pub use output::{OrderOutput, OutputFormat, OutputOptions, RepoPaths, display_label, display_package, format};
pub use parse::{AQUERY_FUNCTIONS, Arg, ArgKind, CQUERY_FUNCTIONS, DEFAULT_FUNCTIONS, Expr, Function, ParseError, SetOp, UNIVERSE_FUNCTIONS, parse, truncate};
pub use target_graph::{attr_string, is_manual, possible_values};
pub use target_proto::target_message;
pub use universe::{QueryOptions, TargetRef, TargetSet, Universe, attr_value};

use bazel_label::{PackageId, RepoMappingTable};
use skyframe::Compute;
use std::path::PathBuf;
use std::sync::Arc;

/// A query to run.
pub struct QueryRequest {
    pub expression: String,
    pub output: OutputOptions,
    pub options: QueryOptions,
    /// The patterns `--universe_scope` gives, if any.
    pub universe_scope: Option<Vec<String>>,
    /// The package relative patterns are relative to: the working
    /// directory's.
    pub working_package: PackageId,
    /// How the main repository sees others, for printing labels.
    pub main_mapping: Arc<RepoMappingTable>,
    pub workspace_root: PathBuf,
}

/// What a query printed, and the errors it met.
pub struct QueryResult {
    /// What the query printed, unless it failed.
    pub output: Option<Vec<u8>>,
    pub targets: usize,
    /// The errors to report, in order, but for the last.
    pub errors: Vec<String>,
    /// The error that failed the query, or, keeping going, its evaluation.
    pub failure: Option<String>,
    /// Whether keeping going went past errors.
    pub kept_going: bool,
}

/// The functions `query` can call, with `--universe_scope` or without.
pub fn query_functions(universe_scope: bool) -> Vec<Function> {
    let mut functions = DEFAULT_FUNCTIONS.to_vec();
    if universe_scope {
        functions.extend_from_slice(UNIVERSE_FUNCTIONS);
    }
    functions
}

/// Parse, evaluate and print a query.
pub async fn run<C: Compute>(compute: C, request: QueryRequest) -> Result<QueryResult, QueryError> {
    let functions = query_functions(request.universe_scope.is_some());
    let expr = parse(&request.expression, &functions).map_err(|error| QueryError::new(error.0))?;
    let keep_going = request.options.keep_going;
    let universe = Universe::new(compute, request.options, request.working_package, request.main_mapping, request.workspace_root).with_scope(request.universe_scope);
    let printed = match evaluate(&universe, &expr).await {
        Ok(targets) => format(&universe, &targets, &expr, &request.output).await.map(|output| (output, targets.len())),
        Err(error) => Err(error),
    };
    let errors = universe.errors().await;
    let shown = truncate(&expr.to_string());
    let evaluation_failure = universe.evaluation_failure().map(|detail| match detail {
        Some(detail) => format!("Evaluation of query \"{shown}\" failed: {detail}"),
        None => format!("Evaluation of query \"{shown}\" failed"),
    });
    let (output, targets, failure) = match printed {
        Ok(_) if evaluation_failure.is_some() && !keep_going => (None, 0, evaluation_failure),
        Ok((output, targets)) => (Some(output), targets, evaluation_failure),
        Err(error) => (None, 0, Some(evaluation_failure.unwrap_or_else(|| error.describe()))),
    };
    let failure = failure.map(|failure| bazel_loading::split_reported(&failure).1.to_string());
    Ok(QueryResult {
        output,
        targets,
        errors,
        failure,
        kept_going: universe.kept_going(),
    })
}
