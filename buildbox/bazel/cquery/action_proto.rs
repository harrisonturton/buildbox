//! Actions as `analysis_v2.proto`'s `ActionGraphContainer`: each target,
//! configuration, file, path and set of inputs written once, by id, the
//! first time an action refers to it, as Bazel writes them. An input set
//! shared by many actions is written once, and its sets before it.

use crate::action_inputs::ShownInputs;
use crate::action_output::{Ids, Owned, Printer, TEMPLATE_EXPAND, aspect_name, aspect_parameters, exec_platform};
use bazel_analysis::{ActionKind, ConfiguredTarget, InputSet};
use bazel_config::ConfigId;
use bazel_label::{Label, RepoMappingTable};
use bazel_query::proto::{self, Message};
use intern::PathId;
use rustc_hash::FxHashMap;

/// A container's entries, in the order they were made: a field of the
/// container, and its value.
struct Entries<'a> {
    printer: &'a mut Printer,
    mapping: &'a RepoMappingTable,
    entries: Vec<(u32, &'static str, Message)>,
    rule_classes: Ids<String>,
    targets: Ids<Label>,
    /// Configurations by checksum: those differing only in what bb keeps
    /// internally are one.
    configs: Ids<String>,
    aspects: Ids<String>,
    paths: Ids<PathId>,
    artifacts: Ids<PathId>,
    /// The ids of the input sets written so far, by identity.
    sets: FxHashMap<usize, u32>,
    set_count: u32,
}

/// A set of inputs being written: its id, its own files, the sets it
/// includes, and the ids of those met so far.
struct PendingSet {
    id: u32,
    files: Vec<PathId>,
    children: Vec<InputSet>,
    child_ids: Vec<i64>,
}

impl<'a> Entries<'a> {
    fn new(printer: &'a mut Printer, mapping: &'a RepoMappingTable) -> Entries<'a> {
        Entries {
            printer,
            mapping,
            entries: Vec::new(),
            rule_classes: Ids::new(),
            targets: Ids::new(),
            configs: Ids::new(),
            aspects: Ids::new(),
            paths: Ids::new(),
            artifacts: Ids::new(),
            sets: FxHashMap::default(),
            set_count: 0,
        }
    }

    fn push(&mut self, number: u32, name: &'static str, message: Message) {
        self.entries.push((number, name, message));
    }

    fn target(&mut self, target: &ConfiguredTarget) -> u32 {
        let (rule_class, new_class) = self.rule_classes.id(&target.kind.to_string());
        if new_class {
            let mut message = Message::new();
            message.int3(1, "id", rule_class.into()).string3(2, "name", target.kind.to_string());
            self.push(7, "rule_classes", message);
        }
        let (id, new) = self.targets.id(&target.label);
        if new {
            let mut message = Message::new();
            message.int3(1, "id", id.into()).string3(2, "label", target.label.to_string()).int3(3, "rule_class_id", rule_class.into());
            self.push(3, "targets", message);
        }
        id
    }

    fn configuration(&mut self, config: ConfigId) -> u32 {
        let (id, new) = self.configs.id(&config.get().digest);
        if new {
            self.push(5, "configuration", crate::output::configuration_message(config, id.into()));
        }
        id
    }

    /// A path's id, its parents written before it.
    fn path(&mut self, path: PathId) -> u32 {
        let (id, new) = self.paths.id(&path);
        if !new {
            return id;
        }
        let parent = path.parent().filter(|parent| !parent.is_root()).map(|parent| self.path(parent));
        let mut message = Message::new();
        message.int3(1, "id", id.into()).string3(2, "label", path.name().to_string()).int3(3, "parent_id", parent.unwrap_or(0).into());
        self.push(8, "path_fragments", message);
        id
    }

    fn artifact(&mut self, path: PathId) -> u32 {
        let (id, new) = self.artifacts.id(&path);
        if !new {
            return id;
        }
        let fragment = self.path(path);
        let mut message = Message::new();
        message.int3(1, "id", id.into()).int3(2, "path_fragment_id", fragment.into());
        self.push(1, "artifacts", message);
        id
    }

    /// Number a new set of `files` and `children`, to write.
    fn begin_set(&mut self, files: Vec<PathId>, children: Vec<InputSet>) -> PendingSet {
        self.set_count += 1;
        PendingSet {
            id: self.set_count,
            files,
            children,
            child_ids: Vec::new(),
        }
    }

    fn begin_input_set(&mut self, set: &InputSet) -> PendingSet {
        let files = set.direct().iter().map(|artifact| artifact.exec_path).collect();
        let pending = self.begin_set(files, set.children().collect());
        if let Some(identity) = set.identity() {
            self.sets.insert(identity, pending.id);
        }
        pending
    }

    /// Write a set and the sets it includes that aren't written yet, as
    /// Bazel does: each numbered when it's first met, and written after
    /// the sets it includes and its own files. Sets are visited without
    /// recursion, so deep ones can't exhaust the stack.
    fn write_set(&mut self, root: PendingSet) -> u32 {
        let root_id = root.id;
        let mut stack = vec![root];
        while let Some(top) = stack.last_mut() {
            if let Some(child) = top.children.get(top.child_ids.len()).cloned() {
                match child.identity().and_then(|identity| self.sets.get(&identity)) {
                    Some(id) => top.child_ids.push((*id).into()),
                    None => {
                        let pending = self.begin_input_set(&child);
                        stack.push(pending);
                    }
                }
                continue;
            }
            let Some(done) = stack.pop() else {
                break;
            };
            self.finish_set(&done);
            if let Some(parent) = stack.last_mut() {
                parent.child_ids.push(done.id.into());
            }
        }
        root_id
    }

    fn finish_set(&mut self, set: &PendingSet) {
        let files: Vec<i64> = set.files.iter().map(|path| i64::from(self.artifact(*path))).collect();
        let mut message = Message::new();
        message.int3(1, "id", set.id.into()).packed(2, "transitive_dep_set_ids", set.child_ids.clone()).packed(3, "direct_artifact_ids", files);
        self.push(4, "dep_set_of_files", message);
    }

    /// The id of the set of inputs an action is shown reading, or `None`
    /// if it reads none. The action's own set is shared with other actions
    /// as it is; one with more files, or fewer, is the action's own, and
    /// includes what it can of the shared set.
    fn inputs(&mut self, inputs: &ShownInputs) -> Option<u32> {
        if !inputs.differ_from_set() {
            let identity = inputs.set.identity()?;
            if let Some(id) = self.sets.get(&identity) {
                return Some(*id);
            }
            let pending = self.begin_input_set(&inputs.set);
            return Some(self.write_set(pending));
        }
        let (mut files, children): (Vec<PathId>, Vec<InputSet>) = match inputs.hidden.is_empty() {
            true => (Vec::new(), inputs.set.identity().map(|_| inputs.set.clone()).into_iter().collect()),
            false => {
                let direct = inputs.set.direct().iter().map(|artifact| artifact.exec_path).filter(|path| !inputs.hidden.contains(path));
                (direct.collect(), inputs.set.children().collect())
            }
        };
        for path in &inputs.extra {
            if !files.contains(path) {
                files.push(*path);
            }
        }
        if files.is_empty() && children.is_empty() {
            return None;
        }
        let pending = self.begin_set(files, children);
        Some(self.write_set(pending))
    }

    fn aspect(&mut self, aspect: &bazel_config::AspectDescriptor) -> u32 {
        let (id, new) = self.aspects.id(&aspect.to_string());
        if new {
            let mut message = Message::new();
            message.int3(1, "id", id.into()).string3(2, "name", aspect_name(aspect, self.mapping));
            for (name, value) in aspect_parameters(aspect) {
                message.repeated(3, "parameters", pair(&name, &value));
            }
            self.push(6, "aspect_descriptors", message);
        }
        id
    }

    /// Write an action, and what it refers to first, in the order Bazel
    /// writes them.
    fn action(&mut self, owned: &Owned) {
        let target_id = self.target(&owned.target);
        let inputs = ShownInputs::of(&owned.action);
        let mut message = Message::new();
        message.int3(1, "target_id", target_id.into());
        message.string3(3, "action_key", self.printer.action_key(&owned.action, &inputs));
        message.string3(4, "mnemonic", owned.action.mnemonic.to_string());
        kind_fields(&mut message, owned, self.printer);
        let config = self.configuration(owned.target.config.get().without_aspects());
        message.int3(5, "configuration_id", config.into());
        message.string3(14, "execution_platform", exec_platform(&owned.target).to_string());
        let aspect_ids: Vec<i64> = owned.aspects.iter().rev().map(|aspect| i64::from(self.aspect(aspect))).collect();
        message.packed(2, "aspect_descriptor_ids", aspect_ids);
        if self.printer.output.include_artifacts {
            self.artifact_fields(&mut message, owned, &inputs);
        }
        template_fields(&mut message, owned, self.printer);
        self.push(2, "actions", message);
    }

    fn artifact_fields(&mut self, message: &mut Message, owned: &Owned, inputs: &ShownInputs) {
        if let Some(set) = self.inputs(inputs) {
            message.packed(8, "input_dep_set_ids", vec![set.into()]);
        }
        let outputs: Vec<i64> = owned.action.outputs.iter().map(|artifact| i64::from(self.artifact(artifact.exec_path))).collect();
        let primary = outputs.first().copied().unwrap_or(0);
        message.packed(9, "output_ids", outputs).int3(13, "primary_output_id", primary);
    }
}

/// The fields of an action that depend on what it does.
fn kind_fields(message: &mut Message, owned: &Owned, printer: &Printer) {
    match &owned.action.kind {
        ActionKind::Spawn(spawn) => {
            message.bool3(10, "discovers_inputs", discovers_inputs(&owned.action));
            for (key, value) in &spawn.env {
                message.repeated(7, "environment_variables", pair(key, value));
            }
            if printer.output.include_commandline {
                message.strings(6, "arguments", owned.arguments.iter().cloned());
            }
            for (key, value) in &spawn.execution_info {
                message.repeated(11, "execution_info", pair(key, value));
            }
        }
        ActionKind::Write { content, executable, from_args: false } => {
            message.bool3(19, "is_executable", *executable);
            if printer.output.include_file_write_contents {
                message.string3(17, "file_contents", content.clone());
            }
        }
        _ => {}
    }
}

/// Whether Bazel finds more of the action's inputs as it runs: those of C++
/// compiles that write dependency files, which name the headers they read.
fn discovers_inputs(action: &bazel_analysis::Action) -> bool {
    action.mnemonic.as_str() == "CppCompile" && action.outputs.iter().any(|output| output.exec_path.name().as_str().ends_with(".d"))
}

/// A template expansion's template and substitutions.
fn template_fields(message: &mut Message, owned: &Owned, printer: &Printer) {
    let ActionKind::Template { template, substitutions, .. } = &owned.action.kind else {
        return;
    };
    if owned.action.mnemonic.as_str() != TEMPLATE_EXPAND {
        return;
    }
    message.string3(15, "template_content", printer.template_content(template));
    for (key, value) in substitutions {
        message.repeated(16, "substitutions", pair(key, value));
    }
}

fn pair(key: &str, value: &str) -> Message {
    let mut message = Message::new();
    message.string3(1, "key", key.to_string()).string3(2, "value", value.to_string());
    message
}

fn entries(actions: &[Owned], printer: &mut Printer, mapping: &RepoMappingTable) -> Vec<(u32, &'static str, Message)> {
    let mut entries = Entries::new(printer, mapping);
    for owned in actions {
        entries.action(owned);
    }
    entries.entries
}

/// The actions as one `ActionGraphContainer`.
pub(crate) fn container(actions: &[Owned], printer: &mut Printer, mapping: &RepoMappingTable) -> Message {
    let mut container = Message::new();
    for (number, name, message) in entries(actions, printer, mapping) {
        container.repeated(number, name, message);
    }
    container
}

/// The actions as `ActionGraphContainer`s of one entry each, in the order
/// they were made, each prefixed by its length.
pub(crate) fn streamed(actions: &[Owned], printer: &mut Printer, mapping: &RepoMappingTable) -> Vec<u8> {
    let mut out = Vec::new();
    for (number, name, message) in entries(actions, printer, mapping) {
        let mut container = Message::new();
        container.repeated(number, name, message);
        out.extend(proto::delimited(&container));
    }
    out
}

/// The actions as text format, each entry as it was made, as Bazel
/// streams them.
pub(crate) fn text(actions: &[Owned], printer: &mut Printer, mapping: &RepoMappingTable) -> String {
    let mut out = String::new();
    for (number, name, message) in entries(actions, printer, mapping) {
        let mut container = Message::new();
        container.repeated(number, name, message);
        out.push_str(&container.to_flat_text());
    }
    out
}
