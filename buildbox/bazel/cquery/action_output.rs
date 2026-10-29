//! Printing `aquery` results: the actions of the configured targets a
//! query evaluates to, as `aquery`'s output formats show them.

use crate::action_inputs::{PathNames, ShownInputs};
use crate::action_key::ActionKeys;
use crate::configured_graph::ConfiguredGraph;
use crate::node::ConfiguredNode;
use crate::runfiles_actions::{ManifestContents, runfiles_actions};
use bazel_analysis::{Action, ActionKind, Artifact, ConfiguredTarget, KnownSets};
use bazel_config::{AspectDescriptor, ConfigId};
use bazel_label::{Label, RepoMappingTable};
use bazel_loading::SourceTextKey;
use bazel_query::{Arg, Expr, Node, NodeSet, QueryError, display_label};
use bazel_ui::shell_escape;
use intern::PathId;
use regex::Regex;
use rustc_hash::FxHashMap;
use skyframe::Compute;
use std::collections::HashMap;
use std::fmt::Write;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

/// The mnemonic of Bazel's template expansions, the only actions whose
/// templates `aquery` shows.
pub(crate) const TEMPLATE_EXPAND: &str = "TemplateExpand";

/// The mnemonic of C++ compiles. Bazel computes their environment as they
/// run, so `--output=text` lists their configuration's instead.
const CPP_COMPILE: &str = "CppCompile";

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
#[derive(Clone)]
pub struct AqueryOutput {
    pub format: AqueryFormat,
    /// Whether to show actions' command lines, `--include_commandline`.
    pub include_commandline: bool,
    /// Whether to show actions' inputs and outputs, `--include_artifacts`.
    pub include_artifacts: bool,
    /// Whether to show what actions that write files write,
    /// `--include_file_write_contents`.
    pub include_file_write_contents: bool,
    /// The execution root, which runfiles manifests name files in.
    pub exec_root: std::path::PathBuf,
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
    /// Whether each input set met so far has an input the inputs
    /// patterns match, by the set's identity: actions share most of them.
    sets_matched: Mutex<KnownSets>,
    /// Whether the inputs patterns match each path met so far.
    paths_matched: Mutex<FxHashMap<PathId, bool>>,
}

impl ActionFilters {
    /// The filters an expression's calls of `inputs()`, `outputs()` and
    /// `mnemonic()` give, wherever they are in it.
    pub fn of(expr: &Expr) -> Result<ActionFilters, QueryError> {
        let mut filters = ActionFilters::default();
        for id in expr.preorder() {
            if let Node::Function { name, args } = expr.node(id) {
                filters.collect(name, args)?;
            }
        }
        Ok(filters)
    }

    /// The filter a call gives, if it's a call of a filter.
    fn collect(&mut self, name: &str, args: &[Arg]) -> Result<(), QueryError> {
        if let (Some(filters), Some(Arg::Word(pattern))) = (self.filters_for(name), args.first()) {
            let anchored = Regex::new(&format!("^(?:{pattern})$")).map_err(|error| QueryError::in_evaluation(format!("illegal '{name}' pattern regexp '{pattern}': {error}")))?;
            filters.push(anchored);
        }
        Ok(())
    }

    fn filters_for(&mut self, function: &str) -> Option<&mut Vec<Regex>> {
        match function {
            "mnemonic" => Some(&mut self.mnemonic),
            "inputs" => Some(&mut self.inputs),
            "outputs" => Some(&mut self.outputs),
            _ => None,
        }
    }

    fn matches(&self, action: &Action, names: &mut PathNames) -> bool {
        if !all_match(&self.mnemonic, action.mnemonic.as_str()) {
            return false;
        }
        if !self.inputs.is_empty() && !self.any_input_matches(action, names) {
            return false;
        }
        self.outputs.is_empty() || action.outputs.iter().any(|output| all_match(&self.outputs, &names.name(output.exec_path)))
    }

    fn any_input_matches(&self, action: &Action, names: &mut PathNames) -> bool {
        let inputs = ShownInputs::of(action);
        let mut paths_matched = self.paths_matched.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut path_matches = |path: PathId| *paths_matched.entry(path).or_insert_with(|| all_match(&self.inputs, &names.name(path)));
        if inputs.extra.iter().any(|path| path_matches(*path)) {
            return true;
        }
        if !inputs.hidden.is_empty() {
            let mut matched = false;
            inputs.for_each(|path| matched = matched || path_matches(path));
            return matched;
        }
        let mut sets_matched = self.sets_matched.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        inputs.set.any(&mut sets_matched, |artifact| path_matches(artifact.exec_path))
    }
}

fn all_match(patterns: &[Regex], text: &str) -> bool {
    patterns.iter().all(|pattern| pattern.is_match(text))
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

/// What printing actions keeps from one to the next: how to print them,
/// paths as text, action keys, and templates' text.
pub(crate) struct Printer {
    pub output: AqueryOutput,
    names: PathNames,
    keys: ActionKeys,
    templates: HashMap<PathId, String>,
}

impl Printer {
    /// The key of `action`, which reads `inputs`.
    pub fn action_key(&mut self, action: &Action, inputs: &ShownInputs) -> String {
        self.keys.key(action, inputs, &mut self.names)
    }

    /// A template's text, as Bazel shows it: a generated template's isn't
    /// known before it's built, so it's named instead.
    pub fn template_content(&self, template: &Artifact) -> String {
        match self.templates.get(&template.exec_path) {
            Some(text) => text.clone(),
            None => format!("ARTIFACT: {}", template.exec_path),
        }
    }
}

/// The actions of the targets of `result` that `filters` match, target
/// by target, each target's in the order its rule made them, then those
/// laying out its runfiles. The actions writing a command's params files
/// aren't listed: Bazel writes them as part of the command.
async fn actions<C: Compute>(graph: &ConfiguredGraph<C>, result: &NodeSet<ConfiguredNode>, filters: &ActionFilters, output: &AqueryOutput, names: &mut PathNames) -> Result<Vec<Owned>, QueryError> {
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
        owned.extend(target_actions(&target, filters, names)?);
        let contents = match output.include_file_write_contents && target.executable.is_some() {
            true => Some(ManifestContents {
                sources: graph.transitive_repos(&node).await,
                exec_root: &output.exec_root,
            }),
            false => None,
        };
        let runfiles = runfiles_actions(graph.packages.compute(), &target, contents).await?;
        owned.extend(runfiles.into_iter().filter(|action| filters.matches(action, names)).map(|action| unexpanded(&target, action)));
        let Some(key) = node.key() else {
            continue;
        };
        for application in applications.get(&key).into_iter().flatten() {
            let node = ConfiguredNode { label: application.label, config: Some(application.config) };
            if let Some(applied) = graph.configured(&node).await {
                owned.extend(target_actions(&applied, filters, names)?);
            }
        }
    }
    Ok(owned)
}

/// A configured target's actions that `filters` match, but for those
/// writing its commands' params files.
fn target_actions(target: &Arc<ConfiguredTarget>, filters: &ActionFilters, names: &mut PathNames) -> Result<Vec<Owned>, QueryError> {
    let actions: Vec<Action> = target.actions.iter().map(|stored| stored.load()).collect::<Result<_, _>>()?;
    let params = params_files(&actions);
    let aspects = target.config.get().aspects.clone();
    let mut owned = Vec::new();
    for action in actions {
        let writes_params = action.outputs.iter().any(|output| params.contains_key(&output.exec_path));
        if writes_params || !filters.matches(&action, names) {
            continue;
        }
        let arguments = expanded_arguments(&action, &params);
        owned.push(Owned { target: target.clone(), action, arguments, aspects: aspects.clone() });
    }
    Ok(owned)
}

/// An action that runs no command, of `target` itself.
fn unexpanded(target: &Arc<ConfiguredTarget>, action: Action) -> Owned {
    Owned {
        target: target.clone(),
        action,
        arguments: Vec::new(),
        aspects: Vec::new(),
    }
}

/// The contents of the params files a target's commands read, by path.
fn params_files(actions: &[Action]) -> HashMap<PathId, String> {
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
fn expanded_arguments(action: &Action, params: &HashMap<PathId, String>) -> Vec<String> {
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

/// The text of the source templates of the template expansions among
/// `owned`, by path.
async fn template_texts(compute: &impl Compute, owned: &[Owned]) -> HashMap<PathId, String> {
    let mut templates: Vec<Artifact> = Vec::new();
    for Owned { action, .. } in owned {
        if let (ActionKind::Template { template, .. }, TEMPLATE_EXPAND) = (&action.kind, action.mnemonic.as_str()) {
            if template.is_source() && !templates.contains(template) {
                templates.push(*template);
            }
        }
    }
    let keys: Vec<SourceTextKey> = templates.iter().map(|template| SourceTextKey(template.owner)).collect();
    let texts = compute.compute_many(&keys).await;
    let mut by_path = HashMap::new();
    for (template, text) in templates.iter().zip(texts) {
        if let Ok(text) = text {
            if let Some(text) = &text.0 {
                by_path.insert(template.exec_path, text.clone());
            }
        }
    }
    by_path
}

/// Print the actions of the result's targets as `output` says.
pub async fn format<C: Compute>(graph: &ConfiguredGraph<C>, result: &NodeSet<ConfiguredNode>, filters: &ActionFilters, output: &AqueryOutput) -> Result<Vec<u8>, QueryError> {
    let mut names = PathNames::default();
    let owned = actions(graph, result, filters, output, &mut names).await?;
    let templates = template_texts(graph.packages.compute(), &owned).await;
    let mut printer = Printer {
        output: output.clone(),
        names,
        keys: ActionKeys::default(),
        templates,
    };
    let mapping = &graph.packages.main_mapping;
    Ok(match output.format {
        AqueryFormat::Text => texts(&owned, &mut printer, mapping).into_bytes(),
        AqueryFormat::Commands => owned.iter().filter_map(command).collect::<String>().into_bytes(),
        AqueryFormat::Summary => summary(&owned, mapping).into_bytes(),
        AqueryFormat::Proto => crate::action_proto::container(&owned, &mut printer).to_bytes(),
        AqueryFormat::StreamedProto => crate::action_proto::streamed(&owned, &mut printer),
        AqueryFormat::TextProto => crate::action_proto::text(&owned, &mut printer).into_bytes(),
        AqueryFormat::JsonProto => format!("{}\n", crate::action_proto::container(&owned, &mut printer).to_json(false)).into_bytes(),
    })
}

/// An aspect as `aquery` describes it: its class, then its parameters.
pub fn aspect_description(aspect: &AspectDescriptor) -> String {
    let parameters: Vec<String> = aspect_parameters(aspect).iter().map(|(name, value)| format!("{name}='{value}'")).collect();
    format!("{}({})", aspect_name(aspect), parameters.join(", "))
}

/// An aspect's class, as Bazel names it, with its file's canonical label:
/// `//pkg:defs.bzl%name`, or `@@repo//pkg:defs.bzl%name`.
pub fn aspect_name(aspect: &AspectDescriptor) -> String {
    format!("{}%{}", aspect.file, aspect.name)
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

/// The platform an action runs on.
pub fn exec_platform(target: &ConfiguredTarget) -> Label {
    target.exec_platform.unwrap_or(target.config.get().host_platform)
}

/// The actions as `--output=text` shows them.
fn texts(owned: &[Owned], printer: &mut Printer, mapping: &RepoMappingTable) -> String {
    let mut out = String::new();
    for owned in owned {
        text(owned, printer, mapping, &mut out);
    }
    out
}

/// An action as `--output=text` shows it.
fn text(owned: &Owned, printer: &mut Printer, mapping: &RepoMappingTable, out: &mut String) {
    let Owned { target, action, arguments, aspects } = owned;
    let config = target.config.get();
    let platform = exec_platform(target);
    let inputs = ShownInputs::of(action);
    writeln!(out, "{}", header(owned)).ok();
    writeln!(out, "  Mnemonic: {}", action.mnemonic).ok();
    writeln!(out, "  Target: {}", display_label(target.label, mapping)).ok();
    writeln!(out, "  Configuration: {}", config.output_directory).ok();
    writeln!(out, "  Execution platform: {}", display_label(platform, mapping)).ok();
    if !aspects.is_empty() {
        let described: Vec<String> = aspects.iter().rev().map(aspect_description).collect();
        writeln!(out, "  AspectDescriptors: [{}]", described.join("\n    -> ")).ok();
    }
    writeln!(out, "  ActionKey: {}", printer.action_key(action, &inputs)).ok();
    if printer.output.include_artifacts {
        let inputs: Vec<String> = inputs.sorted(&mut printer.names).iter().map(|path| escaped_unicode(path)).collect();
        writeln!(out, "  Inputs: [{}]", inputs.join(", ")).ok();
        writeln!(out, "  Outputs: [{}]", outputs(action).join(", ")).ok();
    }
    let environment = text_environment(owned);
    if !environment.is_empty() {
        writeln!(out, "  Environment: [{}]", environment.join(", ")).ok();
    }
    if let ActionKind::Spawn(spawn) = &action.kind {
        if printer.output.include_commandline {
            writeln!(out, "  Command Line: {}", command_line(arguments, target.config, platform)).ok();
        }
        if !spawn.execution_info.is_empty() {
            let info: Vec<String> = spawn.execution_info.iter().map(|(key, value)| format!("{}: {}", shell_escape(key), shell_escape(value))).collect();
            writeln!(out, "  ExecutionInfo: {{{}}}", info.join(", ")).ok();
        }
    }
    kind_text(owned, printer, out);
    out.push('\n');
}

/// The line that introduces an action: what Bazel calls it.
fn header(owned: &Owned) -> String {
    let Owned { target, action, .. } = owned;
    if let ActionKind::RunfilesTree = action.kind {
        return action.progress_message.clone();
    }
    let owner = bazel_analysis::ConfiguredTargetKey { label: target.label, config: target.config };
    format!("action '{}'", action.description(&owner))
}

/// An action's outputs by path, as `--output=text` lists them, a
/// directory marked as one.
fn outputs(action: &Action) -> Vec<String> {
    let mut paths: Vec<String> = action
        .outputs
        .iter()
        .map(|output| match output.is_directory {
            true => escaped_unicode(&format!("{} (TreeArtifact)", output.exec_path)),
            false => escaped_unicode(&output.exec_path.to_string()),
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// The environment `--output=text` lists for an action, as `NAME=value`.
fn text_environment(owned: &Owned) -> Vec<String> {
    let config = owned.target.config.get();
    let environment = match &owned.action.kind {
        ActionKind::Spawn(_) if owned.action.mnemonic.as_str() == CPP_COMPILE => &config.options.action_env,
        ActionKind::Spawn(spawn) => &spawn.env,
        ActionKind::SymlinkTree { env } => env,
        _ => return Vec::new(),
    };
    let mut entries: Vec<String> = environment.iter().map(|(name, value)| escaped_unicode(&format!("{name}={value}"))).collect();
    entries.sort();
    entries
}

/// What `--output=text` shows of a template expansion's template, or of
/// what a file write writes.
fn kind_text(owned: &Owned, printer: &Printer, out: &mut String) {
    match &owned.action.kind {
        ActionKind::Template { template, substitutions, .. } if owned.action.mnemonic.as_str() == TEMPLATE_EXPAND => {
            writeln!(out, "  Template: {}", printer.template_content(template)).ok();
            out.push_str("  Substitutions: [\n");
            for (key, value) in substitutions {
                writeln!(out, "    {{{key}: {value}}}").ok();
            }
            out.push_str("  ]\n");
        }
        ActionKind::Write { content, executable, from_args: false, .. } => {
            writeln!(out, "  IsExecutable: {executable}").ok();
            if printer.output.include_file_write_contents {
                writeln!(out, "  FileWriteContents: [{}]", data_encoding::BASE64.encode(content.as_bytes())).ok();
            }
        }
        ActionKind::RepoMappingManifest { .. } => {
            writeln!(out, "  IsExecutable: false").ok();
        }
        _ => {}
    }
}

/// A command line as a script that runs it, as Bazel describes commands,
/// with the configuration and platform it runs in.
fn command_line(arguments: &[String], config: ConfigId, platform: Label) -> String {
    let escaped: Vec<String> = arguments.iter().map(|argument| shell_escape(&escaped_unicode(argument))).collect();
    format!("(exec {})\n# Configuration: {}\n# Execution platform: {platform}", escaped.join(" \\\n    "), config.get().digest)
}

/// An action's command line on one line, as `--output=commands` shows it;
/// nothing for actions that don't run a command.
fn command(owned: &Owned) -> Option<String> {
    let ActionKind::Spawn(_) = &owned.action.kind else {
        return None;
    };
    let escaped: Vec<String> = owned.arguments.iter().map(|argument| shell_escape(&escaped_unicode(argument))).collect();
    Some(format!("{}\n", escaped.join(" ")))
}

/// Text as `aquery` prints it, as Bazel does: characters other than
/// printable ASCII escaped, as `\uXXXX`, or `\UXXXXXXXX` beyond the
/// Basic Multilingual Plane.
fn escaped_unicode(text: &str) -> String {
    if text.bytes().all(|byte| (0x20..0x7f).contains(&byte)) {
        return text.to_string();
    }
    let mut escaped = String::with_capacity(text.len() * 2);
    for character in text.chars() {
        let code = u32::from(character);
        match code {
            0x20..0x7f => escaped.push(character),
            0..=0xffff => escaped.push_str(&format!("\\u{code:04X}")),
            _ => escaped.push_str(&format!("\\U{code:08X}")),
        }
    }
    escaped
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
            *aspects.entry(aspect_name(aspect)).or_default() += 1;
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
    ids: FxHashMap<K, u32>,
}

impl<K: Hash + Eq + Clone> Ids<K> {
    pub fn new() -> Ids<K> {
        Ids { ids: FxHashMap::default() }
    }

    /// The id of `key`, and whether it's new.
    pub fn id(&mut self, key: &K) -> (u32, bool) {
        if let Some(id) = self.ids.get(key) {
            return (*id, false);
        }
        let id = self.ids.len() as u32 + 1;
        self.ids.insert(key.clone(), id);
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
    fn escapes_what_isnt_printable_ascii_as_bazel_does() {
        assert_eq!(escaped_unicode("plain text"), "plain text");
        assert_eq!(escaped_unicode("a\nb\u{e9}\u{1F600}"), "a\\u000Ab\\u00E9\\U0001F600");
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
