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
use std::collections::HashMap;
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

/// Every node alive in some action's inputs.
static NODES: LazyLock<Sharing<Node>> = LazyLock::new(Sharing::default);

impl InputSet {
    /// The set of `direct` and the members of `children`.
    pub fn new(mut direct: Vec<Artifact>, children: Vec<InputSet>) -> InputSet {
        direct.sort();
        direct.dedup();
        let mut children: Vec<Arc<Node>> = children.into_iter().filter_map(|child| child.0).collect();
        children.sort_by_key(|child| Arc::as_ptr(child) as usize);
        children.dedup_by(|a, b| Arc::ptr_eq(a, b));
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

    /// Every member, once each, sorted.
    pub fn to_vec(&self) -> Vec<Artifact> {
        let Some(root) = &self.0 else {
            return Vec::new();
        };
        let mut visited: std::collections::HashSet<*const Node> = std::collections::HashSet::new();
        let mut pending: Vec<&Arc<Node>> = vec![root];
        let mut members = Vec::new();
        while let Some(node) = pending.pop() {
            if !visited.insert(Arc::as_ptr(node)) {
                continue;
            }
            members.extend_from_slice(&node.direct);
            pending.extend(node.children.iter());
        }
        members.sort();
        members.dedup();
        members
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

/// Converts the depsets of one target's analysis, converting each frozen
/// depset once however many actions include it. Frozen values don't move,
/// and outlive the analysis, so their identities stay theirs.
#[derive(Default)]
pub struct InputSets {
    frozen: HashMap<ValueIdentity<'static>, InputSet>,
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

    fn depset(&mut self, value: Value<'_>, what: &str) -> anyhow::Result<InputSet> {
        let identity = value.unpack_frozen().map(|frozen| frozen.to_value().identity());
        if let Some(converted) = identity.and_then(|identity| self.frozen.get(&identity)) {
            return Ok(converted.clone());
        }
        let Some(depset) = Depset::from_value(value) else {
            anyhow::bail!("{what} must contain files, not {}", value.get_type());
        };
        let direct = artifacts(&depset.direct, what)?;
        let mut children = Vec::with_capacity(depset.transitive.len());
        for child in &depset.transitive {
            children.push(self.depset(*child, what)?);
        }
        let converted = InputSet::new(direct, children);
        if let Some(identity) = identity {
            self.frozen.insert(identity, converted.clone());
        }
        Ok(converted)
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
    fn equal_sets_are_one_node() {
        let a = InputSet::new(vec![file("x")], vec![InputSet::of(vec![file("y")])]);
        let b = InputSet::new(vec![file("x")], vec![InputSet::of(vec![file("y")])]);
        assert_eq!(a, b);
        assert_ne!(a, InputSet::of(vec![file("x")]));
    }
}
