//! Printing `query` results in Bazel's output formats.

use crate::engine::{QueryEnvironment, QueryError};
use crate::functions::kind_of;
use crate::graph::{Digraph, DotStyle};
use crate::nodes::NodeSet;
use crate::parse::Expr;
use crate::universe::{TargetRef, Universe};
use crate::build_output::AttrValues;
use crate::proto::{self, Message};
use crate::{build_output, target_proto, xml_output};
use bazel_label::{Label, PackageId, RepoMappingTable};
use bazel_packages::{AttrValue, Target};
use skyframe::Compute;
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;
use std::path::PathBuf;

/// How query results are printed, as `--output` chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Label,
    LabelKind,
    Package,
    Location,
    Build,
    MinRank,
    MaxRank,
    Graph,
    Xml,
    Proto,
    StreamedProto,
    StreamedJsonProto,
}

impl OutputFormat {
    /// The formats `query --output` takes, as Bazel lists them.
    pub const NAMES: &'static [&'static str] = &[
        "label",
        "label_kind",
        "build",
        "minrank",
        "maxrank",
        "package",
        "location",
        "graph",
        "xml",
        "proto",
        "streamed_jsonproto",
        "streamed_proto",
    ];

    pub fn parse(name: &str) -> Option<OutputFormat> {
        Some(match name {
            "label" => OutputFormat::Label,
            "label_kind" => OutputFormat::LabelKind,
            "package" => OutputFormat::Package,
            "location" => OutputFormat::Location,
            "build" => OutputFormat::Build,
            "minrank" => OutputFormat::MinRank,
            "maxrank" => OutputFormat::MaxRank,
            "graph" => OutputFormat::Graph,
            "xml" => OutputFormat::Xml,
            "proto" => OutputFormat::Proto,
            "streamed_proto" => OutputFormat::StreamedProto,
            "streamed_jsonproto" => OutputFormat::StreamedJsonProto,
            _ => return None,
        })
    }

    /// Whether Bazel prints the format target by target, which
    /// `--order_output=auto` sorts by label.
    pub fn is_streamed(self) -> bool {
        !matches!(self, OutputFormat::MinRank | OutputFormat::MaxRank | OutputFormat::Graph)
    }
}

/// The order results are printed in, as `--order_output` chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderOutput {
    No,
    Deps,
    Auto,
    Full,
}

impl OrderOutput {
    pub fn parse(name: &str) -> Option<OrderOutput> {
        Some(match name {
            "no" => OrderOutput::No,
            "deps" => OrderOutput::Deps,
            "auto" => OrderOutput::Auto,
            "full" => OrderOutput::Full,
            _ => return None,
        })
    }
}

/// How to print a query's result.
pub struct OutputOptions {
    pub format: OutputFormat,
    pub order: OrderOutput,
    pub graph: DotStyle,
    /// Whether locations are relative to their repository's root,
    /// `--relative_locations`.
    pub relative_locations: bool,
    /// Whether the XML output lists attributes' default values,
    /// `--xml:default_values`.
    pub xml_default_values: bool,
    /// Whether text lines end with NULs rather than newlines, `--null`.
    pub null_terminated: bool,
}

impl Default for OutputOptions {
    fn default() -> OutputOptions {
        OutputOptions {
            format: OutputFormat::Label,
            order: OrderOutput::Auto,
            graph: DotStyle::default(),
            relative_locations: false,
            xml_default_values: false,
            null_terminated: false,
        }
    }
}

/// A label as Bazel prints it: `//pkg:name` in the main repository,
/// `@name//pkg:name` for repositories the main repository can see, and
/// `@@canonical//pkg:name` for the rest.
pub fn display_label(label: Label, mapping: &RepoMappingTable) -> String {
    format!("{}:{}", display_package(label.package, mapping), label.name)
}

pub fn display_package(package: PackageId, mapping: &RepoMappingTable) -> String {
    if package.repo.is_main() {
        return format!("//{}", package.path);
    }
    match mapping.apparent_name(package.repo) {
        Some(apparent) if !apparent.is_empty() => format!("@{apparent}//{}", package.path),
        _ => format!("@@{}//{}", package.repo, package.path),
    }
}

/// Print the result of `expr` as `options` say.
pub async fn format<C: Compute>(universe: &Universe<C>, result: &NodeSet<Label>, expr: &Expr, options: &OutputOptions) -> Result<Vec<u8>, QueryError> {
    if let OutputFormat::Proto | OutputFormat::StreamedProto | OutputFormat::StreamedJsonProto = options.format {
        let ordered = ordered(universe, result, expr, options.order).await?;
        return proto_output(universe, &ordered, options).await;
    }
    let text = format_text(universe, result, expr, options).await?;
    Ok(match options.null_terminated {
        true => text.replace('\n', "\0").into_bytes(),
        false => text.into_bytes(),
    })
}

/// The result in a text format.
async fn format_text<C: Compute>(universe: &Universe<C>, result: &NodeSet<Label>, expr: &Expr, options: &OutputOptions) -> Result<String, QueryError> {
    let mapping = universe.main_mapping.clone();
    let mut out = String::new();
    match options.format {
        OutputFormat::MinRank | OutputFormat::MaxRank => {
            let graph = result_graph(universe, result).await?;
            for (rank, label) in graph.ranks(options.format == OutputFormat::MaxRank) {
                writeln!(out, "{rank} {}", display_label(*label, &mapping)).ok();
            }
        }
        OutputFormat::Graph => {
            let graph = result_graph(universe, result).await?;
            let conditions = conditional_edges(universe, result).await?;
            let name = |label: &Label| display_label(*label, &mapping);
            let condition = |from: &Label, to: &Label| conditions.get(&(*from, *to)).cloned().unwrap_or_default();
            graph.write_dot(&mut out, &options.graph, name, condition);
        }
        format => {
            let ordered = ordered(universe, result, expr, options.order).await?;
            write_streamed(universe, &ordered, format, options, &mut out).await?;
        }
    }
    Ok(out)
}

/// The result in the order `order` gives: by label, unless the query is a
/// path, whose order it keeps, or in dependency order.
async fn ordered<C: Compute>(universe: &Universe<C>, result: &NodeSet<Label>, expr: &Expr, order: OrderOutput) -> Result<Vec<Label>, QueryError> {
    match order {
        OrderOutput::Auto if expr.is_top_level_somepath() => Ok(result.iter().copied().collect()),
        OrderOutput::Auto | OrderOutput::No => {
            let mut sorted: Vec<Label> = result.iter().copied().collect();
            sorted.sort();
            Ok(sorted)
        }
        OrderOutput::Deps | OrderOutput::Full => {
            let graph = result_graph(universe, result).await?;
            Ok(graph.topological_order().into_iter().copied().collect())
        }
    }
}

async fn write_streamed<C: Compute>(universe: &Universe<C>, labels: &[Label], format: OutputFormat, options: &OutputOptions, out: &mut String) -> Result<(), QueryError> {
    debug_assert!(format.is_streamed());
    let mapping = universe.main_mapping.clone();
    let paths = RepoPaths::new(universe);
    match format {
        OutputFormat::Label => {
            for label in labels {
                writeln!(out, "{}", display_label(*label, &mapping)).ok();
            }
        }
        OutputFormat::LabelKind => {
            for target in universe.targets_in_order(labels).await? {
                writeln!(out, "{} {}", kind_of(&target), display_label(target.label(), &mapping)).ok();
            }
        }
        OutputFormat::Package => {
            let packages: BTreeSet<String> = labels.iter().map(|label| package_name(label.package, &mapping)).collect();
            for package in packages {
                writeln!(out, "{package}").ok();
            }
        }
        OutputFormat::Location => {
            for target in universe.targets_in_order(labels).await? {
                let location = paths.location(&target, options.relative_locations).await?;
                writeln!(out, "{location}: {} {}", kind_of(&target), display_label(target.label(), &mapping)).ok();
            }
        }
        OutputFormat::Build => {
            let mut printed = BTreeSet::new();
            for target in universe.targets_in_order(labels).await? {
                let Some(rule) = generating_rule(&target) else {
                    continue;
                };
                if printed.insert(rule.label()) {
                    build_output::write_rule(universe, &rule, &paths, out).await?;
                }
            }
        }
        OutputFormat::Xml => xml_output::write(universe, labels, &paths, options, out).await?,
        _ => {}
    }
    Ok(())
}

/// The targets as `build.proto` messages: a `QueryResult`, or each
/// `Target` prefixed by its length, or as JSON on a line of its own.
async fn proto_output<C: Compute>(universe: &Universe<C>, labels: &[Label], options: &OutputOptions) -> Result<Vec<u8>, QueryError> {
    let paths = RepoPaths::new(universe);
    let mut result = Message::new();
    let mut out = Vec::new();
    for target in universe.targets_in_order(labels).await? {
        let message = target_proto::target_message(universe, &target, AttrValues::Given, &paths, options.relative_locations).await?;
        match options.format {
            OutputFormat::StreamedProto => out.extend(proto::delimited(&message)),
            OutputFormat::StreamedJsonProto => {
                out.extend(message.to_json(true).into_bytes());
                out.push(b'\n');
            }
            _ => {
                result.repeated(1, "target", message);
            }
        }
    }
    if options.format == OutputFormat::Proto {
        out = result.to_bytes();
    }
    Ok(out)
}

/// A package as `--output=package` prints it: without a leading `//`.
fn package_name(package: PackageId, mapping: &RepoMappingTable) -> String {
    let display = display_package(package, mapping);
    match display.strip_prefix("//") {
        Some(name) => name.to_string(),
        None => display,
    }
}

/// The rule a target is, or that generates it.
fn generating_rule(target: &TargetRef) -> Option<TargetRef> {
    match target.target() {
        Target::Rule(_) => Some(target.clone()),
        Target::OutputFile { rule, .. } => target.sibling(*rule),
        _ => None,
    }
}

/// The graph of the result: its nodes, and the dependencies among them.
async fn result_graph<C: Compute>(universe: &Universe<C>, result: &NodeSet<Label>) -> Result<Digraph<Label>, QueryError> {
    let mut graph = Digraph::new(result.iter().copied());
    for label in result {
        let Ok(deps) = universe.fwd_deps(label).await else {
            continue;
        };
        for dep in deps.iter().filter(|dep| result.contains(dep)) {
            graph.add_edge(label, dep);
        }
    }
    Ok(graph)
}

/// The conditions of the `select()` branches each edge of the result is
/// taken under, as `--output=graph` labels edges.
async fn conditional_edges<C: Compute>(universe: &Universe<C>, result: &NodeSet<Label>) -> Result<HashMap<(Label, Label), Vec<String>>, QueryError> {
    let mut edges: HashMap<(Label, Label), Vec<String>> = HashMap::new();
    for label in result {
        let Ok(target) = universe.target(*label).await else {
            continue;
        };
        let Target::Rule(rule) = target.target() else {
            continue;
        };
        for (_, value) in &rule.attrs {
            let AttrValue::Configurable(parts) = value else {
                continue;
            };
            for part in parts {
                let bazel_packages::SelectorPart::Select(select) = part else {
                    continue;
                };
                for (condition, branch) in &select.branches {
                    let mut labels = Vec::new();
                    branch.labels(&mut labels);
                    for dep in labels.into_iter().filter(|dep| result.contains(dep)) {
                        let conditions = edges.entry((*label, dep)).or_default();
                        let condition = condition.to_string();
                        if !conditions.contains(&condition) {
                            conditions.push(condition);
                        }
                    }
                }
            }
        }
    }
    Ok(edges)
}

/// Resolves repositories to directories, for printing file locations.
pub struct RepoPaths<'a, C> {
    universe: &'a Universe<C>,
}

impl<'a, C: Compute> RepoPaths<'a, C> {
    pub fn new(universe: &'a Universe<C>) -> RepoPaths<'a, C> {
        RepoPaths { universe }
    }

    pub async fn package_dir(&self, package: PackageId) -> Result<PathBuf, QueryError> {
        self.universe.package_dir(package).await
    }

    /// The path of a file named by its label, as an error or call stack
    /// names it, or the label if it can't be found.
    pub async fn file_path(&self, file: &str) -> String {
        let Ok(label) = Label::parse_canonical(file) else {
            return file.to_string();
        };
        match self.package_dir(label.package).await {
            Ok(dir) => dir.join(label.name.as_str()).display().to_string(),
            Err(_) => file.to_string(),
        }
    }

    /// Where a target is defined: `path:line:column`, relative to its
    /// repository's root if `relative`.
    pub async fn location(&self, target: &TargetRef, relative: bool) -> Result<String, QueryError> {
        let package = target.package();
        let dir = self.package_dir(package.id).await?;
        let build_file = dir.join(package.build_file.name.as_str());
        let rule_location = |rule: &bazel_packages::Rule| format!("{}:{}:{}", build_file.display(), rule.location.line, rule.location.column);
        let location = match target.target() {
            Target::Rule(rule) => rule_location(rule),
            Target::OutputFile { rule, .. } => match package.target(*rule).and_then(Target::as_rule) {
                Some(rule) => rule_location(rule),
                None => format!("{}:1:1", build_file.display()),
            },
            Target::SourceFile(file) => format!("{}:1:1", dir.join(file.name.as_str()).display()),
            Target::PackageGroup(_) => format!("{}:1:1", build_file.display()),
        };
        if !relative {
            return Ok(location);
        }
        let root = self.package_dir(PackageId::new(package.id.repo, intern::PathId::ROOT)).await?;
        let root = format!("{}/", root.display());
        Ok(location.strip_prefix(&root).map(str::to_string).unwrap_or(location))
    }
}

impl<C: Compute> Universe<C> {
    /// The targets of `labels` that exist, in their order.
    async fn targets_in_order(&self, labels: &[Label]) -> Result<Vec<TargetRef>, QueryError> {
        let mut found = Vec::with_capacity(labels.len());
        for label in labels {
            match self.target(*label).await {
                Ok(target) => found.push(target),
                Err(error) => self.error(&label.to_string(), error.message)?,
            }
        }
        Ok(found)
    }
}
