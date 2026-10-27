//! Sets of action inputs that share their parts, as Bazel's nested sets do.
//!
//! An action's inputs are mostly depsets of its dependencies' files, such as
//! a toolchain's, which many actions include. An [`InputSet`] keeps the
//! shape of the depsets it was made from, and equal parts are one shared
//! node across every action and target, so actions hold the files they
//! share once. Sets are flattened only when an action runs.

use crate::artifact::Artifact;
use crate::file_value::FileValue;
use crate::shared::Sharing;
use bazel_starlark::depset::Depset;
use skycodec::{Decode, DecodeError, Decoder, Encode, Encoder};
use starlark::values::{Value, ValueIdentity};
use rustc_hash::{FxHashMap, FxHashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock};

/// A set of artifacts: some of its own, and other sets.
#[derive(Clone, Default)]
pub struct InputSet(Option<Arc<Node>>);

struct Node {
    direct: Box<[Artifact]>,
    children: Box<[Arc<Node>]>,
}

impl Drop for Node {
    /// Drop the nodes only this one holds without recursion, so dropping a
    /// deep set can't exhaust the stack.
    fn drop(&mut self) {
        let mut pending: Vec<Arc<Node>> = std::mem::take(&mut self.children).into_vec();
        while let Some(child) = pending.pop() {
            if let Ok(mut node) = Arc::try_unwrap(child) {
                pending.extend(std::mem::take(&mut node.children).into_vec());
            }
        }
    }
}

/// Every node alive in some action's inputs.
static NODES: LazyLock<Sharing<Node>> = LazyLock::new(Sharing::default);

impl InputSet {
    /// The set of `direct` and the members of `children`, which keep
    /// their order, each once. As in Bazel, a child of one file is kept as
    /// that file.
    pub fn new(mut direct: Vec<Artifact>, children: Vec<InputSet>) -> InputSet {
        let mut seen: FxHashSet<*const Node> = FxHashSet::default();
        let mut kept: Vec<Arc<Node>> = Vec::with_capacity(children.len());
        for child in children.into_iter().filter_map(|child| child.0) {
            match (&*child.direct, child.children.is_empty()) {
                ([file], true) => direct.push(*file),
                _ => {
                    if seen.insert(Arc::as_ptr(&child)) {
                        kept.push(child);
                    }
                }
            }
        }
        let mut children = kept;
        direct.sort();
        direct.dedup();
        if direct.is_empty() && children.len() <= 1 {
            return InputSet(children.pop());
        }
        let node = Node {
            direct: direct.into_boxed_slice(),
            children: children.into_boxed_slice(),
        };
        InputSet(Some(NODES.share(node)))
    }

    /// The set of `artifacts`.
    pub fn of(artifacts: Vec<Artifact>) -> InputSet {
        InputSet::new(artifacts, Vec::new())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    /// The set's own members, sorted, without those of the sets it
    /// includes.
    pub fn direct(&self) -> &[Artifact] {
        match &self.0 {
            Some(node) => &node.direct,
            None => &[],
        }
    }

    /// The sets it includes, in the order they were given.
    pub fn children(&self) -> impl ExactSizeIterator<Item = InputSet> + '_ {
        let children: &[Arc<Node>] = match &self.0 {
            Some(node) => &node.children,
            None => &[],
        };
        children.iter().map(|child| InputSet(Some(child.clone())))
    }

    /// Whether `test` holds for any member, remembering in `known` whether
    /// it does for each set visited, for later calls to skip. Sets are
    /// visited without recursion, so deep ones can't exhaust the stack.
    pub fn any(&self, known: &mut KnownSets, mut test: impl FnMut(Artifact) -> bool) -> bool {
        let Some(root) = &self.0 else {
            return false;
        };
        let mut pending: Vec<(&Arc<Node>, bool)> = vec![(root, false)];
        while let Some((node, children_done)) = pending.pop() {
            if known.get(node).is_some() {
                continue;
            }
            if !children_done {
                pending.push((node, true));
                pending.extend(node.children.iter().map(|child| (child, false)));
                continue;
            }
            let any_child = node.children.iter().any(|child| known.get(child).unwrap_or(false));
            let matched = any_child || node.direct.iter().any(|artifact| test(*artifact));
            known.insert(node, matched);
        }
        known.get(root).unwrap_or(false)
    }

    /// What identifies the set while it's alive: equal sets have the same
    /// identity. `None` for the empty set.
    pub fn identity(&self) -> Option<usize> {
        self.0.as_ref().map(|node| Arc::as_ptr(node) as usize)
    }

    /// Every member, once each, sorted.
    pub fn to_vec(&self) -> Vec<Artifact> {
        let mut members = Vec::new();
        self.for_each(|artifact| members.push(artifact));
        members.sort();
        members.dedup();
        members
    }

    /// Call `visit` with every member, in no particular order, without
    /// flattening the set. A member of several of its parts is visited
    /// once for each.
    pub fn for_each(&self, mut visit: impl FnMut(Artifact)) {
        let Some(root) = &self.0 else {
            return;
        };
        let mut visited: FxHashSet<*const Node> = FxHashSet::default();
        let mut pending: Vec<&Arc<Node>> = vec![root];
        while let Some(node) = pending.pop() {
            if !visited.insert(Arc::as_ptr(node)) {
                continue;
            }
            node.direct.iter().copied().for_each(&mut visit);
            pending.extend(node.children.iter());
        }
    }
}

impl PartialEq for InputSet {
    /// Equal sets made the same way are the same node.
    fn eq(&self, other: &InputSet) -> bool {
        match (&self.0, &other.0) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        }
    }
}

impl Eq for InputSet {}

impl fmt::Debug for InputSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.to_vec()).finish()
    }
}

impl Hash for Node {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.direct.hash(state);
        for child in &self.children {
            (Arc::as_ptr(child) as usize).hash(state);
        }
    }
}

impl PartialEq for Node {
    fn eq(&self, other: &Node) -> bool {
        self.direct == other.direct && self.children.len() == other.children.len() && self.children.iter().zip(&other.children).all(|(a, b)| Arc::ptr_eq(a, b))
    }
}

impl Eq for Node {}

impl Encode for InputSet {
    fn encode(&self, encoder: &mut Encoder) {
        self.to_vec().encode(encoder);
    }
}

impl Decode for InputSet {
    fn decode(decoder: &mut Decoder<'_>) -> Result<InputSet, DecodeError> {
        Ok(InputSet::of(Vec::decode(decoder)?))
    }
}

/// What [`InputSet::any`] found of sets, kept alive so that no other set
/// takes their place.
#[derive(Default)]
pub struct KnownSets {
    sets: FxHashMap<usize, (Arc<Node>, bool)>,
}

impl KnownSets {
    fn get(&self, node: &Arc<Node>) -> Option<bool> {
        self.sets.get(&(Arc::as_ptr(node) as usize)).map(|(_, known)| *known)
    }

    fn insert(&mut self, node: &Arc<Node>, known: bool) {
        self.sets.insert(Arc::as_ptr(node) as usize, (node.clone(), known));
    }
}

/// Converts the depsets of one target's analysis, converting each frozen
/// depset once however many actions include it. Frozen values don't move,
/// and outlive the analysis, so their identities stay theirs.
#[derive(Default)]
pub struct InputSets {
    frozen: FxHashMap<ValueIdentity<'static>, InputSet>,
}

impl InputSets {
    /// The files of a `File`, a list of them, or a depset.
    pub fn convert(&mut self, value: Value<'_>, what: &str) -> anyhow::Result<InputSet> {
        if value.is_none() {
            return Ok(InputSet::default());
        }
        if let Some(artifact) = FileValue::artifact(value) {
            return Ok(InputSet::of(vec![artifact]));
        }
        if Depset::from_value(value).is_some() {
            return self.depset(value, what);
        }
        let items = bazel_starlark::convert::items(value).ok_or_else(|| anyhow::anyhow!("{what} must be a list or depset of files, not {}", value.get_type()))?;
        Ok(InputSet::of(artifacts(&items, what)?))
    }

    /// A depset's input set, the depsets it includes converted first.
    /// Depsets are visited without recursion, so deep ones can't exhaust
    /// the stack, and each is converted once.
    fn depset<'v>(&mut self, root: Value<'v>, what: &str) -> anyhow::Result<InputSet> {
        let mut unfrozen: FxHashMap<ValueIdentity<'v>, InputSet> = FxHashMap::default();
        let mut pending = vec![(root, false)];
        while let Some((value, children_done)) = pending.pop() {
            if self.converted(value, &unfrozen).is_some() {
                continue;
            }
            let Some(depset) = Depset::from_value(value) else {
                anyhow::bail!("{what} must contain files, not {}", value.get_type());
            };
            if !children_done {
                pending.push((value, true));
                let unconverted = depset.transitive.iter().filter(|child| self.converted(**child, &unfrozen).is_none());
                pending.extend(unconverted.map(|child| (*child, false)));
                continue;
            }
            let direct = artifacts(&depset.direct, what)?;
            let mut children = Vec::with_capacity(depset.transitive.len());
            for child in &depset.transitive {
                let converted = self.converted(*child, &unfrozen);
                children.push(converted.ok_or_else(|| anyhow::anyhow!("{what}: a depset was included before it was converted"))?);
            }
            let converted = InputSet::new(direct, children);
            match value.unpack_frozen() {
                Some(frozen) => self.frozen.insert(frozen.to_value().identity(), converted),
                None => unfrozen.insert(value.identity(), converted),
            };
        }
        self.converted(root, &unfrozen).ok_or_else(|| anyhow::anyhow!("{what}: the depset wasn't converted"))
    }

    /// A depset's input set, if it's been converted.
    fn converted<'v>(&self, value: Value<'v>, unfrozen: &FxHashMap<ValueIdentity<'v>, InputSet>) -> Option<InputSet> {
        match value.unpack_frozen() {
            Some(frozen) => self.frozen.get(&frozen.to_value().identity()).cloned(),
            None => unfrozen.get(&value.identity()).cloned(),
        }
    }
}

fn artifacts(items: &[Value<'_>], what: &str) -> anyhow::Result<Vec<Artifact>> {
    items
        .iter()
        .map(|item| FileValue::artifact(*item).ok_or_else(|| anyhow::anyhow!("{what} must contain files, not {}", item.get_type())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bazel_label::Label;

    fn file(name: &str) -> Artifact {
        Artifact::source(Label::parse_canonical(&format!("@@//pkg:{name}")).unwrap())
    }

    #[test]
    fn flattens_once_each_and_sorted() {
        let shared = InputSet::of(vec![file("b"), file("a")]);
        let set = InputSet::new(vec![file("c"), file("a")], vec![shared.clone(), shared]);
        assert_eq!(set.to_vec(), vec![file("a"), file("b"), file("c")]);
    }

    #[test]
    fn deep_sets_flatten_and_drop_without_recursion() {
        let (a, b) = (file("a"), file("b"));
        let mut set = InputSet::of(vec![a, b]);
        for _ in 0..200_000 {
            set = InputSet::new(vec![a], vec![set]);
        }
        assert_eq!(set.to_vec(), vec![a, b]);
        drop(set);
    }

    #[test]
    fn sets_made_again_and_again_stay_cheap() {
        let leaf = file("leaf");
        let mut set = InputSet::of(vec![leaf]);
        for _ in 0..200_000 {
            set = InputSet::new(vec![leaf], vec![set]);
        }
        assert_eq!(set.to_vec(), vec![leaf]);
    }

    #[test]
    fn one_file_children_are_kept_as_their_file() {
        let set = InputSet::new(vec![file("a")], vec![InputSet::of(vec![file("b")])]);
        assert_eq!(set.direct(), &[file("a"), file("b")]);
        assert_eq!(set.children().len(), 0);
    }

    #[test]
    fn children_keep_their_order() {
        let a = InputSet::of(vec![file("a1"), file("a2")]);
        let b = InputSet::of(vec![file("b1"), file("b2")]);
        let set = InputSet::new(vec![file("c")], vec![b.clone(), a.clone(), b.clone()]);
        let children: Vec<InputSet> = set.children().collect();
        assert_eq!(children, vec![b, a]);
    }

    #[test]
    fn equal_sets_are_one_node() {
        let a = InputSet::new(vec![file("x")], vec![InputSet::of(vec![file("y")])]);
        let b = InputSet::new(vec![file("x")], vec![InputSet::of(vec![file("y")])]);
        assert_eq!(a, b);
        assert_ne!(a, InputSet::of(vec![file("x")]));
    }
}
