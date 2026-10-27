//! Printing `cquery` results in Bazel's output formats.

use crate::configured_graph::ConfiguredGraph;
use crate::node::ConfiguredNode;
use crate::starlark_output::Formatter;
use bazel_analysis::Artifact;
use bazel_config::ConfigId;
use bazel_packages::Target;
use bazel_query::proto::{self, Message};
use bazel_query::{AttrValues, Digraph, DotStyle, Expr, NodeSet, QueryEnvironment, QueryError, RepoPaths, kind_of, target_message, write_rule_with};
use skyframe::Compute;
use std::fmt::Write;

/// How `cquery` prints results, as `--output` chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CqueryFormat {
    Label,
    LabelKind,
    Build,
    Graph,
    Files,
    Proto,
    StreamedProto,
    TextProto,
    JsonProto,
    Starlark,
}

impl CqueryFormat {
    /// The formats `cquery --output` takes, as Bazel lists them.
    pub const NAMES: &'static [&'static str] = &[
        "label_kind",
        "label",
        "transitions",
        "proto",
        "streamed_proto",
        "textproto",
        "jsonproto",
        "build",
        "graph",
        "starlark",
        "files",
    ];

    pub fn parse(name: &str) -> Option<CqueryFormat> {
        Some(match name {
            "label" => CqueryFormat::Label,
            "label_kind" => CqueryFormat::LabelKind,
            "build" => CqueryFormat::Build,
            "graph" => CqueryFormat::Graph,
            "files" => CqueryFormat::Files,
            "proto" => CqueryFormat::Proto,
            "streamed_proto" => CqueryFormat::StreamedProto,
            "textproto" => CqueryFormat::TextProto,
            "jsonproto" => CqueryFormat::JsonProto,
            "starlark" => CqueryFormat::Starlark,
            _ => return None,
        })
    }
}

/// How to print a `cquery` result.
pub struct CqueryOutput {
    pub format: CqueryFormat,
    pub graph: DotStyle,
    /// The output groups `--output=files` prints the files of.
    pub output_groups: Vec<String>,
    /// What `--output=starlark` formats each target with.
    pub starlark: Formatter,
    /// Whether lines end with NULs rather than newlines.
    pub null_terminated: bool,
}

/// What printing a result printed, and the errors to report.
pub struct Printed {
    pub output: Vec<u8>,
    pub errors: Vec<String>,
}

/// Print the result of `expr` as `output` says.
pub async fn format<C: Compute>(graph: &ConfiguredGraph<C>, result: &NodeSet<ConfiguredNode>, expr: &Expr, output: &CqueryOutput) -> Result<Printed, QueryError> {
    let nodes = ordered(result, expr);
    let mut errors = Vec::new();
    let text = match output.format {
        CqueryFormat::Proto | CqueryFormat::StreamedProto | CqueryFormat::TextProto | CqueryFormat::JsonProto => {
            let output = proto_output(graph, &nodes, output.format).await?;
            return Ok(Printed { output, errors });
        }
        CqueryFormat::Label => labels(graph, &nodes),
        CqueryFormat::LabelKind => label_kinds(graph, &nodes).await?,
        CqueryFormat::Build => build(graph, &nodes).await?,
        CqueryFormat::Graph => dot(graph, result, &output.graph).await?,
        CqueryFormat::Files => files(graph, &nodes, &output.output_groups).await,
        CqueryFormat::Starlark => {
            let (text, failed) = crate::starlark_output::format(graph, &nodes, &output.starlark).await?;
            errors = failed;
            text
        }
    };
    let output = match output.null_terminated {
        true => text.replace('\n', "\0").into_bytes(),
        false => text.into_bytes(),
    };
    Ok(Printed { output, errors })
}

/// The result in the order it's printed: a path's, for `somepath`, else
/// by label and configuration.
fn ordered(result: &NodeSet<ConfiguredNode>, expr: &Expr) -> Vec<ConfiguredNode> {
    let mut nodes: Vec<ConfiguredNode> = result.iter().copied().collect();
    if !expr.is_top_level_somepath() {
        nodes.sort();
    }
    nodes
}

fn labels<C: Compute>(graph: &ConfiguredGraph<C>, nodes: &[ConfiguredNode]) -> String {
    let mapping = &graph.packages.main_mapping;
    let mut out = String::new();
    for node in nodes {
        writeln!(out, "{}", node.display(mapping)).ok();
    }
    out
}

async fn label_kinds<C: Compute>(graph: &ConfiguredGraph<C>, nodes: &[ConfiguredNode]) -> Result<String, QueryError> {
    let mapping = &graph.packages.main_mapping;
    let mut out = String::new();
    for node in nodes {
        let target = graph.target_of(node).await?;
        writeln!(out, "{} {}", kind_of(&target), node.display(mapping)).ok();
    }
    Ok(out)
}

/// Rules as they'd be written in a BUILD file, with their attributes as
/// configured. As in Bazel, a rule in several configurations is written
/// once, as first configured.
async fn build<C: Compute>(graph: &ConfiguredGraph<C>, nodes: &[ConfiguredNode]) -> Result<String, QueryError> {
    let paths = RepoPaths::new(&graph.packages);
    let mut out = String::new();
    let mut written = std::collections::HashSet::new();
    for node in nodes {
        let target = graph.target_of(node).await?;
        if !matches!(target.target(), Target::Rule(_)) || !written.insert(node.label) {
            continue;
        }
        let attrs = graph.configured_attrs(node).await?;
        write_rule_with(&graph.packages, &target, AttrValues::Configured(&attrs), &paths, &mut out).await?;
    }
    Ok(out)
}

/// The result as a graph of its dependencies on each other.
async fn dot<C: Compute>(graph: &ConfiguredGraph<C>, result: &NodeSet<ConfiguredNode>, style: &DotStyle) -> Result<String, QueryError> {
    let mut digraph = Digraph::new(result.iter().copied());
    for node in result {
        for dep in graph.fwd_deps(node).await?.iter().filter(|dep| result.contains(dep)) {
            digraph.add_edge(node, dep);
        }
    }
    let mapping = &graph.packages.main_mapping;
    let mut out = String::new();
    digraph.write_dot(&mut out, style, |node| node.display(mapping), |_, _| Vec::new());
    Ok(out)
}

/// The files of each node's output groups, by exec path, each once: a
/// source file's own.
async fn files<C: Compute>(graph: &ConfiguredGraph<C>, nodes: &[ConfiguredNode], groups: &[String]) -> String {
    let mut out = String::new();
    let mut printed = std::collections::HashSet::new();
    for node in nodes {
        for artifact in node_files(graph, node, groups).await {
            if printed.insert(artifact.exec_path) {
                writeln!(out, "{}", artifact.exec_path).ok();
            }
        }
    }
    out
}

async fn node_files<C: Compute>(graph: &ConfiguredGraph<C>, node: &ConfiguredNode, groups: &[String]) -> Vec<Artifact> {
    let Some(target) = graph.configured(node).await else {
        let is_source = graph.target_of(node).await.is_ok_and(|target| matches!(target.target(), Target::SourceFile(_)));
        return match is_source {
            true => vec![Artifact::source(node.label)],
            false => Vec::new(),
        };
    };
    let Ok(providers) = target.providers() else {
        return Vec::new();
    };
    match providers.get() {
        Some(value) => value.output_group_files(groups),
        None => Vec::new(),
    }
}

/// The result as `analysis_v2.proto`'s messages: a `CqueryResult`, each
/// `ConfiguredTarget` prefixed by its length, or as text or JSON.
async fn proto_output<C: Compute>(graph: &ConfiguredGraph<C>, nodes: &[ConfiguredNode], format: CqueryFormat) -> Result<Vec<u8>, QueryError> {
    let paths = RepoPaths::new(&graph.packages);
    let mut configs: Vec<ConfigId> = Vec::new();
    let mut result = Message::new();
    let mut streamed = Vec::new();
    for node in nodes {
        let target = graph.target_of(node).await?;
        let attrs = graph.configured_attrs(node).await?;
        let values = match node.config {
            Some(_) => AttrValues::Configured(&attrs),
            None => AttrValues::Given,
        };
        let mut configured = Message::new();
        configured.message(1, "target", target_message(&graph.packages, &target, values, &paths, false).await?);
        if let Some(config) = node.config {
            let id = match configs.iter().position(|known| known.get().digest == config.get().digest) {
                Some(index) => index + 1,
                None => {
                    configs.push(config);
                    configs.len()
                }
            };
            configured.message(2, "configuration", configuration_message(config, 0));
            configured.int3(3, "configuration_id", id as i64);
        }
        match format {
            CqueryFormat::StreamedProto => streamed.extend(proto::delimited(&configured)),
            _ => {
                result.repeated(1, "results", configured);
            }
        }
    }
    if format == CqueryFormat::StreamedProto {
        return Ok(streamed);
    }
    for (index, config) in configs.iter().enumerate() {
        result.repeated(2, "configurations", configuration_message(*config, index as i64 + 1));
    }
    Ok(match format {
        CqueryFormat::TextProto => result.to_text().into_bytes(),
        CqueryFormat::JsonProto => format!("{}\n", result.to_json(false)).into_bytes(),
        _ => result.to_bytes(),
    })
}

/// A `Configuration` message: its id among those printed, its mnemonic,
/// CPU and checksum, and whether it's for tools.
pub fn configuration_message(config: ConfigId, id: i64) -> Message {
    let config = config.get();
    let mut message = Message::new();
    message.int3(1, "id", id).string3(2, "mnemonic", config.output_directory.clone()).string3(3, "platform_name", config.cpu.clone());
    message.string3(4, "checksum", config.digest.clone()).bool3(5, "is_tool", config.is_exec);
    message
}
