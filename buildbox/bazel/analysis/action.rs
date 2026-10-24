//! Actions: what analysis says to run to generate files.

use crate::artifact::Artifact;
use crate::inputs::InputSet;
use intern::{PathId, Symbol};
use skycodec::{Decode, Encode};
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
        let runfiles = self.runfiles.iter().flat_map(|tree| tree.entries.iter().map(|(_, artifact)| *artifact));
        let template = match &self.kind {
            ActionKind::Template { template, .. } => Some(*template),
            ActionKind::Symlink { target, .. } => Some(*target),
            _ => None,
        };
        let mut all = self.inputs.to_vec();
        all.extend(runfiles.chain(template));
        all.sort();
        all.dedup();
        all
    }
}
