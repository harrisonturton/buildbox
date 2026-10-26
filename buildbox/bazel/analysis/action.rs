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
    /// Write a file with fixed contents.
    Write { content: String, executable: bool },
    /// Write a file by substituting strings in a template.
    Template {
        template: Artifact,
        substitutions: Vec<(String, String)>,
        executable: bool,
    },
    /// Make an output that is a copy of another file.
    Symlink { target: Artifact, executable: bool },
}

/// A command to run.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct Spawn {
    pub arguments: Vec<String>,
    /// Environment variables, sorted by name.
    pub env: Vec<(String, String)>,
    /// Hints for how to run it, such as `no-sandbox` or `requires-network`,
    /// sorted by name.
    pub execution_info: Vec<(String, String)>,
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
}

impl Action {
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
