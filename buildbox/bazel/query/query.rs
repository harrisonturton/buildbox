//! Bazel's query language over loaded packages.

mod eval;
mod output;
mod parse;
mod universe;

pub use eval::{deps, evaluate};
pub use output::{OutputFormat, display_label, display_package, format};
pub use parse::{Arg, Expr, ParseError, SetOp, parse};
pub use universe::{QueryError, QueryOptions, TargetSet, Universe};

use bazel_label::{PackageId, RepoMappingTable};
use skyframe::Compute;
use std::path::PathBuf;
use std::sync::Arc;

/// A query to run.
pub struct QueryRequest {
    pub expression: String,
    pub output: OutputFormat,
    pub options: QueryOptions,
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
    pub output: Option<String>,
    pub targets: usize,
    /// The errors to report, in order, but for the last.
    pub errors: Vec<String>,
    /// The error that failed the query, or, keeping going, its evaluation.
    pub failure: Option<String>,
    /// Whether keeping going went past errors.
    pub kept_going: bool,
}

/// Parse, evaluate and print a query.
pub async fn run<C: Compute>(compute: C, request: QueryRequest) -> Result<QueryResult, QueryError> {
    let expr = parse(&request.expression).map_err(|error| QueryError(error.0))?;
    let keep_going = request.options.keep_going;
    let universe = Universe::new(compute, request.options, request.working_package, request.main_mapping, request.workspace_root);
    let printed = match evaluate(&universe, &expr).await {
        Ok(targets) => format(&universe, &targets, request.output).await.map(|output| (output, targets.len())),
        Err(error) => Err(error),
    };
    let errors = universe.errors().await;
    let evaluation_failure = universe.evaluation_failure().map(|detail| match detail {
        Some(detail) => format!("Evaluation of query \"{}\" failed: {detail}", request.expression),
        None => format!("Evaluation of query \"{}\" failed", request.expression),
    });
    let (output, targets, failure) = match printed {
        Ok(_) if evaluation_failure.is_some() && !keep_going => (None, 0, evaluation_failure),
        Ok((output, targets)) => (Some(output), targets, evaluation_failure),
        Err(error) => (None, 0, Some(evaluation_failure.unwrap_or(error.0))),
    };
    Ok(QueryResult {
        output,
        targets,
        errors,
        failure,
        kept_going: universe.kept_going(),
    })
}
