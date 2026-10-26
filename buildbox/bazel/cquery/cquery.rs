//! `cquery` and `aquery`: Bazel's query language over the configured
//! graph, which analysis builds from the universe's top-level targets.
//!
//! The language and its functions are `bazel_query`'s; this crate gives
//! them the configured graph as their environment, with `cquery`'s
//! `config()` or `aquery`'s action filters, and prints results in
//! `cquery`'s formats, or as `aquery` prints the actions of the targets.

mod action_output;
mod action_proto;
mod configured_graph;
mod node;
mod output;
mod starlark_output;

pub use action_output::{ActionFilters, AqueryFormat, AqueryOutput};
pub use configured_graph::{ConfiguredGraph, ConfiguredOptions, Language};
pub use node::{ConfiguredNode, config_name};
pub use output::{CqueryFormat, CqueryOutput, Printed, configuration_message, format};
pub use starlark_output::Formatter;

use bazel_query::{Expr, QueryError, evaluate};
use skyframe::Compute;

/// Evaluate a parsed `cquery` expression over the configured graph, and
/// print its result.
pub async fn run<C: Compute>(graph: &ConfiguredGraph<C>, expr: &Expr, output: &CqueryOutput) -> Result<Printed, QueryError> {
    let result = evaluate(graph, expr).await?;
    format(graph, &result, expr, output).await
}

/// Evaluate a parsed `aquery` expression over the configured graph, and
/// print the actions of its result's targets that its filters match.
pub async fn run_aquery<C: Compute>(graph: &ConfiguredGraph<C>, expr: &Expr, output: &AqueryOutput) -> Result<Vec<u8>, QueryError> {
    let filters = ActionFilters::of(expr)?;
    let result = evaluate(graph, expr).await?;
    action_output::format(graph, &result, &filters, output).await
}
