//! The graph of a query's result, which orders and draws it as Bazel's
//! `Digraph` does.
//!
//! Where Bazel's order depends on the order its threads found nodes in,
//! as `--order_output=auto` and `deps` do for graphs and ranks, this
//! orders them as `--order_output=full` does, by label: one of the orders
//! Bazel may print.

use std::collections::HashMap;
use std::fmt::Write;
use std::hash::Hash;

/// A directed graph of nodes.
pub struct Digraph<N> {
    nodes: Vec<N>,
    index: HashMap<N, usize>,
    successors: Vec<Vec<usize>>,
    predecessors: Vec<Vec<usize>>,
}

impl<N: Clone + Eq + Hash + Ord> Digraph<N> {
    /// A graph of `nodes`, without edges.
    pub fn new(nodes: impl IntoIterator<Item = N>) -> Digraph<N> {
        let mut graph = Digraph {
            nodes: Vec::new(),
            index: HashMap::new(),
            successors: Vec::new(),
            predecessors: Vec::new(),
        };
        for node in nodes {
            if graph.index.contains_key(&node) {
                continue;
            }
            graph.index.insert(node.clone(), graph.nodes.len());
            graph.nodes.push(node);
            graph.successors.push(Vec::new());
            graph.predecessors.push(Vec::new());
        }
        graph
    }

    /// Add an edge between nodes of the graph, ignoring others.
    pub fn add_edge(&mut self, from: &N, to: &N) {
        let (Some(&from), Some(&to)) = (self.index.get(from), self.index.get(to)) else {
            return;
        };
        if self.successors[from].contains(&to) {
            return;
        }
        self.successors[from].push(to);
        self.predecessors[to].push(from);
    }

    pub fn nodes(&self) -> &[N] {
        &self.nodes
    }

    /// The indices of the nodes, sorted by node.
    fn sorted(&self, indices: &[usize]) -> Vec<usize> {
        let mut sorted = indices.to_vec();
        sorted.sort_by(|a, b| self.nodes[*a].cmp(&self.nodes[*b]));
        sorted
    }

    /// The nodes in topological order, dependents first, ties broken by
    /// node: the reverse of a depth-first post-order over nodes and edges
    /// in node order.
    pub fn topological_order(&self) -> Vec<&N> {
        let all: Vec<usize> = (0..self.nodes.len()).collect();
        let mut visited = vec![false; self.nodes.len()];
        let mut order = Vec::with_capacity(self.nodes.len());
        for start in self.sorted(&all) {
            self.post_order(start, &mut visited, &mut order);
        }
        order.reverse();
        order.into_iter().map(|index| &self.nodes[index]).collect()
    }

    fn post_order(&self, start: usize, visited: &mut [bool], order: &mut Vec<usize>) {
        if visited[start] {
            return;
        }
        visited[start] = true;
        let mut stack = vec![(start, self.sorted(&self.successors[start]), 0)];
        while let Some((node, successors, next)) = stack.last_mut() {
            let Some(&successor) = successors.get(*next) else {
                order.push(*node);
                stack.pop();
                continue;
            };
            *next += 1;
            if !visited[successor] {
                visited[successor] = true;
                stack.push((successor, self.sorted(&self.successors[successor]), 0));
            }
        }
    }

    /// Each node with its rank, ordered by rank and then node: the length
    /// of the shortest path to it from a node nothing depends on, or for
    /// `longest`, of the longest. The nodes of a cycle share a rank.
    pub fn ranks(&self, longest: bool) -> Vec<(usize, &N)> {
        let components = self.components();
        let ranks = components.ranks(longest);
        let mut ranked: Vec<(usize, &N)> = (0..self.nodes.len()).map(|node| (ranks[components.of[node]], &self.nodes[node])).collect();
        ranked.sort();
        ranked
    }

    /// The strongly connected components of the graph.
    fn components(&self) -> Components {
        let mut tarjan = Tarjan {
            graph: &self.successors,
            index: vec![None; self.nodes.len()],
            low: vec![0; self.nodes.len()],
            on_stack: vec![false; self.nodes.len()],
            stack: Vec::new(),
            next: 0,
            of: vec![0; self.nodes.len()],
            count: 0,
        };
        for node in 0..self.nodes.len() {
            if tarjan.index[node].is_none() {
                tarjan.visit(node);
            }
        }
        let mut successors = vec![Vec::new(); tarjan.count];
        for (from, targets) in self.successors.iter().enumerate() {
            for to in targets {
                let (a, b) = (tarjan.of[from], tarjan.of[*to]);
                if a != b && !successors[a].contains(&b) {
                    successors[a].push(b);
                }
            }
        }
        Components { of: tarjan.of, successors }
    }

    /// Write the graph in Graphviz's dot language, as `--output=graph`
    /// does: nodes named by `name`, and with `factored`, nodes with the
    /// same dependencies and dependents merged into one. `conditions`
    /// gives the `select()` conditions an edge is taken under, to label it
    /// with.
    pub fn write_dot(&self, out: &mut String, style: &DotStyle, name: impl Fn(&N) -> String, conditions: impl Fn(&N, &N) -> Vec<String>) {
        let classes = match style.factored {
            true => self.factored_classes(),
            false => (0..self.nodes.len()).map(|node| vec![node]).collect(),
        };
        let image = self.image(&classes);
        out.push_str("digraph mygraph {\n  node [shape=box];\n");
        for class in image.topological_order() {
            let label = self.class_label(&classes[class.index], style, &name);
            writeln!(out, "  \"{label}\"").ok();
            for successor in image.sorted_successors(class) {
                let target = self.class_label(&classes[successor], style, &name);
                writeln!(out, "  \"{label}\" -> \"{target}\"").ok();
                let annotation = self.edge_conditions(&classes[class.index], &classes[successor], style, &conditions);
                if !annotation.is_empty() {
                    writeln!(out, "  [label=\"{annotation}\"];").ok();
                }
            }
        }
        out.push_str("}\n");
    }

    /// The graph of the classes of a partition of the nodes, each class
    /// ordered by its nodes, as Bazel orders a factored graph's nodes.
    fn image(&self, classes: &[Vec<usize>]) -> Digraph<ClassKey<'_, N>> {
        let mut of = vec![0; self.nodes.len()];
        for (class, members) in classes.iter().enumerate() {
            for member in members {
                of[*member] = class;
            }
        }
        let keys: Vec<ClassKey<'_, N>> = classes.iter().enumerate().map(|(index, members)| ClassKey::new(index, members, &self.nodes)).collect();
        let mut image = Digraph::new(keys.iter().cloned());
        for (from, targets) in self.successors.iter().enumerate() {
            for to in targets {
                image.add_edge(&keys[of[from]], &keys[of[*to]]);
            }
        }
        image
    }

    fn class_label(&self, members: &[usize], style: &DotStyle, name: &impl Fn(&N) -> String) -> String {
        let limit = style.node_limit.map(|limit| limit.saturating_sub(RESERVED_LABEL_CHARS));
        let mut label = String::new();
        for (count, member) in members.iter().enumerate() {
            let text = name(&self.nodes[*member]);
            if count > 0 {
                label.push_str("\\n");
                if limit.is_some_and(|limit| label.len() + text.len() > limit) {
                    write!(label, "...and {} more items", members.len() - count).ok();
                    break;
                }
            }
            label.push_str(&text);
        }
        label
    }

    /// The conditions of the edges between two classes, each once, as a
    /// label for the edge between them.
    fn edge_conditions(&self, from: &[usize], to: &[usize], style: &DotStyle, conditions: &impl Fn(&N, &N) -> Vec<String>) -> String {
        if style.conditional_edges_limit == Some(0) {
            return String::new();
        }
        let mut annotated: Vec<String> = Vec::new();
        let mut label = String::new();
        for source in from {
            for target in to {
                let found = conditions(&self.nodes[*source], &self.nodes[*target]);
                if found.is_empty() {
                    continue;
                }
                let limit = style.conditional_edges_limit.map_or(found.len(), |limit| limit - 1);
                let mut first = true;
                for condition in found.iter().take(limit) {
                    if annotated.contains(condition) {
                        continue;
                    }
                    annotated.push(condition.clone());
                    if !first {
                        label.push_str("\\n");
                    }
                    label.push_str(condition);
                    first = false;
                }
                if found.len() > limit {
                    label.push_str("...");
                }
            }
        }
        label
    }

    /// Classes of nodes that share their dependencies and dependents, each
    /// in node order, as Bazel factors a graph: siblings of a node, or
    /// roots, with the same predecessors and successors.
    fn factored_classes(&self) -> Vec<Vec<usize>> {
        let mut class_of: Vec<Option<usize>> = vec![None; self.nodes.len()];
        let mut classes: Vec<Vec<usize>> = Vec::new();
        let roots: Vec<usize> = (0..self.nodes.len()).filter(|node| self.predecessors[*node].is_empty()).collect();
        self.group_siblings(&self.sorted(&roots), &mut class_of, &mut classes);
        let all: Vec<usize> = (0..self.nodes.len()).collect();
        for node in self.sorted(&all) {
            self.group_siblings(&self.sorted(&self.successors[node]), &mut class_of, &mut classes);
        }
        for (node, class) in class_of.iter().enumerate() {
            if class.is_none() {
                classes.push(vec![node]);
            }
        }
        for class in &mut classes {
            *class = self.sorted(class);
        }
        classes
    }

    fn group_siblings(&self, siblings: &[usize], class_of: &mut [Option<usize>], classes: &mut Vec<Vec<usize>>) {
        for (position, child) in siblings.iter().enumerate() {
            if class_of[*child].is_some() {
                continue;
            }
            let class = classes.len();
            class_of[*child] = Some(class);
            let mut members = vec![*child];
            for sibling in &siblings[position + 1..] {
                if class_of[*sibling].is_none() && self.equivalent(*child, *sibling) {
                    class_of[*sibling] = Some(class);
                    members.push(*sibling);
                }
            }
            classes.push(members);
        }
    }

    fn equivalent(&self, a: usize, b: usize) -> bool {
        let same = |x: &[usize], y: &[usize]| x.len() == y.len() && x.iter().all(|node| y.contains(node));
        same(&self.predecessors[a], &self.predecessors[b]) && same(&self.successors[a], &self.successors[b])
    }

    fn sorted_successors(&self, node: &N) -> Vec<usize> {
        self.sorted(&self.successors[self.index[node]])
    }
}

/// How `--output=graph` draws a graph.
pub struct DotStyle {
    /// Whether to merge nodes with the same dependencies and dependents.
    pub factored: bool,
    /// The most characters of a node's label, `--graph:node_limit`.
    pub node_limit: Option<usize>,
    /// The most conditions to label an edge with,
    /// `--graph:conditional_edges_limit`.
    pub conditional_edges_limit: Option<usize>,
}

impl Default for DotStyle {
    fn default() -> DotStyle {
        DotStyle {
            factored: true,
            node_limit: Some(512),
            conditional_edges_limit: Some(4),
        }
    }
}

/// The characters Bazel keeps for saying how many nodes a label leaves
/// out.
const RESERVED_LABEL_CHARS: usize = "\\n...and 9999999 more items".len();

/// A class of nodes, ordered as Bazel orders sets of nodes: by their
/// nodes, in order.
#[derive(Clone)]
struct ClassKey<'a, N> {
    index: usize,
    members: Vec<&'a N>,
}

impl<'a, N> ClassKey<'a, N> {
    fn new(index: usize, members: &[usize], nodes: &'a [N]) -> ClassKey<'a, N> {
        ClassKey {
            index,
            members: members.iter().map(|member| &nodes[*member]).collect(),
        }
    }
}

impl<N> PartialEq for ClassKey<'_, N> {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl<N> Eq for ClassKey<'_, N> {}

impl<N> Hash for ClassKey<'_, N> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.index.hash(state);
    }
}

impl<N: Ord> PartialOrd for ClassKey<'_, N> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<N: Ord> Ord for ClassKey<'_, N> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.members.cmp(&other.members).then(self.index.cmp(&other.index))
    }
}

/// A graph's strongly connected components and the edges between them.
struct Components {
    /// The component of each node.
    of: Vec<usize>,
    successors: Vec<Vec<usize>>,
}

impl Components {
    /// Each component's rank: its shortest, or longest, distance from a
    /// component nothing depends on.
    fn ranks(&self, longest: bool) -> Vec<usize> {
        let count = self.successors.len();
        let mut indegree = vec![0; count];
        for targets in &self.successors {
            for target in targets {
                indegree[*target] += 1;
            }
        }
        let mut ranks: Vec<Option<usize>> = vec![None; count];
        let mut ready: Vec<usize> = (0..count).filter(|component| indegree[*component] == 0).collect();
        for root in &ready {
            ranks[*root] = Some(0);
        }
        while let Some(component) = ready.pop() {
            let rank = ranks[component].unwrap_or(0);
            for target in &self.successors[component] {
                let candidate = rank + 1;
                ranks[*target] = Some(match ranks[*target] {
                    Some(current) if longest => current.max(candidate),
                    Some(current) => current.min(candidate),
                    None => candidate,
                });
                indegree[*target] -= 1;
                if indegree[*target] == 0 {
                    ready.push(*target);
                }
            }
        }
        ranks.into_iter().map(|rank| rank.unwrap_or(0)).collect()
    }
}

/// Tarjan's algorithm for strongly connected components, without
/// recursion.
struct Tarjan<'a> {
    graph: &'a [Vec<usize>],
    index: Vec<Option<usize>>,
    low: Vec<usize>,
    on_stack: Vec<bool>,
    stack: Vec<usize>,
    next: usize,
    of: Vec<usize>,
    count: usize,
}

impl Tarjan<'_> {
    fn visit(&mut self, root: usize) {
        let mut calls = vec![(root, 0)];
        self.enter(root);
        while let Some((node, edge)) = calls.last_mut() {
            let node = *node;
            if let Some(&target) = self.graph[node].get(*edge) {
                *edge += 1;
                match self.index[target] {
                    None => {
                        self.enter(target);
                        calls.push((target, 0));
                    }
                    Some(index) if self.on_stack[target] => self.low[node] = self.low[node].min(index),
                    Some(_) => {}
                }
                continue;
            }
            calls.pop();
            if let Some((parent, _)) = calls.last() {
                self.low[*parent] = self.low[*parent].min(self.low[node]);
            }
            if Some(self.low[node]) == self.index[node] {
                self.close_component(node);
            }
        }
    }

    fn enter(&mut self, node: usize) {
        self.index[node] = Some(self.next);
        self.low[node] = self.next;
        self.next += 1;
        self.stack.push(node);
        self.on_stack[node] = true;
    }

    fn close_component(&mut self, root: usize) {
        while let Some(member) = self.stack.pop() {
            self.on_stack[member] = false;
            self.of[member] = self.count;
            if member == root {
                break;
            }
        }
        self.count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[(&'static str, &'static str)]) -> Digraph<&'static str> {
        let mut nodes: Vec<&str> = edges.iter().flat_map(|(a, b)| [*a, *b]).collect();
        nodes.sort();
        let mut graph = Digraph::new(nodes);
        for (from, to) in edges {
            graph.add_edge(from, to);
        }
        graph
    }

    #[test]
    fn orders_topologically_by_node() {
        let graph = graph(&[("b", "a"), ("b", "c"), ("c", "d"), ("a", "d")]);
        assert_eq!(graph.topological_order(), [&"b", &"c", &"a", &"d"]);
    }

    #[test]
    fn ranks_shortest_and_longest() {
        let graph = graph(&[("a", "b"), ("b", "c"), ("a", "c")]);
        assert_eq!(graph.ranks(false), [(0, &"a"), (1, &"b"), (1, &"c")]);
        assert_eq!(graph.ranks(true), [(0, &"a"), (1, &"b"), (2, &"c")]);
    }

    #[test]
    fn ranks_cycles_together() {
        let graph = graph(&[("a", "b"), ("b", "c"), ("c", "b")]);
        assert_eq!(graph.ranks(false), [(0, &"a"), (1, &"b"), (1, &"c")]);
    }

    #[test]
    fn factors_nodes_with_the_same_edges() {
        let graph = graph(&[("a", "x"), ("a", "y"), ("a", "z"), ("z", "w")]);
        let mut out = String::new();
        graph.write_dot(&mut out, &DotStyle::default(), |node| node.to_string(), |_, _| Vec::new());
        assert_eq!(out, "digraph mygraph {\n  node [shape=box];\n  \"a\"\n  \"a\" -> \"x\\ny\"\n  \"a\" -> \"z\"\n  \"z\"\n  \"z\" -> \"w\"\n  \"w\"\n  \"x\\ny\"\n}\n");
    }
}
