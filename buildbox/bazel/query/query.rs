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

/// What a query printed, and the errors it kept going past.
pub struct QueryResult {
    pub output: String,
    pub errors: Vec<String>,
    pub targets: usize,
}

/// Parse, evaluate and print a query.
pub async fn run<C: Compute>(compute: C, request: QueryRequest) -> Result<QueryResult, QueryError> {
    let expr = parse(&request.expression).map_err(|error| QueryError(error.0))?;
    let universe = Universe::new(compute, request.options, request.working_package, request.main_mapping, request.workspace_root);
    let targets = evaluate(&universe, &expr).await?;
    let output = format(&universe, &targets, request.output).await?;
    Ok(QueryResult {
        output,
        errors: universe.errors(),
        targets: targets.len(),
    })
}
