//! Actions: what analysis says to run to generate files.

use crate::artifact::Artifact;
use crate::inputs::InputSet;
use intern::{PathId, Symbol};
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Part, SkyError, Stored};
use std::sync::Arc;

/// An action a rule registers: its inputs and outputs, and how it turns one
/// into the other.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Action {
    /// A short name for the kind of action, such as `Genrule`.
    pub mnemonic: Symbol,
    pub progress_message: String,
    pub inputs: InputSet,
    /// The runfiles trees of tools the action runs, which are inputs too.
    pub runfiles: Vec<Arc<RunfilesTree>>,
    pub outputs: Vec<Artifact>,
    pub kind: ActionKind,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ActionKind {
    /// Run a command.
    Spawn(Spawn),
    /// Write a file with fixed contents. Contents made from an `Args`
    /// are a params file's, which Bazel's aquery doesn't show.
    Write { content: String, executable: bool, from_args: bool },
    /// Write a file by substituting strings in a template.
    Template {
        template: Artifact,
        substitutions: Vec<(String, String)>,
        executable: bool,
    },
    /// Make an output that is a copy of another file.
    Symlink { target: Artifact, executable: bool },
    /// Lay out a runfiles tree from its manifest, as Bazel's `SymlinkTree`
    /// actions do, in an environment of `env`. bb lays out runfiles trees
    /// as it runs their executables, so only `aquery` shows these.
    SymlinkTree { env: Vec<(String, String)> },
    /// Stand for a runfiles tree's files, which actions running its
    /// executable read, as Bazel's `RunfilesTree` actions do. Only
    /// `aquery` shows these.
    RunfilesTree,
    /// Write the repository mapping manifest of a runfiles tree of
    /// `files`, which needs the repositories' mappings.
    RepoMappingManifest { files: Vec<Artifact> },
}

/// A command to run.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Spawn {
    pub arguments: Vec<String>,
    /// Environment variables, in the order Bazel gives them, which
    /// `aquery` shows; commands are given them sorted.
    pub env: Vec<(String, String)>,
    /// Hints for how to run it, such as `no-sandbox` or `requires-network`,
    /// sorted by name.
    pub execution_info: Vec<(String, String)>,
    /// The arguments that name params files holding more arguments, which
    /// another action writes.
    pub param_files: Vec<ParamFile>,
}

/// An environment of `variables`, in order, as Bazel makes them: one set
/// more than once keeps its first place and takes its last value.
pub fn environment(variables: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    for (name, value) in variables {
        match env.iter_mut().find(|(existing, _)| *existing == name) {
            Some(entry) => entry.1 = value,
            None => env.push((name, value)),
        }
    }
    env
}

/// An argument of a command that names a params file: which argument, the
/// file, and how its arguments are written in it.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ParamFile {
    pub argument: u32,
    pub path: PathId,
    /// `multiline`, `shell` or `gcc_quoted`.
    pub format: String,
}

/// A runfiles tree: the files an executable needs at run time, laid out in
/// the directory next to it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RunfilesTree {
    /// The tree's directory, relative to the execution root, such as
    /// `bazel-out/k8-opt-exec-ST-1234/bin/pkg/tool.runfiles`.
    pub dir: PathId,
    /// Each file's path within the tree, sorted.
    pub entries: Vec<(PathId, Artifact)>,
    /// The paths of the empty files in the tree, sorted.
    pub empty: Vec<PathId>,
}

/// The mnemonics of actions Bazel gives no progress message, which it
/// describes by their output alone, even when they build tools.
const UNREPORTED: &[&str] = &["SolibSymlink"];

impl Action {
    /// What progress output and `aquery` call the action, as Bazel does:
    /// its progress message, or its kind's default for its first output,
    /// marked if it builds a tool.
    pub fn description(&self, owner: &crate::ConfiguredTargetKey) -> String {
        let output = self.outputs.first().map(|output| output.root_relative().to_string());
        if self.progress_message.is_empty() && UNREPORTED.contains(&self.mnemonic.as_str()) {
            return format!("{} {}", self.mnemonic, output.unwrap_or_default());
        }
        let message = match (self.progress_message.is_empty(), output) {
            (false, _) => self.progress_message.clone(),
            (true, Some(output)) => match &self.kind {
                ActionKind::Write { executable: true, .. } => format!("Writing script {output}"),
                ActionKind::Write { executable: false, .. } => format!("Writing file {output}"),
                ActionKind::Template { .. } => format!("Expanding template {output}"),
                ActionKind::Symlink { .. } | ActionKind::Spawn(_) | ActionKind::SymlinkTree { .. } | ActionKind::RunfilesTree | ActionKind::RepoMappingManifest { .. } => {
                    format!("{} {output}", self.mnemonic)
                }
            },
            (true, None) => format!("{} {}", self.mnemonic, owner.label),
        };
        match owner.config.get().is_exec {
            true => format!("{message} [for tool]"),
            false => message,
        }
    }

    /// Every artifact the action reads, including those of runfiles trees,
    /// once each.
    pub fn all_inputs(&self) -> Vec<Artifact> {
        let mut all = Vec::new();
        self.for_each_input(|artifact| all.push(artifact));
        all.sort();
        all.dedup();
        all
    }

    /// Call `visit` with every artifact [`Action::all_inputs`] lists,
    /// without flattening the action's input set. Some may be visited more
    /// than once.
    pub fn for_each_input(&self, mut visit: impl FnMut(Artifact)) {
        self.inputs.for_each(&mut visit);
        for tree in &self.runfiles {
            tree.entries.iter().for_each(|(_, artifact)| visit(*artifact));
        }
        match &self.kind {
            ActionKind::Template { template, .. } => visit(*template),
            ActionKind::Symlink { target, .. } => visit(*target),
            _ => {}
        }
    }
}

/// What an action is besides its inputs and tools' runfiles: kept in the
/// engine's store, compressed with the other actions'.
#[derive(Encode, Decode)]
struct ActionBody {
    mnemonic: Symbol,
    progress_message: String,
    outputs: Vec<Artifact>,
    kind: ActionKind,
}

impl Part for ActionBody {
    const KIND: &'static str = "ACTION";
}

/// An action as an analyzed target keeps it: its inputs and runfiles
/// trees, which it shares with other actions, and the rest stored
/// compressed, read when the action runs.
#[derive(Clone, Debug)]
pub struct StoredAction {
    inputs: InputSet,
    runfiles: Vec<Arc<RunfilesTree>>,
    body: Stored<ActionBody>,
}

impl StoredAction {
    pub fn new(ctx: &Ctx, action: Action) -> StoredAction {
        let body = ActionBody {
            mnemonic: action.mnemonic,
            progress_message: action.progress_message,
            outputs: action.outputs,
            kind: action.kind,
        };
        StoredAction {
            inputs: action.inputs,
            runfiles: action.runfiles,
            body: ctx.store(&body),
        }
    }

    /// The action, read from the store.
    pub fn load(&self) -> Result<Action, SkyError> {
        let body = self.body.get()?;
        Ok(Action {
            mnemonic: body.mnemonic,
            progress_message: body.progress_message,
            inputs: self.inputs.clone(),
            runfiles: self.runfiles.clone(),
            outputs: body.outputs,
            kind: body.kind,
        })
    }
}
