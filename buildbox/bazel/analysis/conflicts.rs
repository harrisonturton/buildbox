//! Finding actions that can't both run in one build, as Bazel's
//! `ArtifactConflictFinder` does: different actions of different targets
//! writing the same file, and outputs whose paths are prefixes of others'.

use crate::action::{Action, ActionKind};
use crate::configured::{ConfiguredTarget, ConfiguredTargetKey};
use bazel_label::Label;
use intern::{PathId, Symbol};
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// The conflicts among the actions of the targets that the targets of a
/// build depend on, themselves included, with the applications of aspects
/// to them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct ActionConflictsKey(pub Vec<ConfiguredTargetKey>);

/// Each conflict, as Bazel reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct ActionConflicts(pub Vec<String>);

encoded_value!(ActionConflicts);

impl Key for ActionConflictsKey {
    type Value = ActionConflicts;
    const KIND: &'static str = "ACTION_CONFLICTS";

    async fn compute(&self, ctx: &Ctx) -> Result<ActionConflicts, SkyError> {
        let targets = closure(ctx, &self.0).await;
        let mut outputs: BTreeMap<PathId, Vec<(&ConfiguredTarget, u32)>> = BTreeMap::new();
        for target in &targets {
            for (path, action) in &target.generating {
                outputs.entry(*path).or_default().push((target, *action));
            }
        }
        let mut conflicts = Vec::new();
        for (path, generators) in &outputs {
            if let Some(conflict) = same_file_conflict(*path, generators)? {
                conflicts.push(conflict);
            }
        }
        conflicts.extend(prefix_conflicts(&outputs));
        Ok(ActionConflicts(conflicts))
    }
}

/// The analyzed targets `tops` depend on, themselves included, and the
/// applications of aspects to them. Targets whose analysis failed are left
/// out: their errors are reported already.
async fn closure(ctx: &Ctx, tops: &[ConfiguredTargetKey]) -> Vec<Arc<ConfiguredTarget>> {
    let mut seen: HashSet<ConfiguredTargetKey> = tops.iter().copied().collect();
    let mut frontier: Vec<ConfiguredTargetKey> = tops.to_vec();
    let mut targets = Vec::new();
    while !frontier.is_empty() {
        let computed = ctx.compute_many(&frontier).await;
        frontier.clear();
        for target in computed.into_iter().filter_map(Result::ok) {
            let deps = target.deps.iter().filter_map(|dep| Some(ConfiguredTargetKey { label: dep.label, config: dep.config? }));
            for next in deps.chain(target.aspects.iter().copied()) {
                if seen.insert(next) {
                    frontier.push(next);
                }
            }
            targets.push(target);
        }
    }
    targets
}

/// The conflict among the actions that write `path`, if more than one
/// target's does and they aren't the same action, which Bazel shares.
fn same_file_conflict(path: PathId, generators: &[(&ConfiguredTarget, u32)]) -> Result<Option<String>, SkyError> {
    let Some(((first, first_index), rest)) = generators.split_first() else {
        return Ok(None);
    };
    if rest.is_empty() {
        return Ok(None);
    }
    let first_action = load(first, *first_index)?;
    for (other, other_index) in rest {
        let other_action = load(other, *other_index)?;
        if !shareable(&first_action, &other_action) {
            return Ok(Some(conflict_message(path, (first, &first_action), (other, &other_action))));
        }
    }
    Ok(None)
}

fn load(target: &ConfiguredTarget, index: u32) -> Result<Action, SkyError> {
    let stored = target
        .actions
        .get(index as usize)
        .ok_or_else(|| SkyError::new(format!("{} has no action {index}", target.label)))?;
    stored.load()
}

/// Whether two targets' actions are the same action, which Bazel lets
/// both register: the same command, inputs and outputs, whoever owns them.
fn shareable(a: &Action, b: &Action) -> bool {
    a.mnemonic == b.mnemonic && paths(&a.outputs) == paths(&b.outputs) && input_paths(a) == input_paths(b) && without_owners(&a.kind) == without_owners(&b.kind)
}

fn paths(artifacts: &[crate::Artifact]) -> Vec<PathId> {
    artifacts.iter().map(|artifact| artifact.exec_path).collect()
}

fn input_paths(action: &Action) -> Vec<PathId> {
    let mut inputs = Vec::new();
    action.inputs.for_each(|artifact| inputs.push(artifact.exec_path));
    inputs.sort();
    inputs.dedup();
    let trees = action.runfiles.iter().map(|tree| tree.dir);
    inputs.extend(trees);
    inputs
}

/// `kind`, with the owners of the files it names left out.
fn without_owners(kind: &ActionKind) -> ActionKind {
    let unowned = |artifact: &crate::Artifact| crate::Artifact {
        exec_path: artifact.exec_path,
        owner: Label::parse_canonical("@@//:_").expect("a valid label"),
        config: None,
        is_directory: artifact.is_directory,
    };
    match kind {
        ActionKind::Template { template, substitutions, executable } => ActionKind::Template {
            template: unowned(template),
            substitutions: substitutions.clone(),
            executable: *executable,
        },
        ActionKind::Symlink { target, executable } => ActionKind::Symlink {
            target: unowned(target),
            executable: *executable,
        },
        ActionKind::RepoMappingManifest { files } => ActionKind::RepoMappingManifest { files: files.iter().map(unowned).collect() },
        other => other.clone(),
    }
}

/// Bazel's report of two actions writing one file, with the details that
/// tell them apart.
fn conflict_message(path: PathId, (a, a_action): (&ConfiguredTarget, &Action), (b, b_action): (&ConfiguredTarget, &Action)) -> String {
    let shown = a_action.outputs.iter().find(|output| output.exec_path == path).map(|output| output.short_path()).unwrap_or_else(|| path.to_string());
    let mut message = format!("file '{shown}' is generated by these conflicting actions:\n");
    let mut detail = |key: &str, a_value: String, b_value: String| {
        message.push_str(&format!("{key}: {a_value}"));
        if a_value != b_value {
            message.push_str(&format!(", {b_value}"));
        }
        message.push('\n');
    };
    detail("Label", a.label.to_string(), b.label.to_string());
    detail("RuleClass", kind_name(a.kind), kind_name(b.kind));
    detail("Configuration", a.config.get().digest.clone(), b.config.get().digest.clone());
    detail("Mnemonic", a_action.mnemonic.to_string(), b_action.mnemonic.to_string());
    detail("Progress message", a_action.description(&key(a)), b_action.description(&key(b)));
    message.push_str(&outputs_detail(a_action, b_action));
    message
}

/// How the two actions' outputs differ, as Bazel words it.
fn outputs_detail(a: &Action, b: &Action) -> String {
    let a_paths = paths(&a.outputs);
    let b_paths = paths(&b.outputs);
    if a_paths == b_paths {
        return "Outputs: are equal".to_string();
    }
    let only = |these: &[PathId], those: &[PathId]| these.iter().filter(|path| !those.contains(path)).take(5).map(|path| format!("\t{path}\n")).collect::<String>();
    let mut detail = "Outputs: ".to_string();
    let a_only = only(&a_paths, &b_paths);
    if !a_only.is_empty() {
        detail.push_str(&format!("Attempted action contains artifacts not in previous action (first 5): \n{a_only}"));
    }
    let b_only = only(&b_paths, &a_paths);
    if !b_only.is_empty() {
        detail.push_str(&format!("Previous action contains artifacts not in attempted action (first 5): \n{b_only}"));
    }
    detail.trim_end().to_string()
}

fn key(target: &ConfiguredTarget) -> ConfiguredTargetKey {
    ConfiguredTargetKey {
        label: target.label,
        config: target.config,
    }
}

fn kind_name(kind: Symbol) -> String {
    format!("{kind} rule")
}

/// Bazel's reports of outputs inside other outputs: a file can't be a
/// directory too.
fn prefix_conflicts(outputs: &BTreeMap<PathId, Vec<(&ConfiguredTarget, u32)>>) -> Vec<String> {
    let owners: HashMap<String, Label> = outputs.iter().filter_map(|(path, generators)| Some((path.to_string(), generators.first()?.0.label))).collect();
    let mut conflicts = Vec::new();
    for (path, generators) in outputs {
        let Some((target, _)) = generators.first() else {
            continue;
        };
        let path = path.to_string();
        let mut parent = path.as_str();
        while let Some((dir, _)) = parent.rsplit_once('/') {
            if let Some(owner) = owners.get(dir) {
                conflicts.push(format!(
                    "One of the output paths '{dir}' (belonging to {owner}) and '{path}' (belonging to {}) is a prefix of the other. These actions cannot be simultaneously present; please rename one of the output files or build just one of them",
                    target.label
                ));
                break;
            }
            parent = dir;
        }
    }
    conflicts
}
