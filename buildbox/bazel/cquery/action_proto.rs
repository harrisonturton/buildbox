//! Actions as `analysis_v2.proto`'s `ActionGraphContainer`: each target,
//! configuration, file and path written once, by id, the first time an
//! action refers to it, as Bazel writes them.

use crate::action_output::{AqueryOutput, Ids, action_key, exec_platform};
use bazel_analysis::{Action, ActionKind, Artifact, ConfiguredTarget};
use bazel_config::ConfigId;
use bazel_label::{Label, RepoMappingTable};
use bazel_query::display_label;
use bazel_query::proto::{self, Message};
use intern::PathId;

/// A container's entries, in the order they were made: a field of the
/// container, and its value.
struct Entries {
    entries: Vec<(u32, &'static str, Message)>,
    rule_classes: Ids<String>,
    targets: Ids<Label>,
    configs: Ids<ConfigId>,
    paths: Ids<PathId>,
    artifacts: Ids<PathId>,
    dep_sets: u32,
}

impl Entries {
    fn new() -> Entries {
        Entries {
            entries: Vec::new(),
            rule_classes: Ids::new(),
            targets: Ids::new(),
            configs: Ids::new(),
            paths: Ids::new(),
            artifacts: Ids::new(),
            dep_sets: 0,
        }
    }

    fn push(&mut self, number: u32, name: &'static str, message: Message) {
        self.entries.push((number, name, message));
    }

    fn target(&mut self, target: &ConfiguredTarget, mapping: &RepoMappingTable) -> u32 {
        let (rule_class, new_class) = self.rule_classes.id(&target.kind.to_string());
        if new_class {
            let mut message = Message::new();
            message.int3(1, "id", rule_class.into()).string3(2, "name", target.kind.to_string());
            self.push(7, "rule_classes", message);
        }
        let (id, new) = self.targets.id(&target.label);
        if new {
            let mut message = Message::new();
            message.int3(1, "id", id.into()).string3(2, "label", display_label(target.label, mapping)).int3(3, "rule_class_id", rule_class.into());
            self.push(3, "targets", message);
        }
        id
    }

    fn configuration(&mut self, config: ConfigId) -> u32 {
        let (id, new) = self.configs.id(&config);
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

    fn artifact(&mut self, artifact: &Artifact) -> u32 {
        let (id, new) = self.artifacts.id(&artifact.exec_path);
        if !new {
            return id;
        }
        let path = self.path(artifact.exec_path);
        let mut message = Message::new();
        message.int3(1, "id", id.into()).int3(2, "path_fragment_id", path.into());
        self.push(1, "artifacts", message);
        id
    }

    fn dep_set(&mut self, artifacts: &[Artifact]) -> u32 {
        let direct: Vec<i64> = artifacts.iter().map(|artifact| i64::from(self.artifact(artifact))).collect();
        self.dep_sets += 1;
        let mut message = Message::new();
        message.int3(1, "id", self.dep_sets.into()).packed(3, "direct_artifact_ids", direct);
        self.push(4, "dep_set_of_files", message);
        self.dep_sets
    }

    fn action(&mut self, target: &ConfiguredTarget, action: &Action, output: &AqueryOutput, mapping: &RepoMappingTable) {
        let target_id = self.target(target, mapping);
        let config = self.configuration(target.config);
        let mut message = Message::new();
        message.int3(1, "target_id", target_id.into()).string3(3, "action_key", action_key(action)).string3(4, "mnemonic", action.mnemonic.to_string());
        message.int3(5, "configuration_id", config.into());
        if output.include_artifacts {
            let inputs = action.all_inputs();
            if !inputs.is_empty() {
                let dep_set = self.dep_set(&inputs);
                message.packed(8, "input_dep_set_ids", vec![dep_set.into()]);
            }
            let outputs: Vec<i64> = action.outputs.iter().map(|artifact| i64::from(self.artifact(artifact))).collect();
            let primary = outputs.first().copied().unwrap_or(0);
            message.packed(9, "output_ids", outputs).int3(13, "primary_output_id", primary);
        }
        kind_fields(&mut message, action, output);
        message.string3(14, "execution_platform", exec_platform(target).to_string());
        self.push(2, "actions", message);
    }
}

/// The fields of an action that depend on what it does.
fn kind_fields(message: &mut Message, action: &Action, output: &AqueryOutput) {
    match &action.kind {
        ActionKind::Spawn(spawn) => {
            if output.include_commandline {
                message.strings(6, "arguments", spawn.arguments.iter().cloned());
            }
            for (key, value) in &spawn.env {
                message.repeated(7, "environment_variables", pair(key, value));
            }
            for (key, value) in &spawn.execution_info {
                message.repeated(11, "execution_info", pair(key, value));
            }
        }
        ActionKind::Template { substitutions, executable, .. } => {
            for (key, value) in substitutions {
                message.repeated(16, "substitutions", pair(key, value));
            }
            message.bool3(19, "is_executable", *executable);
        }
        ActionKind::Write { executable, .. } => {
            message.bool3(19, "is_executable", *executable);
        }
        ActionKind::Symlink { .. } => {}
    }
}

fn pair(key: &str, value: &str) -> Message {
    let mut message = Message::new();
    message.string3(1, "key", key.to_string()).string3(2, "value", value.to_string());
    message
}

fn entries(actions: &[(&ConfiguredTarget, &Action)], output: &AqueryOutput, mapping: &RepoMappingTable) -> Entries {
    let mut entries = Entries::new();
    for (target, action) in actions {
        entries.action(target, action, output, mapping);
    }
    entries
}

/// The actions as one `ActionGraphContainer`.
pub fn container(actions: &[(&ConfiguredTarget, &Action)], output: &AqueryOutput, mapping: &RepoMappingTable) -> Message {
    let mut container = Message::new();
    for (number, name, message) in entries(actions, output, mapping).entries {
        container.repeated(number, name, message);
    }
    container
}

/// The actions as `ActionGraphContainer`s of one entry each, in the order
/// they were made, each prefixed by its length.
pub fn streamed(actions: &[(&ConfiguredTarget, &Action)], output: &AqueryOutput, mapping: &RepoMappingTable) -> Vec<u8> {
    let mut out = Vec::new();
    for (number, name, message) in entries(actions, output, mapping).entries {
        let mut container = Message::new();
        container.repeated(number, name, message);
        out.extend(proto::delimited(&container));
    }
    out
}

/// The actions as text format, each entry as it was made, as Bazel
/// streams them.
pub fn text(actions: &[(&ConfiguredTarget, &Action)], output: &AqueryOutput, mapping: &RepoMappingTable) -> String {
    let mut out = String::new();
    for (number, name, message) in entries(actions, output, mapping).entries {
        let mut container = Message::new();
        container.repeated(number, name, message);
        out.push_str(&container.to_flat_text());
    }
    out
}
