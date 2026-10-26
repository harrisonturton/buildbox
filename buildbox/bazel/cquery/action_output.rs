//! Printing `aquery` results: the actions of the configured targets a
//! query evaluates to, as `aquery`'s output formats show them.

use crate::configured_graph::ConfiguredGraph;
use crate::node::ConfiguredNode;
use bazel_analysis::{Action, ActionKind, Artifact, ConfiguredTarget};
use bazel_config::{AspectDescriptor, ConfigId};
use bazel_label::{Label, RepoMappingTable};
use bazel_query::proto::Message;
use bazel_query::{Arg, Expr, NodeSet, QueryError, display_label};
use bazel_ui::shell_escape;
use regex::Regex;
use skyframe::Compute;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::sync::Arc;

/// How `aquery` prints results, as `--output` chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AqueryFormat {
    Text,
    Summary,
    Commands,
    Proto,
    StreamedProto,
    TextProto,
    JsonProto,
}

impl AqueryFormat {
    /// The formats `aquery --output` takes, as Bazel lists them.
    pub const NAMES: &'static [&'static str] = &["text", "summary", "commands", "proto", "streamed_proto", "textproto", "jsonproto"];

    pub fn parse(name: &str) -> Option<AqueryFormat> {
        Some(match name {
            "text" => AqueryFormat::Text,
            "summary" => AqueryFormat::Summary,
            "commands" => AqueryFormat::Commands,
            "proto" => AqueryFormat::Proto,
            "streamed_proto" => AqueryFormat::StreamedProto,
            "textproto" => AqueryFormat::TextProto,
            "jsonproto" => AqueryFormat::JsonProto,
            _ => return None,
        })
    }
}

/// How to print an `aquery` result.
pub struct AqueryOutput {
    pub format: AqueryFormat,
    /// Whether to show actions' command lines, `--include_commandline`.
    pub include_commandline: bool,
    /// Whether to show actions' inputs and outputs, `--include_artifacts`.
    pub include_artifacts: bool,
}

/// The patterns `inputs()`, `outputs()` and `mnemonic()` filter actions
/// by: an action matches if its mnemonic matches every mnemonic pattern,
/// one of its inputs every inputs pattern, and one of its outputs every
/// outputs pattern.
#[derive(Default)]
pub struct ActionFilters {
    mnemonic: Vec<Regex>,
    inputs: Vec<Regex>,
    outputs: Vec<Regex>,
}

impl ActionFilters {
    /// The filters an expression's calls of `inputs()`, `outputs()` and
    /// `mnemonic()` give, wherever they are in it.
    pub fn of(expr: &Expr) -> Result<ActionFilters, QueryError> {
        let mut filters = ActionFilters::default();
        filters.collect(expr)?;
        Ok(filters)
    }

    fn collect(&mut self, expr: &Expr) -> Result<(), QueryError> {
        match expr {
            Expr::Target(_) | Expr::Set(_) => Ok(()),
            Expr::Let { value, body, .. } => {
                self.collect(value)?;
                self.collect(body)
            }
            Expr::Binary { operands, .. } => operands.iter().try_for_each(|operand| self.collect(operand)),
            Expr::Function { name, args } => {
                if let (Some(filters), Some(Arg::Word(pattern))) = (self.filters_for(name), args.first()) {
                    let anchored = Regex::new(&format!("^(?:{pattern})$")).map_err(|error| QueryError::in_evaluation(format!("illegal '{name}' pattern regexp '{pattern}': {error}")))?;
                    filters.push(anchored);
                }
                for arg in args {
                    if let Arg::Expr(expr) = arg {
                        self.collect(expr)?;
                    }
                }
                Ok(())
            }
        }
    }

    fn filters_for(&mut self, function: &str) -> Option<&mut Vec<Regex>> {
        match function {
            "mnemonic" => Some(&mut self.mnemonic),
            "inputs" => Some(&mut self.inputs),
            "outputs" => Some(&mut self.outputs),
            _ => None,
        }
    }

    fn matches(&self, action: &Action) -> bool {
        let all = |patterns: &[Regex], text: &str| patterns.iter().all(|pattern| pattern.is_match(text));
        if !all(&self.mnemonic, action.mnemonic.as_str()) {
            return false;
        }
        if !self.inputs.is_empty() && !action.all_inputs().iter().any(|input| all(&self.inputs, &input.exec_path.to_string())) {
            return false;
        }
        self.outputs.is_empty() || action.outputs.iter().any(|output| all(&self.outputs, &output.exec_path.to_string()))
    }
}

/// An action of a configured target the query evaluated to, and for a
/// command, its arguments, with those of its params files in place of the
/// arguments naming them.
pub(crate) struct Owned {
    pub target: Arc<ConfiguredTarget>,
    pub action: Action,
    pub arguments: Vec<String>,
    /// For an action of aspects applied to the target: the aspects, in the
    /// order they apply.
    pub aspects: Vec<AspectDescriptor>,
}

impl Owned {
    /// The files the action reads, as Bazel lists them: without the
    /// params files a command writes as it runs.
    pub fn inputs(&self) -> Vec<Artifact> {
        let mut inputs = self.action.all_inputs();
        if let ActionKind::Spawn(spawn) = &self.action.kind {
            inputs.retain(|input| !spawn.param_files.iter().any(|param_file| param_file.path == input.exec_path));
        }
        inputs
    }
}

/// The actions of the targets of `result` that `filters` match, target
/// by target, each target's in the order its rule made them. The actions
/// writing a command's params files aren't listed: Bazel writes them as
/// part of the command.
async fn actions<C: Compute>(graph: &ConfiguredGraph<C>, result: &NodeSet<ConfiguredNode>, filters: &ActionFilters) -> Result<Vec<Owned>, QueryError> {
    let mut nodes: Vec<ConfiguredNode> = result.iter().copied().collect();
    nodes.sort();
    let applications = match graph.options.include_aspects {
        true => graph.aspect_applications().await,
        false => HashMap::new(),
    };
    let mut owned = Vec::new();
    for node in nodes {
        let Some(target) = graph.configured(&node).await else {
            continue;
        };
        owned.extend(target_actions(&target, filters)?);
        let Some(key) = node.key() else {
            continue;
        };
        for application in applications.get(&key).into_iter().flatten() {
            let node = ConfiguredNode { label: application.label, config: Some(application.config) };
            if let Some(applied) = graph.configured(&node).await {
                owned.extend(target_actions(&applied, filters)?);
            }
        }
    }
    Ok(owned)
}

/// A configured target's actions that `filters` match, but for those
/// writing its commands' params files.
fn target_actions(target: &Arc<ConfiguredTarget>, filters: &ActionFilters) -> Result<Vec<Owned>, QueryError> {
    let actions: Vec<Action> = target.actions.iter().map(|stored| stored.load()).collect::<Result<_, _>>()?;
    let params = params_files(&actions);
    let aspects = target.config.get().aspects.clone();
    let mut owned = Vec::new();
    for action in actions {
        let writes_params = action.outputs.iter().any(|output| params.contains_key(&output.exec_path));
        if writes_params || !filters.matches(&action) {
            continue;
        }
        let arguments = expanded_arguments(&action, &params);
        owned.push(Owned { target: target.clone(), action, arguments, aspects: aspects.clone() });
    }
    Ok(owned)
}

/// The contents of the params files a target's commands read, by path.
fn params_files(actions: &[Action]) -> HashMap<intern::PathId, String> {
    let mut wanted = Vec::new();
    for action in actions {
        if let ActionKind::Spawn(spawn) = &action.kind {
            wanted.extend(spawn.param_files.iter().map(|param_file| param_file.path));
        }
    }
    let mut contents = HashMap::new();
    for action in actions {
        let (ActionKind::Write { content, .. }, Some(output)) = (&action.kind, action.outputs.first()) else {
            continue;
        };
        if wanted.contains(&output.exec_path) {
            contents.insert(output.exec_path, content.clone());
        }
    }
    contents
}

/// A command's arguments, those of its params files in place of the
/// arguments naming them, as Bazel's aquery shows them.
fn expanded_arguments(action: &Action, params: &HashMap<intern::PathId, String>) -> Vec<String> {
    let ActionKind::Spawn(spawn) = &action.kind else {
        return Vec::new();
    };
    let mut arguments = Vec::with_capacity(spawn.arguments.len());
    for (index, argument) in spawn.arguments.iter().enumerate() {
        let param_file = spawn.param_files.iter().find(|param_file| param_file.argument as usize == index);
        match param_file.and_then(|param_file| Some((param_file, params.get(&param_file.path)?))) {
            Some((param_file, contents)) => arguments.extend(bazel_analysis::param_file_arguments(contents, &param_file.format)),
            None => arguments.push(argument.clone()),
        }
    }
    arguments
}

/// Print the actions of the result's targets as `output` says.
pub async fn format<C: Compute>(graph: &ConfiguredGraph<C>, result: &NodeSet<ConfiguredNode>, filters: &ActionFilters, output: &AqueryOutput) -> Result<Vec<u8>, QueryError> {
    let owned = actions(graph, result, filters).await?;
    let mapping = &graph.packages.main_mapping;
    Ok(match output.format {
        AqueryFormat::Text => owned.iter().map(|owned| text(owned, output, mapping)).collect::<String>().into_bytes(),
        AqueryFormat::Commands => owned.iter().filter_map(command).collect::<String>().into_bytes(),
        AqueryFormat::Summary => summary(&owned, mapping).into_bytes(),
        AqueryFormat::Proto => container(&owned, output, mapping).to_bytes(),
        AqueryFormat::StreamedProto => crate::action_proto::streamed(&owned, output, mapping),
        AqueryFormat::TextProto => crate::action_proto::text(&owned, output, mapping).into_bytes(),
        AqueryFormat::JsonProto => format!("{}\n", container(&owned, output, mapping).to_json(false)).into_bytes(),
    })
}

fn container(owned: &[Owned], output: &AqueryOutput, mapping: &RepoMappingTable) -> Message {
    crate::action_proto::container(owned, output, mapping)
}

/// An aspect as `aquery` describes it: its class, then its parameters.
pub fn aspect_description(aspect: &AspectDescriptor, mapping: &RepoMappingTable) -> String {
    let parameters: Vec<String> = aspect_parameters(aspect).iter().map(|(name, value)| format!("{name}='{value}'")).collect();
    format!("{}({})", aspect_name(aspect, mapping), parameters.join(", "))
}

/// An aspect's class, as Bazel names it: `//pkg:defs.bzl%name`.
pub fn aspect_name(aspect: &AspectDescriptor, mapping: &RepoMappingTable) -> String {
    format!("{}%{}", display_label(aspect.file, mapping), aspect.name)
}

/// An aspect's parameters, with their values as Bazel's Java writes them.
pub fn aspect_parameters(aspect: &AspectDescriptor) -> Vec<(String, String)> {
    let value = |text: &str| match text {
        "True" => "true".to_string(),
        "False" => "false".to_string(),
        _ => bazel_config::unquoted(text).unwrap_or_else(|| text.to_string()),
    };
    aspect.params.iter().map(|(name, text)| (name.clone(), value(text))).collect()
}

/// An action's key: a hash of what it does, which identifies it.
pub fn action_key(action: &Action) -> String {
    let identity = format!("{}\0{:?}\0{:?}\0{:?}", action.mnemonic, action.all_inputs(), action.outputs, action.kind);
    let digest = ring::digest::digest(&ring::digest::SHA256, identity.as_bytes());
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The platform an action runs on.
pub fn exec_platform(target: &ConfiguredTarget) -> Label {
    target.exec_platform.unwrap_or(target.config.get().host_platform)
}

/// An action as `--output=text` shows it.
fn text(owned: &Owned, output: &AqueryOutput, mapping: &RepoMappingTable) -> String {
    let Owned { target, action, arguments, aspects } = owned;
    let config = target.config.get();
    let platform = exec_platform(target);
    let mut out = String::new();
    let owner = bazel_analysis::ConfiguredTargetKey { label: target.label, config: target.config };
    writeln!(out, "action '{}'", action.description(&owner)).ok();
    writeln!(out, "  Mnemonic: {}", action.mnemonic).ok();
    writeln!(out, "  Target: {}", display_label(target.label, mapping)).ok();
    writeln!(out, "  Configuration: {}", config.output_directory).ok();
    writeln!(out, "  Execution platform: {}", display_label(platform, mapping)).ok();
    if !aspects.is_empty() {
        let described: Vec<String> = aspects.iter().rev().map(|aspect| aspect_description(aspect, mapping)).collect();
        writeln!(out, "  AspectDescriptors: [{}]", described.join("\n    -> ")).ok();
    }
    writeln!(out, "  ActionKey: {}", action_key(action)).ok();
    if output.include_artifacts {
        writeln!(out, "  Inputs: [{}]", sorted_paths(&owned.inputs()).join(", ")).ok();
        writeln!(out, "  Outputs: [{}]", sorted_paths(&action.outputs).join(", ")).ok();
    }
    if let ActionKind::Spawn(spawn) = &action.kind {
        if !spawn.env.is_empty() {
            let env: Vec<String> = spawn.env.iter().map(|(name, value)| format!("{name}={value}")).collect();
            writeln!(out, "  Environment: [{}]", env.join(", ")).ok();
        }
        if output.include_commandline {
            writeln!(out, "  Command Line: {}", command_line(arguments, target.config, platform)).ok();
        }
        if !spawn.execution_info.is_empty() {
            let info: Vec<String> = spawn.execution_info.iter().map(|(key, value)| format!("{}: {}", shell_escape(key), shell_escape(value))).collect();
            writeln!(out, "  ExecutionInfo: {{{}}}", info.join(", ")).ok();
        }
    }
    match &action.kind {
        ActionKind::Write { executable, .. } | ActionKind::Template { executable, .. } => {
            writeln!(out, "  IsExecutable: {executable}").ok();
        }
        _ => {}
    }
    out.push('\n');
    out
}

fn sorted_paths(artifacts: &[Artifact]) -> Vec<String> {
    let mut paths: Vec<String> = artifacts.iter().map(|artifact| artifact.exec_path.to_string()).collect();
    paths.sort();
    paths.dedup();
    paths
}

/// A command line as a script that runs it, as Bazel describes commands,
/// with the configuration and platform it runs in.
fn command_line(arguments: &[String], config: ConfigId, platform: Label) -> String {
    let escaped: Vec<String> = arguments.iter().map(|argument| shell_escape(argument)).collect();
    format!("(exec {})\n# Configuration: {}\n# Execution platform: {platform}", escaped.join(" \\\n    "), config.get().digest)
}

/// An action's command line on one line, as `--output=commands` shows it;
/// nothing for actions that don't run a command.
fn command(owned: &Owned) -> Option<String> {
    let ActionKind::Spawn(_) = &owned.action.kind else {
        return None;
    };
    let escaped: Vec<String> = owned.arguments.iter().map(|argument| shell_escape(argument)).collect();
    Some(format!("{}\n", escaped.join(" ")))
}

/// How many actions there are, and of each mnemonic, configuration,
/// execution platform and aspect, fewest first.
fn summary(owned: &[Owned], mapping: &RepoMappingTable) -> String {
    let mut mnemonics: HashMap<String, usize> = HashMap::new();
    let mut configurations: HashMap<String, usize> = HashMap::new();
    let mut aspects: HashMap<String, usize> = HashMap::new();
    let mut platforms: HashMap<String, usize> = HashMap::new();
    for Owned { target, action, aspects: applied, .. } in owned {
        *mnemonics.entry(action.mnemonic.to_string()).or_default() += 1;
        for aspect in applied {
            *aspects.entry(aspect_name(aspect, mapping)).or_default() += 1;
        }
        *configurations.entry(target.config.get().output_directory.clone()).or_default() += 1;
        *platforms.entry(display_label(exec_platform(target), mapping)).or_default() += 1;
    }
    let mut out = match owned.len() {
        0 => "No actions matched.\n".to_string(),
        1 => "1 total action.\n".to_string(),
        count => format!("{count} total actions.\n"),
    };
    for (title, counts) in [("Mnemonics:", mnemonics), ("Configurations:", configurations), ("Execution Platforms:", platforms), ("Aspects:", aspects)] {
        if counts.is_empty() {
            continue;
        }
        let mut counts: Vec<(usize, String)> = counts.into_iter().map(|(key, count)| (count, key)).collect();
        counts.sort();
        writeln!(out, "\n{title}").ok();
        for (count, key) in counts {
            writeln!(out, "  {key}: {count}").ok();
        }
    }
    out
}

/// Ids for the distinct values of a table, in order of first appearance,
/// as `aquery`'s protos number them from 1.
pub struct Ids<K> {
    ids: BTreeMap<K, u32>,
    pub order: Vec<K>,
}

impl<K: Ord + Clone> Ids<K> {
    pub fn new() -> Ids<K> {
        Ids {
            ids: BTreeMap::new(),
            order: Vec::new(),
        }
    }

    /// The id of `key`, and whether it's new.
    pub fn id(&mut self, key: &K) -> (u32, bool) {
        if let Some(id) = self.ids.get(key) {
            return (*id, false);
        }
        let id = self.order.len() as u32 + 1;
        self.ids.insert(key.clone(), id);
        self.order.push(key.clone());
        (id, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(text: &str) -> Expr {
        bazel_query::parse(text, &crate::Language::Aquery.functions()).unwrap()
    }

    #[test]
    fn collects_filters_from_anywhere_in_the_expression() {
        let filters = ActionFilters::of(&query("mnemonic('Lib.*', deps(inputs('.*\\.txt', //:a)) + //:b)")).unwrap();
        assert_eq!(filters.mnemonic.len(), 1);
        assert_eq!(filters.inputs.len(), 1);
        assert!(filters.outputs.is_empty());
        assert!(filters.mnemonic[0].is_match("LibCat"));
        assert!(!filters.mnemonic[0].is_match("MyLibCat"), "patterns match whole strings");
    }

    #[test]
    fn describes_command_lines_as_scripts() {
        let config = bazel_config::no_config();
        let platform = Label::parse_canonical("@@platforms//host:host").unwrap();
        let described = command_line(&["/bin/bash".into(), "-c".into(), "echo $@ > out".into(), "".into()], config, platform);
        let expected = format!("(exec /bin/bash \\\n    -c \\\n    'echo $@ > out' \\\n    '')\n# Configuration: {}\n# Execution platform: @@platforms//host:host", config.get().digest);
        assert_eq!(described, expected);
    }
}
