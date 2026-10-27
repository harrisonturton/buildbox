//! Action keys as `aquery` shows them: a digest of what an action does,
//! which identifies it. They're bb's own, not Bazel's.

use crate::action_inputs::{PathNames, ShownInputs};
use bazel_analysis::{Action, InputSet};
use ring::digest::{Context, SHA256};
use rustc_hash::FxHashMap;

type Digest = [u8; 32];

/// Computes action keys, digesting each input set once however many
/// actions share it.
#[derive(Default)]
pub(crate) struct ActionKeys {
    sets: FxHashMap<usize, Digest>,
}

impl ActionKeys {
    /// The key of `action`, which reads `inputs`, in hex.
    pub fn key(&mut self, action: &Action, inputs: &ShownInputs, names: &mut PathNames) -> String {
        let mut context = Context::new(&SHA256);
        context.update(action.mnemonic.as_str().as_bytes());
        context.update(&self.set_digest(&inputs.set, names));
        for path in inputs.extra.iter().chain(&inputs.hidden) {
            context.update(names.name(*path).as_bytes());
            context.update(b"\0");
        }
        for output in &action.outputs {
            context.update(names.name(output.exec_path).as_bytes());
            context.update(b"\0");
        }
        context.update(format!("{:?}", action.kind).as_bytes());
        context.finish().as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// The digest of a set: of its own members, then of the sets it
    /// includes. Sets are visited without recursion, so deep ones can't
    /// exhaust the stack.
    fn set_digest(&mut self, root: &InputSet, names: &mut PathNames) -> Digest {
        let Some(root_identity) = root.identity() else {
            return [0; 32];
        };
        let mut pending = vec![(root.clone(), false)];
        while let Some((set, children_done)) = pending.pop() {
            let Some(identity) = set.identity() else {
                continue;
            };
            if self.sets.contains_key(&identity) {
                continue;
            }
            if !children_done {
                pending.push((set.clone(), true));
                pending.extend(set.children().filter(|child| child.identity().is_some_and(|child| !self.sets.contains_key(&child))).map(|child| (child, false)));
                continue;
            }
            let digest = self.digest_of(&set, names);
            self.sets.insert(identity, digest);
        }
        self.sets.get(&root_identity).copied().unwrap_or([0; 32])
    }

    /// The digest of a set whose included sets' digests are known.
    fn digest_of(&self, set: &InputSet, names: &mut PathNames) -> Digest {
        let mut context = Context::new(&SHA256);
        for artifact in set.direct() {
            context.update(names.name(artifact.exec_path).as_bytes());
            context.update(b"\0");
        }
        for child in set.children() {
            let digest = child.identity().and_then(|identity| self.sets.get(&identity));
            context.update(digest.map(|digest| digest.as_slice()).unwrap_or(&[]));
        }
        let mut digest = [0; 32];
        digest.copy_from_slice(context.finish().as_ref());
        digest
    }
}
