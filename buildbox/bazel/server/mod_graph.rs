//! `bb mod graph` and `bb mod deps`: the module graph as a tree, as
//! Bazel's `ModExecutor` expands and prunes it and its text formatter
//! draws it.

use bazel_bzlmod::version::Version;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet, VecDeque};

/// A module of the graph to draw: its name and version, which identify
/// it, and its dependencies, by index.
#[derive(Clone, Debug)]
pub struct GraphModule {
    pub name: String,
    /// Empty for the root and for modules not from a registry, as Bazel's
    /// module keys have them.
    pub version: String,
    pub deps: Vec<usize>,
    /// Whether it's built into Bazel, as `@bazel_tools` is, which the graph
    /// leaves out unless asked to show it.
    pub builtin: bool,
}

impl GraphModule {
    /// The module as Bazel names it, `name@version`, `_` for no version.
    pub fn key(&self) -> String {
        let version = if self.version.is_empty() { "_" } else { &self.version };
        format!("{}@{version}", self.name)
    }
}

/// How to draw the graph.
#[derive(Clone, Debug)]
pub struct GraphOptions {
    /// How deep to draw: `--depth`.
    pub depth: usize,
    /// Whether to show `@bazel_tools` and other builtin modules.
    pub include_builtin: bool,
    /// Whether to show where a dependency cycles back.
    pub cycles: bool,
    /// Whether to draw with ASCII rather than box-drawing characters.
    pub ascii: bool,
}

/// How an edge of the drawn tree is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Edge {
    child: usize,
    /// Whether the child's dependencies are drawn here, rather than where
    /// the traversal first reached it.
    expanded: bool,
    cycle: bool,
}

/// The tree under the root, `modules[0]`, from the modules in `from` if
/// the root isn't among them, drawn as Bazel's `mod graph` draws it.
pub fn draw(modules: &[GraphModule], from: &[usize], options: &GraphOptions) -> String {
    let full = expand(modules, from, options);
    let pruned = prune(&full, options);
    let mut drawing = Drawing {
        modules,
        tree: &pruned,
        chars: if options.ascii { ASCII } else { UTF8 },
        last_child: Vec::new(),
        on_path: HashSet::new(),
        out: String::new(),
    };
    drawing.module(0, Edge { child: 0, expanded: true, cycle: false }, 0);
    drawing.out.push('\n');
    drawing.out
}

/// Bazel's order of modules: by name, then version.
fn compare(modules: &[GraphModule], a: usize, b: usize) -> Ordering {
    let (a, b) = (&modules[a], &modules[b]);
    a.name.cmp(&b.name).then_with(|| version_order(&a.version, &b.version))
}

fn version_order(a: &str, b: &str) -> Ordering {
    match (Version::parse(a), Version::parse(b)) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        _ => a.cmp(b),
    }
}

fn sorted(modules: &[GraphModule], mut indices: Vec<usize>) -> Vec<usize> {
    indices.sort_by(|a, b| compare(modules, *a, *b));
    indices.dedup();
    indices
}

/// Every module reachable from the root's children, each expanded where a
/// breadth-first traversal first reaches it. The root's children are the
/// modules of `from`, or its own dependencies if the root is among them.
fn expand(modules: &[GraphModule], from: &[usize], options: &GraphOptions) -> BTreeMap<usize, Vec<Edge>> {
    let shown = |index: &usize| options.include_builtin || !modules[*index].builtin;
    let mut pinned: Vec<usize> = from.iter().copied().filter(|index| *index != 0).collect();
    if from.contains(&0) {
        pinned.extend(modules[0].deps.iter().copied());
    }
    let pinned = sorted(modules, pinned.into_iter().filter(shown).collect());
    let mut tree = BTreeMap::new();
    tree.insert(0, pinned.iter().map(|child| Edge { child: *child, expanded: true, cycle: false }).collect());
    let mut visited: HashSet<usize> = pinned.iter().copied().collect();
    visited.insert(0);
    let mut queue: VecDeque<usize> = pinned.into_iter().collect();
    while let Some(index) = queue.pop_front() {
        let mut edges = Vec::new();
        for dep in sorted(modules, modules[index].deps.iter().copied().filter(shown).collect()) {
            let first = visited.insert(dep);
            if first {
                queue.push_back(dep);
            }
            edges.push(Edge { child: dep, expanded: first, cycle: false });
        }
        tree.insert(index, edges);
    }
    tree
}

/// The expanded tree cut off below `--depth`, noting where a dependency
/// leads back to a module on the path to it.
fn prune(full: &BTreeMap<usize, Vec<Edge>>, options: &GraphOptions) -> BTreeMap<usize, Vec<Edge>> {
    let mut pruner = Pruner {
        full,
        options,
        tree: BTreeMap::new(),
        on_path: HashSet::from([0]),
    };
    pruner.tree.insert(0, Vec::new());
    for edge in full.get(&0).cloned().unwrap_or_default() {
        pruner.tree.get_mut(&0).expect("inserted above").push(edge);
        pruner.visit(edge, 1, 0);
    }
    pruner.tree
}

struct Pruner<'a> {
    full: &'a BTreeMap<usize, Vec<Edge>>,
    options: &'a GraphOptions,
    tree: BTreeMap<usize, Vec<Edge>>,
    on_path: HashSet<usize>,
}

impl Pruner<'_> {
    fn visit(&mut self, edge: Edge, depth: usize, parent: usize) {
        let index = edge.child;
        self.on_path.insert(index);
        self.tree.entry(index).or_default();
        if depth > 1 {
            let edges = self.tree.entry(parent).or_default();
            if !edges.contains(&edge) {
                edges.push(edge);
            }
        }
        if edge.expanded {
            for child in self.full.get(&index).cloned().unwrap_or_default() {
                if !self.on_path.contains(&child.child) {
                    if depth < self.options.depth {
                        self.visit(child, depth + 1, index);
                    }
                } else if self.options.cycles {
                    let cycle = Edge { cycle: true, ..child };
                    let edges = self.tree.entry(index).or_default();
                    if !edges.contains(&cycle) {
                        edges.push(cycle);
                    }
                }
            }
        }
        self.on_path.remove(&index);
    }
}

/// The characters trees are drawn with: an empty indent, an indent past a
/// child with more after it, a child, and a last child.
struct Charset {
    empty: &'static str,
    previous: &'static str,
    child: &'static str,
    last: &'static str,
}

const ASCII: Charset = Charset {
    empty: "    ",
    previous: "|   ",
    child: "|___",
    last: "|___",
};

const UTF8: Charset = Charset {
    empty: "    ",
    previous: "│   ",
    child: "├───",
    last: "└───",
};

struct Drawing<'a> {
    modules: &'a [GraphModule],
    tree: &'a BTreeMap<usize, Vec<Edge>>,
    chars: Charset,
    /// For each ancestor of the module being drawn, whether it's the last
    /// child of its parent, innermost last.
    last_child: Vec<bool>,
    on_path: HashSet<usize>,
    out: String,
}

impl Drawing<'_> {
    fn module(&mut self, index: usize, edge: Edge, depth: usize) {
        self.indent(depth);
        let added = self.on_path.insert(index);
        let module = &self.modules[index];
        if index == 0 {
            let version = if module.version.is_empty() { "_" } else { &module.version };
            self.out.push_str(&format!("<root> ({}@{version})", module.name));
        } else {
            self.out.push_str(&module.key());
            self.out.push(' ');
        }
        let cycle = edge.cycle || !added;
        if cycle {
            self.out.push_str("(cycle) ");
        } else if !edge.expanded {
            self.out.push_str("(*) ");
        }
        self.out.push('\n');
        if edge.expanded && !cycle {
            let mut children = self.tree.get(&index).cloned().unwrap_or_default();
            children.sort_by(|a, b| {
                b.cycle
                    .cmp(&a.cycle)
                    .then(a.expanded.cmp(&b.expanded))
                    .then_with(|| compare(self.modules, a.child, b.child))
            });
            let count = children.len();
            for (position, child) in children.into_iter().enumerate() {
                self.last_child.push(position + 1 == count);
                self.module(child.child, child, depth + 1);
                self.last_child.pop();
            }
        }
        if added {
            self.on_path.remove(&index);
        }
    }

    fn indent(&mut self, depth: usize) {
        if depth == 0 {
            return;
        }
        let ancestors = self.last_child.len() - 1;
        for last in &self.last_child[..ancestors] {
            self.out.push_str(if *last { self.chars.empty } else { self.chars.previous });
        }
        let last = self.last_child.last().copied().unwrap_or(true);
        self.out.push_str(if last { self.chars.last } else { self.chars.child });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(name: &str, version: &str, deps: &[usize]) -> GraphModule {
        GraphModule {
            name: name.to_string(),
            version: version.to_string(),
            deps: deps.to_vec(),
            builtin: false,
        }
    }

    /// The graph of a workspace depending on bazel_skylib and rules_shell,
    /// as Bazel 9.3.1 resolves it.
    fn graph() -> Vec<GraphModule> {
        vec![
            module("", "", &[1, 4]),
            module("bazel_skylib", "1.8.2", &[2, 3]),
            module("platforms", "1.0.0", &[3]),
            module("rules_license", "1.0.0", &[]),
            module("rules_shell", "0.6.1", &[1, 2, 5]),
            module("bazel_features", "1.42.1", &[1]),
        ]
    }

    fn options(depth: usize) -> GraphOptions {
        GraphOptions {
            depth,
            include_builtin: false,
            cycles: true,
            ascii: false,
        }
    }

    /// A drawing of `lines`, which end with a space where Bazel's do, and
    /// the blank line Bazel ends with.
    fn drawing(lines: &[&str]) -> String {
        lines.iter().map(|line| format!("{line}\n")).collect::<String>() + "\n"
    }

    #[test]
    fn draws_the_graph_as_bazel_does() {
        let expected = drawing(&[
            "<root> (@_)",
            "├───bazel_skylib@1.8.2 ",
            "│   ├───platforms@1.0.0 ",
            "│   │   └───rules_license@1.0.0 (*) ",
            "│   └───rules_license@1.0.0 ",
            "└───rules_shell@0.6.1 ",
            "    ├───bazel_skylib@1.8.2 (*) ",
            "    ├───platforms@1.0.0 (*) ",
            "    └───bazel_features@1.42.1 ",
            "        └───bazel_skylib@1.8.2 (*) ",
        ]);
        assert_eq!(draw(&graph(), &[0], &options(usize::MAX)), expected);
    }

    #[test]
    fn draws_deps_two_levels_deep() {
        let expected = drawing(&[
            "<root> (@_)",
            "├───bazel_skylib@1.8.2 ",
            "│   ├───platforms@1.0.0 ",
            "│   └───rules_license@1.0.0 ",
            "└───rules_shell@0.6.1 ",
            "    ├───bazel_skylib@1.8.2 (*) ",
            "    ├───platforms@1.0.0 (*) ",
            "    └───bazel_features@1.42.1 ",
        ]);
        assert_eq!(draw(&graph(), &[0], &options(2)), expected);
        let skylib = drawing(&["<root> (@_)", "└───bazel_skylib@1.8.2 ", "    ├───platforms@1.0.0 ", "    └───rules_license@1.0.0 "]);
        assert_eq!(draw(&graph(), &[1], &options(2)), skylib);
    }

    #[test]
    fn stops_at_the_depth_asked_for() {
        let expected = drawing(&["<root> (@_)", "├───bazel_skylib@1.8.2 ", "└───rules_shell@0.6.1 "]);
        assert_eq!(draw(&graph(), &[0], &options(1)), expected);
    }
}
