//! The files `aquery` shows an action reading, as Bazel's action graph has
//! them: the action's input set, with each runfiles tree it reads as one
//! file, the tree's directory, and without the params files a command
//! writes as it runs.

use bazel_analysis::{Action, ActionKind, InputSet};
use intern::PathId;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

/// What `aquery` shows an action reading.
pub(crate) struct ShownInputs {
    /// The action's input set.
    pub set: InputSet,
    /// What it reads besides the members of its set: its runfiles trees'
    /// directories, and a template's or a symlink's target.
    pub extra: Vec<PathId>,
    /// The members of its set it isn't shown reading.
    pub hidden: Vec<PathId>,
}

impl ShownInputs {
    pub fn of(action: &Action) -> ShownInputs {
        let mut extra: Vec<PathId> = action.runfiles.iter().map(|tree| tree.dir).collect();
        match &action.kind {
            ActionKind::Template { template, .. } => extra.push(template.exec_path),
            ActionKind::Symlink { target, .. } => extra.push(target.exec_path),
            _ => {}
        }
        let hidden = match &action.kind {
            ActionKind::Spawn(spawn) => spawn.param_files.iter().map(|param_file| param_file.path).collect(),
            _ => Vec::new(),
        };
        ShownInputs { set: action.inputs.clone(), extra, hidden }
    }

    /// Whether the shown inputs differ from the action's input set, and
    /// so need a set of their own.
    pub fn differ_from_set(&self) -> bool {
        !self.extra.is_empty() || !self.hidden.is_empty()
    }

    /// Call `visit` with each input, maybe more than once.
    pub fn for_each(&self, mut visit: impl FnMut(PathId)) {
        self.set.for_each(|artifact| {
            if !self.hidden.contains(&artifact.exec_path) {
                visit(artifact.exec_path);
            }
        });
        self.extra.iter().copied().for_each(visit);
    }

    /// The inputs once each, as `--output=text` lists them: by path.
    pub fn sorted(&self, names: &mut PathNames) -> Vec<Arc<str>> {
        let mut paths: FxHashSet<PathId> = FxHashSet::default();
        self.for_each(|path| {
            paths.insert(path);
        });
        let mut sorted: Vec<Arc<str>> = paths.into_iter().map(|path| names.name(path)).collect();
        sorted.sort_unstable();
        sorted
    }
}

/// Paths as text, each made once however many actions share it.
#[derive(Default)]
pub(crate) struct PathNames {
    names: FxHashMap<PathId, Arc<str>>,
}

impl PathNames {
    pub fn name(&mut self, path: PathId) -> Arc<str> {
        self.names.entry(path).or_insert_with(|| path.to_string().into()).clone()
    }
}
