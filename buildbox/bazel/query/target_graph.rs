//! The package graph `query` reads: targets, and the dependencies their
//! attributes declare in any configuration.

use crate::engine::{Context, QueryEnvironment, QueryError};
use crate::functions::{expr_arg, rdeps, word_arg};
use crate::nodes::NodeSet;
use crate::parse::{Arg, DEFAULT_FUNCTIONS, Function, UNIVERSE_FUNCTIONS};
use crate::universe::{TargetRef, Universe, attr_value};
use async_trait::async_trait;
use bazel_label::{Label, PackageId, RepoName};
use bazel_packages::{AttrValue, RuleClass, RuleClassRef, SelectorPart, Target};
use intern::{PathId, Symbol};
use skyframe::Compute;
use std::collections::BTreeSet;
use std::sync::Arc;

#[async_trait]
impl<C: Compute> QueryEnvironment for Universe<C> {
    type Node = Label;

    fn functions(&self) -> Vec<Function> {
        let mut functions = DEFAULT_FUNCTIONS.to_vec();
        if self.scope().is_some() {
            functions.extend_from_slice(UNIVERSE_FUNCTIONS);
        }
        functions
    }

    fn label(&self, node: &Label) -> Label {
        *node
    }

    async fn targets_matching(&self, pattern: &str) -> Result<NodeSet<Label>, QueryError> {
        Ok(self.resolve(pattern).await?.into_iter().collect())
    }

    async fn fwd_deps(&self, node: &Label) -> Result<Vec<Label>, QueryError> {
        let target = self.target(*node).await?;
        self.deps_of(&target).await
    }

    async fn transitive_closure(&self, roots: NodeSet<Label>, depth: Option<u32>) -> Result<NodeSet<Label>, QueryError> {
        self.closure(roots, depth).await
    }

    async fn target_of(&self, node: &Label) -> Result<TargetRef, QueryError> {
        self.target(*node).await
    }

    async fn rule_class(&self, class: &RuleClassRef) -> Result<Arc<RuleClass>, QueryError> {
        Universe::rule_class(self, class).await
    }

    async fn attr_strings(&self, node: &Label, attr: Symbol) -> Result<Option<Vec<String>>, QueryError> {
        let target = self.target(*node).await?;
        let Target::Rule(rule) = target.target() else {
            return Ok(None);
        };
        if attr.as_str() == "name" {
            return Ok(Some(vec![rule.name.to_string()]));
        }
        if attr.as_str().starts_with("generator_") {
            let generated = generator_attrs(self, &target, rule).await?;
            let value = generated.into_iter().flatten().find(|(name, _)| *name == attr.as_str()).map(|(_, value)| value);
            return Ok(Some(vec![value.unwrap_or_default()]));
        }
        let class = Universe::rule_class(self, target.package().rule_class(rule)).await?;
        let Some(schema) = class.attr(attr) else {
            return Ok(None);
        };
        let value = rule.attr_or_computed(attr).or_else(|| attr_value(rule, schema));
        let Some(value) = value else {
            return Ok(Some(Vec::new()));
        };
        Ok(Some(possible_values(value).iter().map(attr_string).collect()))
    }

    async fn attr_nodes(&self, node: &Label, attr: Symbol) -> Result<Vec<Label>, QueryError> {
        let target = self.target(*node).await?;
        let Target::Rule(rule) = target.target() else {
            return Ok(Vec::new());
        };
        if attr.as_str() == "$implicit_tests" {
            return Ok(target.package().implicit_tests(rule));
        }
        let class = Universe::rule_class(self, target.package().rule_class(rule)).await?;
        let Some(schema) = class.attr(attr) else {
            return Ok(Vec::new());
        };
        let mut labels = Vec::new();
        if let Some(value) = rule.attr_or_computed(attr).or_else(|| attr_value(rule, schema)) {
            value.labels(&mut labels);
        }
        Ok(labels)
    }

    async fn siblings(&self, nodes: &NodeSet<Label>) -> Result<NodeSet<Label>, QueryError> {
        let mut siblings = NodeSet::new();
        for package in nodes.iter().map(|label| label.package).collect::<BTreeSet<_>>() {
            let loaded = self.package(package).await?;
            siblings.extend(loaded.0.targets.iter().map(|target| loaded.0.label(target.name())));
        }
        Ok(siblings)
    }

    async fn is_visible(&self, from: &Label, target: &Label) -> Result<bool, QueryError> {
        if from.package == target.package {
            return Ok(true);
        }
        let target = self.target(*target).await?;
        for label in visibility_of(&target) {
            if self.visibility_allows(label, from.package).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn call(&self, cx: &Context<'_, Self>, name: &str, args: &[Arg]) -> Result<NodeSet<Label>, QueryError> {
        match name {
            "buildfiles" | "loadfiles" => {
                let targets = expr_arg(cx, args, 0).await?;
                build_files(self, targets, name == "buildfiles").await
            }
            "allrdeps" => {
                let targets = expr_arg(cx, args, 0).await?;
                let depth = match args.get(1) {
                    Some(Arg::Int(depth)) => u32::try_from(*depth).ok(),
                    _ => None,
                };
                let universe = self.scope_targets().await?;
                rdeps(self, universe, targets, depth).await
            }
            "rbuildfiles" => {
                let files: Vec<&str> = (0..args.len()).map(|index| word_arg(args, index)).collect::<Result<_, _>>()?;
                reverse_build_files(self, &files).await
            }
            _ => Err(QueryError::in_expression(format!("{name}() isn't supported"))),
        }
    }
}

impl<C: Compute> Universe<C> {
    /// The targets the universe scope names.
    async fn scope_targets(&self) -> Result<NodeSet<Label>, QueryError> {
        let mut targets = NodeSet::new();
        for pattern in self.scope().unwrap_or_default() {
            targets.extend(self.resolve(pattern).await?);
        }
        Ok(targets)
    }

    /// Whether one label of a target's visibility lets a package see it.
    async fn visibility_allows(&self, label: Label, viewer: PackageId) -> Result<bool, QueryError> {
        match label.name.as_str() {
            "public" if is_visibility_constant(label) => return Ok(true),
            "private" if is_visibility_constant(label) => return Ok(false),
            "__pkg__" => return Ok(viewer == label.package),
            "__subpackages__" => return Ok(viewer.repo == label.package.repo && viewer.path.starts_with(label.package.path)),
            _ => {}
        }
        let group = self.target(label).await?;
        let Target::PackageGroup(group) = group.target() else {
            return Ok(false);
        };
        if package_specs_contain(&group.packages, label.package.repo, viewer) {
            return Ok(true);
        }
        for include in &group.includes {
            if Box::pin(self.visibility_allows(*include, viewer)).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// The labels of a target's visibility: its own, its package's default,
/// or for a generated file, its rule's.
pub(crate) fn visibility_of(target: &TargetRef) -> Vec<Label> {
    let package = target.package();
    let visibility = Symbol::intern("visibility");
    let rule = match target.target() {
        Target::Rule(rule) => Some(rule),
        Target::OutputFile { rule, .. } => package.target(*rule).and_then(Target::as_rule),
        Target::SourceFile(file) => return file.visibility.clone().unwrap_or_default(),
        Target::PackageGroup(_) => return vec![public()],
    };
    match rule.and_then(|rule| rule.attr(visibility)) {
        Some(AttrValue::LabelList(labels)) => labels.clone(),
        _ if rule.is_some_and(|rule| package.rule_class(rule).kind.as_str() == "config_setting") => vec![public()],
        _ => package.args.default_visibility.clone(),
    }
}

/// The attributes Bazel gives a rule a macro created: the `name` the
/// BUILD file gave the macro, or the rule's own, the macro's function, and
/// where the BUILD file called it, from where the package's path first
/// appears in that file's path, as Bazel cuts it.
pub(crate) async fn generator_attrs<C: Compute>(universe: &Universe<C>, target: &TargetRef, rule: &bazel_packages::Rule) -> Result<Option<[(&'static str, String); 3]>, QueryError> {
    let Some(generator) = &rule.generator else {
        return Ok(None);
    };
    let build_file = universe.build_file(target.package()).await?;
    let name = match generator.name.is_empty() {
        true => rule.name.to_string(),
        false => generator.name.to_string(),
    };
    let absolute = format!("{}:{}:{}", build_file.display(), generator.location.line, generator.location.column);
    let package = target.package().id.path.to_string();
    let location = match absolute.find(&package) {
        Some(start) => absolute[start..].to_string(),
        None => String::new(),
    };
    Ok(Some([("generator_name", name), ("generator_function", generator.function.to_string()), ("generator_location", location)]))
}

/// The package groups a target's visibility names, which it depends on,
/// as Bazel's query has it: not `//visibility:public` or `private`, nor
/// `__pkg__` or `__subpackages__`.
pub(crate) fn visibility_dependencies(target: &TargetRef) -> Vec<Label> {
    let special = |label: &Label| is_visibility_constant(*label) || matches!(label.name.as_str(), "__pkg__" | "__subpackages__");
    let declared = match target.target() {
        Target::SourceFile(file) => file.visibility.clone().unwrap_or_default(),
        _ => visibility_of(target),
    };
    declared.into_iter().filter(|label| !special(label)).collect()
}

/// Whether a label is `//visibility:public` or `//visibility:private`,
/// which mean the same in any repository.
fn is_visibility_constant(label: Label) -> bool {
    label.package.path.to_string() == "visibility" && matches!(label.name.as_str(), "public" | "private")
}

/// `//visibility:public`, which every package can see.
fn public() -> Label {
    Label::parse_canonical("@@//visibility:public").expect("a valid label")
}

/// Whether a package group's `packages`, in a package of `repo`, contain a
/// package.
fn package_specs_contain(specs: &[String], repo: RepoName, package: PackageId) -> bool {
    let mut contained = false;
    for spec in specs {
        let (negated, spec) = match spec.strip_prefix('-') {
            Some(spec) => (true, spec),
            None => (false, spec.as_str()),
        };
        if spec_contains(spec, repo, package) {
            if negated {
                return false;
            }
            contained = true;
        }
    }
    contained
}

fn spec_contains(spec: &str, repo: RepoName, package: PackageId) -> bool {
    if spec == "public" {
        return true;
    }
    let Some(path) = spec.strip_prefix("//") else {
        return false;
    };
    if package.repo != repo {
        return false;
    }
    if path == "..." {
        return true;
    }
    match path.strip_suffix("/...") {
        Some(prefix) => PathId::parse(prefix).is_ok_and(|prefix| package.path.starts_with(prefix)),
        None => PathId::parse(path).is_ok_and(|path| package.path == path),
    }
}

/// The `.bzl` files the packages of `targets` load, directly or not, and
/// for `buildfiles()`, the BUILD files of those packages and of the
/// packages of the `.bzl` files.
async fn build_files<C: Compute>(universe: &Universe<C>, targets: NodeSet<Label>, include_build_files: bool) -> Result<NodeSet<Label>, QueryError> {
    let mut files = NodeSet::new();
    let mut loads = BTreeSet::new();
    for package in targets.iter().map(|label| label.package).collect::<BTreeSet<_>>() {
        let loaded = universe.package(package).await?;
        if include_build_files {
            files.insert(loaded.0.build_file);
        }
        universe.loaded_files(loaded.0.loads.clone(), &mut loads).await?;
    }
    if include_build_files {
        for package in loads.iter().map(|label| label.package).collect::<BTreeSet<_>>() {
            if let Ok(loaded) = universe.package(package).await {
                files.insert(loaded.0.build_file);
            }
        }
    }
    files.extend(loads);
    Ok(files)
}

/// The BUILD files of the packages in the universe whose BUILD file is one
/// of `paths`, or which load one of them, directly or not.
async fn reverse_build_files<C: Compute>(universe: &Universe<C>, paths: &[&str]) -> Result<NodeSet<Label>, QueryError> {
    let scope = universe.scope_targets().await?;
    let mut result = NodeSet::new();
    for package in scope.iter().map(|label| label.package).collect::<BTreeSet<_>>() {
        let loaded = universe.package(package).await?;
        let mut files = BTreeSet::new();
        universe.loaded_files(loaded.0.loads.clone(), &mut files).await?;
        files.insert(loaded.0.build_file);
        if files.iter().any(|file| paths.contains(&file_path(*file).as_str())) {
            result.insert(loaded.0.build_file);
        }
    }
    Ok(result)
}

/// A source file's path in its repository.
fn file_path(label: Label) -> String {
    match label.package.path.is_root() {
        true => label.name.to_string(),
        false => format!("{}/{}", label.package.path, label.name),
    }
}

/// Every value an attribute may take, one for each choice of branch of
/// each `select()` it has.
pub fn possible_values(value: &AttrValue) -> Vec<AttrValue> {
    let AttrValue::Configurable(parts) = value else {
        return vec![value.clone()];
    };
    let mut values: Vec<Option<AttrValue>> = vec![None];
    for part in parts {
        let choices: Vec<&AttrValue> = match part {
            SelectorPart::Value(value) => vec![value],
            SelectorPart::Select(select) => select.branches.iter().map(|(_, value)| value).collect(),
        };
        let mut next = Vec::with_capacity(values.len() * choices.len());
        for value in &values {
            for choice in &choices {
                next.push(Some(match value {
                    Some(value) => concatenated(value, choice),
                    None => (*choice).clone(),
                }));
            }
        }
        values = next;
    }
    values.into_iter().flatten().collect()
}

/// Two parts of an attribute's value joined, as `+` joins them.
fn concatenated(left: &AttrValue, right: &AttrValue) -> AttrValue {
    match (left, right) {
        (AttrValue::StringList(a), AttrValue::StringList(b)) => AttrValue::StringList(a.iter().chain(b).cloned().collect()),
        (AttrValue::LabelList(a), AttrValue::LabelList(b)) => AttrValue::LabelList(a.iter().chain(b).copied().collect()),
        (AttrValue::IntList(a), AttrValue::IntList(b)) => AttrValue::IntList(a.iter().chain(b).copied().collect()),
        (AttrValue::String(a), AttrValue::String(b)) => AttrValue::String(format!("{a}{b}")),
        (AttrValue::StringDict(a), AttrValue::StringDict(b)) => AttrValue::StringDict(a.iter().chain(b).cloned().collect()),
        (_, right) => right.clone(),
    }
}

/// An attribute's value as `attr()` matches it: as Bazel's Java writes it.
pub fn attr_string(value: &AttrValue) -> String {
    let list = |items: Vec<String>| format!("[{}]", items.join(", "));
    let dict = |entries: Vec<String>| format!("{{{}}}", entries.join(", "));
    match value {
        AttrValue::None => "null".to_string(),
        AttrValue::Bool(true) => "1".to_string(),
        AttrValue::Bool(false) => "0".to_string(),
        AttrValue::Int(i) => i.to_string(),
        AttrValue::String(s) => s.clone(),
        AttrValue::Label(label) => label.to_string(),
        AttrValue::StringList(items) => list(items.clone()),
        AttrValue::LabelList(labels) => list(labels.iter().map(Label::to_string).collect()),
        AttrValue::IntList(items) => list(items.iter().map(i64::to_string).collect()),
        AttrValue::StringDict(entries) => dict(entries.iter().map(|(k, v)| format!("{k}={v}")).collect()),
        AttrValue::StringListDict(entries) => dict(entries.iter().map(|(k, v)| format!("{k}=[{}]", v.join(", "))).collect()),
        AttrValue::LabelKeyedStringDict(entries) => dict(entries.iter().map(|(k, v)| format!("{k}={v}")).collect()),
        AttrValue::StringKeyedLabelDict(entries) => dict(entries.iter().map(|(k, v)| format!("{k}={v}")).collect()),
        AttrValue::Configurable(_) => possible_values(value).iter().map(attr_string).collect::<Vec<_>>().join(" + "),
    }
}

