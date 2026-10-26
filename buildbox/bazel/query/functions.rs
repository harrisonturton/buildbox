//! The functions every query language has, over any environment.

use crate::engine::{Context, QueryEnvironment, QueryError};
use crate::nodes::NodeSet;
use crate::parse::Arg;
use crate::universe::TargetRef;
use bazel_packages::{AttrValue, TagFilter, Target, is_test_kind};
use intern::Symbol;
use regex::Regex;
use std::collections::{HashMap, HashSet, VecDeque};

/// Evaluate a call of the function `name`.
pub(crate) async fn call<E: QueryEnvironment>(cx: &Context<'_, E>, name: &str, args: &[Arg]) -> Result<NodeSet<E::Node>, QueryError> {
    match name {
        "deps" => {
            let roots = expr_arg(cx, args, 0).await?;
            cx.env.transitive_closure(roots, depth_arg(args, 1)).await
        }
        "rdeps" => {
            let universe = expr_arg(cx, args, 0).await?;
            let targets = expr_arg(cx, args, 1).await?;
            rdeps(cx.env, universe, targets, depth_arg(args, 2)).await
        }
        "somepath" => {
            let from = expr_arg(cx, args, 0).await?;
            let to = expr_arg(cx, args, 1).await?;
            somepath(cx.env, from, to).await
        }
        "allpaths" => {
            let from = expr_arg(cx, args, 0).await?;
            let to = expr_arg(cx, args, 1).await?;
            allpaths(cx.env, from, to).await
        }
        "kind" => kind(cx, args).await,
        "filter" => filter(cx, args).await,
        "attr" => attr(cx, args).await,
        "labels" => labels(cx, args).await,
        "tests" => tests(cx.env, expr_arg(cx, args, 0).await?).await,
        "siblings" => cx.env.siblings(&expr_arg(cx, args, 0).await?).await,
        "some" => some(cx, args).await,
        "visible" => visible(cx, args).await,
        "same_pkg_direct_rdeps" => same_package_direct_rdeps(cx.env, expr_arg(cx, args, 0).await?).await,
        "executables" => executables(cx.env, expr_arg(cx, args, 0).await?).await,
        _ => cx.env.call(cx, name, args).await,
    }
}

/// The value of the expression that's argument `index`.
pub async fn expr_arg<E: QueryEnvironment>(cx: &Context<'_, E>, args: &[Arg], index: usize) -> Result<NodeSet<E::Node>, QueryError> {
    match args.get(index) {
        Some(Arg::Expr(expr)) => cx.eval(expr).await,
        _ => Err(QueryError::in_expression(format!("expected an expression as argument {}", index + 1))),
    }
}

/// The word that's argument `index`.
pub fn word_arg(args: &[Arg], index: usize) -> Result<&str, QueryError> {
    match args.get(index) {
        Some(Arg::Word(word)) => Ok(word),
        _ => Err(QueryError::in_expression(format!("expected a word as argument {}", index + 1))),
    }
}

/// The regular expression that's argument `index` of the function
/// `function`.
pub fn regex_arg(function: &str, args: &[Arg], index: usize) -> Result<Regex, QueryError> {
    let word = word_arg(args, index)?;
    Regex::new(word).map_err(|error| QueryError::in_expression(format!("illegal '{function}' pattern regexp '{word}': {error}")))
}

/// The depth that's argument `index`, if given; a negative one is no limit.
fn depth_arg(args: &[Arg], index: usize) -> Option<u32> {
    match args.get(index) {
        Some(Arg::Int(depth)) => u32::try_from(*depth).ok(),
        _ => None,
    }
}

/// The nodes `roots` depend on, transitively, to `depth` steps if given,
/// and the roots.
pub async fn breadth_first<E: QueryEnvironment>(env: &E, roots: NodeSet<E::Node>, depth: Option<u32>) -> Result<NodeSet<E::Node>, QueryError> {
    let mut result = roots.clone();
    let mut frontier = roots.into_vec();
    let mut level = 0;
    while !frontier.is_empty() && depth.is_none_or(|depth| level < depth) {
        let mut next = Vec::new();
        for node in &frontier {
            for dep in env.fwd_deps(node).await? {
                if result.insert(dep.clone()) {
                    next.push(dep);
                }
            }
        }
        frontier = next;
        level += 1;
    }
    Ok(result)
}

/// The edges among the transitive closure of `roots`: each node's direct
/// dependencies, in the closure.
pub async fn closure_edges<E: QueryEnvironment>(env: &E, roots: NodeSet<E::Node>) -> Result<HashMap<E::Node, Vec<E::Node>>, QueryError> {
    let closure = env.transitive_closure(roots, None).await?;
    let mut edges = HashMap::with_capacity(closure.len());
    for node in &closure {
        let deps: Vec<E::Node> = env.fwd_deps(node).await?.into_iter().filter(|dep| closure.contains(dep)).collect();
        edges.insert(node.clone(), deps);
    }
    Ok(edges)
}

fn reversed<N: Clone + Eq + std::hash::Hash>(edges: &HashMap<N, Vec<N>>) -> HashMap<N, Vec<N>> {
    let mut reverse: HashMap<N, Vec<N>> = HashMap::new();
    for (from, deps) in edges {
        for dep in deps {
            reverse.entry(dep.clone()).or_default().push(from.clone());
        }
    }
    reverse
}

/// The nodes of the transitive closure of `universe` that depend on
/// `targets`, to `depth` steps if given, and those of `targets` in it.
pub async fn rdeps<E: QueryEnvironment>(env: &E, universe: NodeSet<E::Node>, targets: NodeSet<E::Node>, depth: Option<u32>) -> Result<NodeSet<E::Node>, QueryError> {
    let edges = closure_edges(env, universe).await?;
    let reverse = reversed(&edges);
    let mut result: NodeSet<E::Node> = targets.into_iter().filter(|node| edges.contains_key(node)).collect();
    let mut frontier: Vec<E::Node> = result.iter().cloned().collect();
    let mut level = 0;
    while !frontier.is_empty() && depth.is_none_or(|depth| level < depth) {
        let mut next = Vec::new();
        for node in &frontier {
            for rdep in reverse.get(node).into_iter().flatten() {
                if result.insert(rdep.clone()) {
                    next.push(rdep.clone());
                }
            }
        }
        frontier = next;
        level += 1;
    }
    Ok(result)
}

/// A shortest path from a node of `from` to one of `to`, in the path's
/// order: from the first of `from` that reaches any of `to`.
async fn somepath<E: QueryEnvironment>(env: &E, from: NodeSet<E::Node>, to: NodeSet<E::Node>) -> Result<NodeSet<E::Node>, QueryError> {
    let edges = closure_edges(env, from.clone()).await?;
    for start in &from {
        if let Some(path) = shortest_path(&edges, start, &to) {
            return Ok(path.into_iter().collect());
        }
    }
    Ok(NodeSet::new())
}

/// The shortest path from `start` to any of `to`, breadth first.
fn shortest_path<N: Clone + Eq + std::hash::Hash>(edges: &HashMap<N, Vec<N>>, start: &N, to: &NodeSet<N>) -> Option<Vec<N>> {
    let mut parents: HashMap<N, N> = HashMap::new();
    let mut seen: HashSet<N> = HashSet::from([start.clone()]);
    let mut queue = VecDeque::from([start.clone()]);
    while let Some(node) = queue.pop_front() {
        if to.contains(&node) {
            let mut path = vec![node.clone()];
            let mut current = node;
            while let Some(parent) = parents.get(&current) {
                path.push(parent.clone());
                current = parent.clone();
            }
            path.reverse();
            return Some(path);
        }
        for dep in edges.get(&node).into_iter().flatten() {
            if seen.insert(dep.clone()) {
                parents.insert(dep.clone(), node.clone());
                queue.push_back(dep.clone());
            }
        }
    }
    None
}

/// The nodes on any path from a node of `from` to one of `to`.
async fn allpaths<E: QueryEnvironment>(env: &E, from: NodeSet<E::Node>, to: NodeSet<E::Node>) -> Result<NodeSet<E::Node>, QueryError> {
    let edges = closure_edges(env, from).await?;
    let reverse = reversed(&edges);
    let mut reaching: NodeSet<E::Node> = to.into_iter().filter(|node| edges.contains_key(node)).collect();
    let mut pending: Vec<E::Node> = reaching.iter().cloned().collect();
    while let Some(node) = pending.pop() {
        for rdep in reverse.get(&node).into_iter().flatten() {
            if reaching.insert(rdep.clone()) {
                pending.push(rdep.clone());
            }
        }
    }
    Ok(reaching)
}

async fn kind<E: QueryEnvironment>(cx: &Context<'_, E>, args: &[Arg]) -> Result<NodeSet<E::Node>, QueryError> {
    let pattern = regex_arg("kind", args, 0)?;
    let mut kept = NodeSet::new();
    for node in expr_arg(cx, args, 1).await? {
        let target = cx.env.target_of(&node).await?;
        if pattern.is_match(&kind_of(&target)) {
            kept.insert(node);
        }
    }
    Ok(kept)
}

async fn filter<E: QueryEnvironment>(cx: &Context<'_, E>, args: &[Arg]) -> Result<NodeSet<E::Node>, QueryError> {
    let pattern = regex_arg("filter", args, 0)?;
    let nodes = expr_arg(cx, args, 1).await?;
    Ok(nodes.into_iter().filter(|node| pattern.is_match(&cx.env.label(node).to_string())).collect())
}

async fn attr<E: QueryEnvironment>(cx: &Context<'_, E>, args: &[Arg]) -> Result<NodeSet<E::Node>, QueryError> {
    let attr = Symbol::intern(word_arg(args, 0)?);
    let pattern = regex_arg("attr", args, 1)?;
    let mut kept = NodeSet::new();
    for node in expr_arg(cx, args, 2).await? {
        let Some(values) = cx.env.attr_strings(&node, attr).await? else {
            continue;
        };
        if values.iter().any(|value| pattern.is_match(value)) {
            kept.insert(node);
        }
    }
    Ok(kept)
}

async fn labels<E: QueryEnvironment>(cx: &Context<'_, E>, args: &[Arg]) -> Result<NodeSet<E::Node>, QueryError> {
    let attr = Symbol::intern(word_arg(args, 0)?);
    let mut labels = NodeSet::new();
    for node in expr_arg(cx, args, 1).await? {
        labels.extend(cx.env.attr_nodes(&node, attr).await?);
    }
    Ok(labels)
}

async fn some<E: QueryEnvironment>(cx: &Context<'_, E>, args: &[Arg]) -> Result<NodeSet<E::Node>, QueryError> {
    let nodes = expr_arg(cx, args, 0).await?;
    if nodes.is_empty() {
        return Err(QueryError::in_expression("argument set is empty"));
    }
    let count = match args.get(1) {
        Some(Arg::Int(count)) => usize::try_from(*count).unwrap_or(0),
        _ => 1,
    };
    Ok(nodes.into_iter().take(count).collect())
}

/// The nodes of the second argument that every node of the first can see.
async fn visible<E: QueryEnvironment>(cx: &Context<'_, E>, args: &[Arg]) -> Result<NodeSet<E::Node>, QueryError> {
    let viewers = expr_arg(cx, args, 0).await?;
    let mut kept = NodeSet::new();
    'nodes: for node in expr_arg(cx, args, 1).await? {
        for viewer in &viewers {
            if !cx.env.is_visible(viewer, &node).await? {
                continue 'nodes;
            }
        }
        kept.insert(node);
    }
    Ok(kept)
}

/// The nodes of the packages of `targets` that depend directly on any of
/// them.
async fn same_package_direct_rdeps<E: QueryEnvironment>(env: &E, targets: NodeSet<E::Node>) -> Result<NodeSet<E::Node>, QueryError> {
    let mut result = NodeSet::new();
    for node in env.siblings(&targets).await? {
        if env.fwd_deps(&node).await?.iter().any(|dep| targets.contains(dep)) {
            result.insert(node);
        }
    }
    Ok(result)
}

/// The rules among `nodes` that are executable, other than tests.
async fn executables<E: QueryEnvironment>(env: &E, nodes: NodeSet<E::Node>) -> Result<NodeSet<E::Node>, QueryError> {
    let mut kept = NodeSet::new();
    for node in nodes {
        let target = env.target_of(&node).await?;
        let Target::Rule(rule) = target.target() else {
            continue;
        };
        let class = env.rule_class(target.package().rule_class(rule)).await?;
        let executable = class.executable || matches!(rule.attr(Symbol::intern("executable")), Some(AttrValue::Bool(true)));
        if executable && !is_test_rule(&target) {
            kept.insert(node);
        }
    }
    Ok(kept)
}

/// The tests among `nodes`, with test suites replaced by the tests they
/// name, which their tags select.
async fn tests<E: QueryEnvironment>(env: &E, nodes: NodeSet<E::Node>) -> Result<NodeSet<E::Node>, QueryError> {
    let mut tests = NodeSet::new();
    let mut suites = NodeSet::new();
    let mut pending = Vec::new();
    for node in nodes {
        let target = env.target_of(&node).await?;
        if is_test_rule(&target) {
            tests.insert(node);
        } else if is_test_suite(&target) && suites.insert(node.clone()) {
            pending.push(node);
        }
    }
    while let Some(suite) = pending.pop() {
        for (node, target) in suite_members(env, &suite).await? {
            if is_test_suite(&target) && suites.insert(node.clone()) {
                pending.push(node);
            } else if is_test_rule(&target) {
                tests.insert(node);
            }
        }
    }
    Ok(tests)
}

/// The tests and suites a test suite names that its tags select: those
/// its `tests` name, and its `$implicit_tests`.
async fn suite_members<E: QueryEnvironment>(env: &E, suite: &E::Node) -> Result<Vec<(E::Node, TargetRef)>, QueryError> {
    let suite_target = env.target_of(suite).await?;
    let filter = TagFilter::of(&string_list_attr(&suite_target, "tags"));
    let mut members = Vec::new();
    for attr in ["tests", "$implicit_tests"] {
        for node in env.attr_nodes(suite, Symbol::intern(attr)).await? {
            let target = env.target_of(&node).await?;
            let selected = match target.target() {
                Target::Rule(rule) => is_test_suite(&target) || filter.includes(rule),
                _ => false,
            };
            if selected {
                members.push((node, target));
            }
        }
    }
    Ok(members)
}

/// A rule's string list attribute, as given.
pub fn string_list_attr(target: &TargetRef, name: &str) -> Vec<String> {
    match target.target() {
        Target::Rule(rule) => match rule.attr(Symbol::intern(name)) {
            Some(AttrValue::StringList(values)) => values.clone(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Whether a target is a test rule, as Bazel's query tells: a rule whose
/// kind ends `_test`.
pub fn is_test_rule(target: &TargetRef) -> bool {
    match target.target() {
        Target::Rule(rule) => is_test_kind(target.package().rule_class(rule).kind.as_str()),
        _ => false,
    }
}

/// Whether a target is a `test_suite`.
pub fn is_test_suite(target: &TargetRef) -> bool {
    match target.target() {
        Target::Rule(rule) => target.package().rule_class(rule).kind.as_str() == "test_suite",
        _ => false,
    }
}

/// A target's kind, as `kind()` and `--output=label_kind` describe it.
pub fn kind_of(target: &TargetRef) -> String {
    match target.target() {
        Target::Rule(rule) => format!("{} rule", target.package().rule_class(rule).kind),
        Target::SourceFile(_) => "source file".to_string(),
        Target::OutputFile { .. } => "generated file".to_string(),
        Target::PackageGroup(_) => "package group".to_string(),
    }
}
