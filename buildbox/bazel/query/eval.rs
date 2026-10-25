//! Evaluating query expressions.

use crate::parse::{Arg, Expr, SetOp};
use crate::universe::{QueryError, TargetRef, TargetSet, Universe, attr_value};
use bazel_label::Label;
use bazel_packages::{AttrValue, SelectorPart, Target};
use futures::future::BoxFuture;
use intern::Symbol;
use regex::Regex;
use skyframe::Compute;
use std::collections::{BTreeSet, HashMap, VecDeque};

type Vars = HashMap<String, TargetSet>;

/// Evaluate a query expression.
pub async fn evaluate<C: Compute>(universe: &Universe<C>, expr: &Expr) -> Result<TargetSet, QueryError> {
    eval(universe, expr, &Vars::new()).await
}

fn eval<'a, C: Compute>(universe: &'a Universe<C>, expr: &'a Expr, vars: &'a Vars) -> BoxFuture<'a, Result<TargetSet, QueryError>> {
    Box::pin(async move {
        match expr {
            Expr::Pattern(pattern) => universe.resolve(pattern).await,
            Expr::Variable(name) => vars
                .get(name)
                .cloned()
                .ok_or_else(|| QueryError(format!("undefined variable '{name}'"))),
            Expr::Let { name, value, body } => {
                let value = eval(universe, value, vars).await?;
                let mut inner = vars.clone();
                inner.insert(name.clone(), value);
                eval(universe, body, &inner).await
            }
            Expr::Set(patterns) => {
                let mut targets = TargetSet::new();
                for pattern in patterns {
                    targets.extend(universe.resolve(pattern).await?);
                }
                Ok(targets)
            }
            Expr::Binary { op, left, right } => {
                let left = eval(universe, left, vars).await?;
                let right = eval(universe, right, vars).await?;
                Ok(match op {
                    SetOp::Union => left.union(&right).copied().collect(),
                    SetOp::Intersect => left.intersection(&right).copied().collect(),
                    SetOp::Except => left.difference(&right).copied().collect(),
                })
            }
            Expr::Function { name, args } => function(universe, name, args, vars).await,
        }
    })
}

async fn function<C: Compute>(universe: &Universe<C>, name: &str, args: &[Arg], vars: &Vars) -> Result<TargetSet, QueryError> {
    match name {
        "deps" => {
            let roots = expr_arg(universe, args, 0, vars).await?;
            deps(universe, roots, int_arg(args, 1)).await
        }
        "rdeps" => {
            let universe_set = expr_arg(universe, args, 0, vars).await?;
            let targets = expr_arg(universe, args, 1, vars).await?;
            rdeps(universe, universe_set, targets, int_arg(args, 2)).await
        }
        "allrdeps" => Err(QueryError("allrdeps() requires --universe_scope, which isn't supported; use rdeps(universe, x)".into())),
        "somepath" => {
            let from = expr_arg(universe, args, 0, vars).await?;
            let to = expr_arg(universe, args, 1, vars).await?;
            somepath(universe, from, to).await
        }
        "allpaths" => {
            let from = expr_arg(universe, args, 0, vars).await?;
            let to = expr_arg(universe, args, 1, vars).await?;
            allpaths(universe, from, to).await
        }
        "kind" => {
            let pattern = regex_arg(args, 0)?;
            let targets = expr_arg(universe, args, 1, vars).await?;
            filter_targets(universe, &targets, |target| pattern.is_match(&kind_of(target))).await
        }
        "filter" => {
            let pattern = regex_arg(args, 0)?;
            let targets = expr_arg(universe, args, 1, vars).await?;
            Ok(targets.into_iter().filter(|label| pattern.is_match(&label.to_string())).collect())
        }
        "attr" => {
            let attr = Symbol::intern(word_arg(args, 0)?);
            let pattern = regex_arg(args, 1)?;
            let targets = expr_arg(universe, args, 2, vars).await?;
            attr_filter(universe, &targets, attr, &pattern).await
        }
        "labels" => {
            let attr = Symbol::intern(word_arg(args, 0)?);
            let targets = expr_arg(universe, args, 1, vars).await?;
            attr_labels(universe, &targets, attr).await
        }
        "tests" => {
            let targets = expr_arg(universe, args, 0, vars).await?;
            tests(universe, targets).await
        }
        "siblings" => {
            let targets = expr_arg(universe, args, 0, vars).await?;
            let mut siblings = TargetSet::new();
            for package in targets.iter().map(|label| label.package).collect::<BTreeSet<_>>() {
                let loaded = universe.package(package).await?;
                siblings.extend(loaded.0.targets.iter().map(|target| loaded.0.label(target.name())));
            }
            Ok(siblings)
        }
        "some" => {
            let targets = expr_arg(universe, args, 0, vars).await?;
            let count = int_arg(args, 1).unwrap_or(1).max(0) as usize;
            if targets.is_empty() {
                return Err(QueryError("argument set is empty".into()));
            }
            Ok(targets.into_iter().take(count).collect())
        }
        "buildfiles" | "loadfiles" => {
            let targets = expr_arg(universe, args, 0, vars).await?;
            build_files(universe, targets, name == "buildfiles").await
        }
        "same_pkg_direct_rdeps" => {
            let targets = expr_arg(universe, args, 0, vars).await?;
            same_package_rdeps(universe, targets).await
        }
        "visible" => {
            let _ = expr_arg(universe, args, 0, vars).await?;
            expr_arg(universe, args, 1, vars).await
        }
        other => Err(QueryError(format!("{other}() isn't supported"))),
    }
}

async fn expr_arg<C: Compute>(universe: &Universe<C>, args: &[Arg], index: usize, vars: &Vars) -> Result<TargetSet, QueryError> {
    match args.get(index) {
        Some(Arg::Expr(expr)) => eval(universe, expr, vars).await,
        _ => Err(QueryError(format!("expected an expression as argument {}", index + 1))),
    }
}

fn word_arg(args: &[Arg], index: usize) -> Result<&str, QueryError> {
    match args.get(index) {
        Some(Arg::Word(word)) => Ok(word),
        _ => Err(QueryError(format!("expected a word as argument {}", index + 1))),
    }
}

fn regex_arg(args: &[Arg], index: usize) -> Result<Regex, QueryError> {
    let word = word_arg(args, index)?;
    Regex::new(word).map_err(|error| QueryError(format!("invalid regular expression '{word}': {error}")))
}

fn int_arg(args: &[Arg], index: usize) -> Option<i64> {
    match args.get(index) {
        Some(Arg::Int(value)) => Some(*value),
        _ => None,
    }
}

/// The transitive dependencies of `roots`, to `depth` steps if given.
pub async fn deps<C: Compute>(universe: &Universe<C>, roots: TargetSet, depth: Option<i64>) -> Result<TargetSet, QueryError> {
    let mut result = TargetSet::new();
    let mut frontier = roots;
    let mut referrers: HashMap<Label, TargetRef> = HashMap::new();
    let mut visited = frontier.clone();
    let mut level = 0;
    while !frontier.is_empty() {
        let targets = universe.closure_targets(&frontier, &referrers).await?;
        result.extend(targets.iter().map(TargetRef::label));
        if depth.is_some_and(|depth| level >= depth) {
            break;
        }
        let mut next = TargetSet::new();
        for target in &targets {
            for dep in universe.deps_of(target).await? {
                if visited.insert(dep) {
                    next.insert(dep);
                    referrers.insert(dep, target.clone());
                }
            }
        }
        frontier = next;
        level += 1;
    }
    universe.finish_closure()?;
    Ok(result)
}

/// The direct dependency edges among the transitive dependencies of
/// `roots`, which must exist.
pub async fn edges<C: Compute>(universe: &Universe<C>, roots: &TargetSet) -> Result<HashMap<Label, Vec<Label>>, QueryError> {
    let closure = deps(universe, roots.clone(), None).await?;
    let mut edges = HashMap::with_capacity(closure.len());
    for target in universe.targets(&closure).await? {
        let deps: Vec<Label> = universe.deps_of(&target).await?.into_iter().filter(|dep| closure.contains(dep)).collect();
        edges.insert(target.label(), deps);
    }
    Ok(edges)
}

async fn rdeps<C: Compute>(universe: &Universe<C>, scope: TargetSet, targets: TargetSet, depth: Option<i64>) -> Result<TargetSet, QueryError> {
    let edges = edges(universe, &scope).await?;
    let mut reverse: HashMap<Label, Vec<Label>> = HashMap::new();
    for (from, deps) in &edges {
        for dep in deps {
            reverse.entry(*dep).or_default().push(*from);
        }
    }
    let mut result: TargetSet = targets.into_iter().filter(|label| edges.contains_key(label)).collect();
    let mut frontier: Vec<Label> = result.iter().copied().collect();
    let mut level = 0;
    while !frontier.is_empty() && !depth.is_some_and(|depth| level >= depth) {
        let mut next = Vec::new();
        for label in frontier {
            for rdep in reverse.get(&label).into_iter().flatten() {
                if result.insert(*rdep) {
                    next.push(*rdep);
                }
            }
        }
        frontier = next;
        level += 1;
    }
    Ok(result)
}

async fn somepath<C: Compute>(universe: &Universe<C>, from: TargetSet, to: TargetSet) -> Result<TargetSet, QueryError> {
    let edges = edges(universe, &from).await?;
    let mut parents: HashMap<Label, Label> = HashMap::new();
    let mut queue: VecDeque<Label> = from.iter().copied().collect();
    let mut seen: BTreeSet<Label> = from.clone();
    while let Some(label) = queue.pop_front() {
        if to.contains(&label) {
            let mut path = TargetSet::from([label]);
            let mut current = label;
            while let Some(parent) = parents.get(&current) {
                path.insert(*parent);
                current = *parent;
            }
            return Ok(path);
        }
        for dep in edges.get(&label).into_iter().flatten() {
            if seen.insert(*dep) {
                parents.insert(*dep, label);
                queue.push_back(*dep);
            }
        }
    }
    Ok(TargetSet::new())
}

async fn allpaths<C: Compute>(universe: &Universe<C>, from: TargetSet, to: TargetSet) -> Result<TargetSet, QueryError> {
    let edges = edges(universe, &from).await?;
    let mut reverse: HashMap<Label, Vec<Label>> = HashMap::new();
    for (source, deps) in &edges {
        for dep in deps {
            reverse.entry(*dep).or_default().push(*source);
        }
    }
    let mut reaches_target: TargetSet = to.iter().filter(|label| edges.contains_key(label)).copied().collect();
    let mut frontier: Vec<Label> = reaches_target.iter().copied().collect();
    while let Some(label) = frontier.pop() {
        for rdep in reverse.get(&label).into_iter().flatten() {
            if reaches_target.insert(*rdep) {
                frontier.push(*rdep);
            }
        }
    }
    Ok(reaches_target)
}

async fn filter_targets<C: Compute>(universe: &Universe<C>, targets: &TargetSet, keep: impl Fn(&TargetRef) -> bool) -> Result<TargetSet, QueryError> {
    let found = universe.targets(targets).await?;
    Ok(found.iter().filter(|target| keep(target)).map(TargetRef::label).collect())
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

async fn attr_filter<C: Compute>(universe: &Universe<C>, targets: &TargetSet, attr: Symbol, pattern: &Regex) -> Result<TargetSet, QueryError> {
    let mut matching = TargetSet::new();
    for target in universe.targets(targets).await? {
        let Target::Rule(rule) = target.target() else {
            continue;
        };
        let class = universe.rule_class(target.package().rule_class(rule)).await?;
        let Some(schema) = class.attr(attr) else {
            continue;
        };
        let Some(value) = attr_value(rule, schema) else {
            continue;
        };
        if attr_strings(value).iter().any(|text| pattern.is_match(text)) {
            matching.insert(target.label());
        }
    }
    Ok(matching)
}

/// The strings `attr()` matches an attribute value against: one for a
/// plain value, one per branch for a `select()`.
fn attr_strings(value: &AttrValue) -> Vec<String> {
    match value {
        AttrValue::Configurable(parts) => {
            let mut strings = Vec::new();
            for part in parts {
                match part {
                    SelectorPart::Value(value) => strings.push(attr_string(value)),
                    SelectorPart::Select(select) => strings.extend(select.branches.iter().map(|(_, value)| attr_string(value))),
                }
            }
            strings
        }
        other => vec![attr_string(other)],
    }
}

fn attr_string(value: &AttrValue) -> String {
    let list = |items: Vec<String>| format!("[{}]", items.join(", "));
    match value {
        AttrValue::None => String::new(),
        AttrValue::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
        AttrValue::Int(i) => i.to_string(),
        AttrValue::String(s) => s.clone(),
        AttrValue::Label(label) => label.to_string(),
        AttrValue::StringList(items) => list(items.clone()),
        AttrValue::LabelList(labels) => list(labels.iter().map(Label::to_string).collect()),
        AttrValue::IntList(items) => list(items.iter().map(i64::to_string).collect()),
        AttrValue::StringDict(entries) => format!("{{{}}}", entries.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", ")),
        AttrValue::StringListDict(entries) => format!(
            "{{{}}}",
            entries.iter().map(|(k, v)| format!("{k}=[{}]", v.join(", "))).collect::<Vec<_>>().join(", ")
        ),
        AttrValue::LabelKeyedStringDict(entries) => {
            format!("{{{}}}", entries.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "))
        }
        AttrValue::StringKeyedLabelDict(entries) => {
            format!("{{{}}}", entries.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(", "))
        }
        AttrValue::Configurable(_) => attr_strings(value).join(" + "),
    }
}

async fn attr_labels<C: Compute>(universe: &Universe<C>, targets: &TargetSet, attr: Symbol) -> Result<TargetSet, QueryError> {
    let mut labels = TargetSet::new();
    for target in universe.targets(targets).await? {
        let Target::Rule(rule) = target.target() else {
            continue;
        };
        let class = universe.rule_class(target.package().rule_class(rule)).await?;
        let Some(schema) = class.attr(attr) else {
            continue;
        };
        if let Some(value) = attr_value(rule, schema) {
            let mut found = Vec::new();
            value.labels(&mut found);
            labels.extend(found);
        }
    }
    Ok(labels)
}

/// The test rules among `targets`, with test suites replaced by their tests.
async fn tests<C: Compute>(universe: &Universe<C>, targets: TargetSet) -> Result<TargetSet, QueryError> {
    let mut tests = TargetSet::new();
    let mut seen_suites = BTreeSet::new();
    let mut pending: Vec<Label> = targets.into_iter().collect();
    while let Some(label) = pending.pop() {
        let target = universe.target(label).await?;
        let Target::Rule(rule) = target.target() else {
            continue;
        };
        let kind = target.package().rule_class(rule).kind;
        if kind.as_str() == "test_suite" {
            if !seen_suites.insert(label) {
                continue;
            }
            pending.extend(suite_tests(universe, &target, rule).await?);
            continue;
        }
        let class = universe.rule_class(target.package().rule_class(rule)).await?;
        if class.test {
            tests.insert(label);
        }
    }
    Ok(tests)
}

/// The tests a `test_suite` names, or, if it names none, every test in its
/// package that isn't tagged `manual`.
async fn suite_tests<C: Compute>(universe: &Universe<C>, suite: &TargetRef, rule: &bazel_packages::Rule) -> Result<Vec<Label>, QueryError> {
    if let Some(AttrValue::LabelList(tests)) = rule.attr(Symbol::intern("tests")) {
        if !tests.is_empty() {
            return Ok(tests.clone());
        }
    }
    let mut tests = Vec::new();
    for candidate in suite.package().rules() {
        let class = universe.rule_class(suite.package().rule_class(candidate)).await?;
        let manual = matches!(candidate.attr(Symbol::intern("tags")), Some(AttrValue::StringList(tags)) if tags.iter().any(|t| t == "manual"));
        if class.test && !manual {
            tests.push(suite.package().label(candidate.name));
        }
    }
    Ok(tests)
}

async fn build_files<C: Compute>(universe: &Universe<C>, targets: TargetSet, include_build_files: bool) -> Result<TargetSet, QueryError> {
    let mut files = TargetSet::new();
    let mut seen = BTreeSet::new();
    for package in targets.iter().map(|label| label.package).collect::<BTreeSet<_>>() {
        let loaded = universe.package(package).await?;
        if include_build_files {
            files.insert(loaded.0.build_file);
        }
        universe.loaded_files(loaded.0.loads.clone(), &mut seen).await?;
    }
    files.extend(seen);
    Ok(files)
}

async fn same_package_rdeps<C: Compute>(universe: &Universe<C>, targets: TargetSet) -> Result<TargetSet, QueryError> {
    let mut result = TargetSet::new();
    for package in targets.iter().map(|label| label.package).collect::<BTreeSet<_>>() {
        let loaded = universe.package(package).await?;
        let all: TargetSet = loaded.0.targets.iter().map(|target| loaded.0.label(target.name())).collect();
        for target in universe.targets(&all).await? {
            if universe.deps_of(&target).await?.iter().any(|dep| targets.contains(dep)) {
                result.insert(target.label());
            }
        }
    }
    Ok(result)
}
