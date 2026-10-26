//! Sets of the nodes queries evaluate to.

use std::collections::HashSet;
use std::hash::Hash;

/// A set of nodes, which remembers the order they were added in: a
/// path's order, for `somepath`, which queries print it in.
#[derive(Clone, Debug)]
pub struct NodeSet<N> {
    order: Vec<N>,
    members: HashSet<N>,
}

impl<N: Clone + Eq + Hash> NodeSet<N> {
    pub fn new() -> NodeSet<N> {
        NodeSet {
            order: Vec::new(),
            members: HashSet::new(),
        }
    }

    /// Add `node`, returning whether it wasn't in the set already.
    pub fn insert(&mut self, node: N) -> bool {
        if !self.members.insert(node.clone()) {
            return false;
        }
        self.order.push(node);
        true
    }

    pub fn contains(&self, node: &N) -> bool {
        self.members.contains(node)
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// The nodes, in the order they were added.
    pub fn iter(&self) -> std::slice::Iter<'_, N> {
        self.order.iter()
    }

    /// The nodes of this set and then of `other`'s that aren't in it.
    pub fn union(mut self, other: NodeSet<N>) -> NodeSet<N> {
        self.extend(other.order);
        self
    }

    /// The nodes of this set that `other` has.
    pub fn intersection(self, other: &NodeSet<N>) -> NodeSet<N> {
        self.order.into_iter().filter(|node| other.contains(node)).collect()
    }

    /// The nodes of this set that `other` doesn't have.
    pub fn difference(self, other: &NodeSet<N>) -> NodeSet<N> {
        self.order.into_iter().filter(|node| !other.contains(node)).collect()
    }

    /// The nodes, in the order they were added.
    pub fn into_vec(self) -> Vec<N> {
        self.order
    }
}

impl<N: Clone + Eq + Hash> Default for NodeSet<N> {
    fn default() -> NodeSet<N> {
        NodeSet::new()
    }
}

impl<N: Clone + Eq + Hash> Extend<N> for NodeSet<N> {
    fn extend<I: IntoIterator<Item = N>>(&mut self, nodes: I) {
        for node in nodes {
            self.insert(node);
        }
    }
}

impl<N: Clone + Eq + Hash> FromIterator<N> for NodeSet<N> {
    fn from_iter<I: IntoIterator<Item = N>>(nodes: I) -> NodeSet<N> {
        let mut set = NodeSet::new();
        set.extend(nodes);
        set
    }
}

impl<N: Clone + Eq + Hash> IntoIterator for NodeSet<N> {
    type Item = N;
    type IntoIter = std::vec::IntoIter<N>;

    fn into_iter(self) -> Self::IntoIter {
        self.order.into_iter()
    }
}

impl<'a, N> IntoIterator for &'a NodeSet<N> {
    type Item = &'a N;
    type IntoIter = std::slice::Iter<'a, N>;

    fn into_iter(self) -> Self::IntoIter {
        self.order.iter()
    }
}
