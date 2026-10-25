//! The targets a query reads, loaded through the build graph, and the
//! dependency edges between them.

use bazel_label::{Label, PackageId, RepoMappingTable, TargetPattern, LabelContext};
use bazel_fs::RootedPath;
use bazel_loading::{BzlLoadKey, LoadedPackage, PackageKey, RepoRootKey, packages_beneath, rule_class};
use bazel_packages::{AttrDefault, AttrSchema, AttrValue, Cfg, Package, Rule, RuleClass, RuleClassRef, Target};
use futures::future::BoxFuture;
use skyframe::{Compute, SkyError};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A set of targets, in the order queries print them.
pub type TargetSet = BTreeSet<Label>;

/// Which dependencies a query follows.
#[derive(Clone, Copy, Debug)]
pub struct QueryOptions {
    /// Follow dependencies through attributes' default values.
    pub implicit_deps: bool,
    /// Follow dependencies built for the execution platform.
    pub tool_deps: bool,
    /// Keep going past errors, returning what could be computed.
    pub keep_going: bool,
}

impl Default for QueryOptions {
    fn default() -> QueryOptions {
        QueryOptions {
            implicit_deps: true,
            tool_deps: true,
            keep_going: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct QueryError(pub String);

impl From<SkyError> for QueryError {
    fn from(error: SkyError) -> QueryError {
        QueryError(error.message().to_string())
    }
}

/// A target found in a loaded package.
#[derive(Clone)]
pub struct TargetRef {
    pub package: Arc<LoadedPackage>,
    index: usize,
}

impl TargetRef {
    pub fn target(&self) -> &Target {
        &self.package.0.targets[self.index]
    }

    pub fn label(&self) -> Label {
        self.package.0.label(self.target().name())
    }

    pub fn package(&self) -> &Package {
        &self.package.0
    }
}

/// Loads what a query needs, once, and remembers errors when keeping going.
pub struct Universe<C> {
    compute: C,
    pub options: QueryOptions,
    /// The package relative patterns are relative to.
    pub working_package: PackageId,
    pub main_mapping: Arc<RepoMappingTable>,
    /// Where the main repository is, for naming files by their paths.
    pub workspace_root: PathBuf,
    packages: Mutex<HashMap<PackageId, Result<Arc<LoadedPackage>, String>>>,
    classes: Mutex<HashMap<RuleClassRef, Arc<RuleClass>>>,
    errors: Mutex<Vec<String>>,
}

impl<C: Compute> Universe<C> {
    pub fn new(
        compute: C,
        options: QueryOptions,
        working_package: PackageId,
        main_mapping: Arc<RepoMappingTable>,
        workspace_root: PathBuf,
    ) -> Universe<C> {
        Universe {
            compute,
            options,
            working_package,
            main_mapping,
            workspace_root,
            packages: Mutex::new(HashMap::new()),
            classes: Mutex::new(HashMap::new()),
            errors: Mutex::new(Vec::new()),
        }
    }

    pub fn compute(&self) -> &C {
        &self.compute
    }

    /// The errors skipped while keeping going.
    pub fn errors(&self) -> Vec<String> {
        self.errors.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Record an error, failing unless keeping going.
    pub fn error(&self, message: String) -> Result<(), QueryError> {
        if !self.options.keep_going {
            return Err(QueryError(message));
        }
        self.errors.lock().unwrap_or_else(|p| p.into_inner()).push(message);
        Ok(())
    }

    /// Load packages, in parallel, skipping those already loaded.
    pub async fn load(&self, ids: impl IntoIterator<Item = PackageId>) {
        let missing: Vec<PackageId> = {
            let packages = self.packages.lock().unwrap_or_else(|p| p.into_inner());
            let mut missing: Vec<PackageId> = ids.into_iter().filter(|id| !packages.contains_key(id)).collect();
            missing.sort();
            missing.dedup();
            missing
        };
        if missing.is_empty() {
            return;
        }
        let keys: Vec<PackageKey> = missing.iter().map(|id| PackageKey(*id)).collect();
        let results = self.compute.compute_many(&keys).await;
        let mut packages = self.packages.lock().unwrap_or_else(|p| p.into_inner());
        for (id, result) in missing.into_iter().zip(results) {
            packages.insert(id, result.map_err(|error| error.message().to_string()));
        }
    }

    pub async fn package(&self, id: PackageId) -> Result<Arc<LoadedPackage>, QueryError> {
        self.load([id]).await;
        let packages = self.packages.lock().unwrap_or_else(|p| p.into_inner());
        match packages.get(&id) {
            Some(Ok(package)) => Ok(package.clone()),
            Some(Err(error)) => Err(QueryError(error.clone())),
            None => Err(QueryError(format!("package {id} wasn't loaded"))),
        }
    }

    /// The target a label names. Unless keeping going, its package must
    /// have loaded without errors.
    pub async fn target(&self, label: Label) -> Result<TargetRef, QueryError> {
        let package = self.package(label.package).await?;
        let Ok(index) = package.0.targets.binary_search_by(|target| target.name().cmp(&label.name)) else {
            let build_file = self.build_file(&package.0).await?;
            return Err(QueryError(bazel_loading::no_such_target(label, &build_file)));
        };
        if !package.0.errors.is_empty() && !self.options.keep_going {
            return Err(QueryError(format!(
                "package contains errors: {}: {}",
                bazel_loading::display_package(label.package),
                package.0.errors[0]
            )));
        }
        Ok(TargetRef { package, index })
    }

    /// The directory a package is in.
    pub async fn package_dir(&self, package: PackageId) -> Result<PathBuf, QueryError> {
        let root = self.compute.compute(&RepoRootKey(package.repo)).await?;
        let dir = RootedPath::new(root.0, package.path).resolve_in(&self.workspace_root);
        Ok(dir)
    }

    /// Where a package's BUILD file is.
    pub async fn build_file(&self, package: &Package) -> Result<PathBuf, QueryError> {
        let dir = self.package_dir(package.id).await?;
        Ok(dir.join(package.build_file.name.as_str()))
    }

    /// The errors of the packages loaded that had them, each as Bazel
    /// reports it: `/path/to/BUILD:line:column: message`.
    pub async fn package_errors(&self) -> Vec<String> {
        let failed: Vec<Arc<LoadedPackage>> = {
            let packages = self.packages.lock().unwrap_or_else(|p| p.into_inner());
            let mut failed: Vec<Arc<LoadedPackage>> = packages.values().filter_map(|package| package.as_ref().ok()).filter(|package| !package.0.errors.is_empty()).cloned().collect();
            failed.sort_by_key(|package| package.0.id);
            failed
        };
        let mut errors = Vec::new();
        for package in failed {
            let Ok(build_file) = self.build_file(&package.0).await else {
                continue;
            };
            for error in &package.0.errors {
                let path = build_file.display().to_string();
                errors.push(error.report(&|_| path.clone()));
            }
        }
        errors
    }

    /// The targets of `labels` that exist, recording the others as errors.
    pub async fn targets(&self, labels: &TargetSet) -> Result<Vec<TargetRef>, QueryError> {
        self.load(labels.iter().map(|label| label.package)).await;
        let mut found = Vec::with_capacity(labels.len());
        for label in labels {
            match self.target(*label).await {
                Ok(target) => found.push(target),
                Err(error) => self.error(error.0)?,
            }
        }
        Ok(found)
    }

    pub async fn rule_class(&self, class: &RuleClassRef) -> Result<Arc<RuleClass>, QueryError> {
        if let Some(found) = self.classes.lock().unwrap_or_else(|p| p.into_inner()).get(class) {
            return Ok(found.clone());
        }
        let loaded = rule_class(&self.compute, class).await?;
        self.classes.lock().unwrap_or_else(|p| p.into_inner()).insert(class.clone(), loaded.clone());
        Ok(loaded)
    }

    /// The labels a target depends on directly.
    pub async fn deps_of(&self, target: &TargetRef) -> Result<Vec<Label>, QueryError> {
        match target.target() {
            Target::Rule(rule) => {
                let class = self.rule_class(target.package().rule_class(rule)).await?;
                let mut deps = rule_deps(rule, &class, &self.options);
                package_default_deps(target.package(), rule, class.kind, &mut deps);
                deps.sort();
                deps.dedup();
                Ok(deps)
            }
            Target::OutputFile { rule, .. } => Ok(vec![target.package().label(*rule)]),
            Target::SourceFile(_) => Ok(Vec::new()),
            Target::PackageGroup(group) => Ok(group.includes.clone()),
        }
    }

    /// The targets a pattern names.
    pub async fn resolve(&self, text: &str) -> Result<TargetSet, QueryError> {
        let context = LabelContext {
            package: self.working_package,
            mapping: &*self.main_mapping,
            lenient: false,
        };
        let pattern = TargetPattern::parse(text, &context).map_err(|error| QueryError(error.to_string()))?;
        match pattern {
            TargetPattern::Target(label) => {
                self.target(label).await?;
                Ok(TargetSet::from([label]))
            }
            TargetPattern::Package { package, rules_only } => {
                let loaded = self.package(package).await?;
                self.check_errors(&loaded)?;
                Ok(package_targets(&loaded.0, rules_only))
            }
            TargetPattern::Recursive { repo, prefix, rules_only } => {
                let ids = packages_beneath(&self.compute, repo, prefix).await?;
                if ids.is_empty() {
                    return Err(QueryError(format!("no targets found beneath '{}'", prefix)));
                }
                self.load(ids.iter().copied()).await;
                let mut targets = TargetSet::new();
                for id in ids {
                    match self.package(id).await {
                        Ok(loaded) => {
                            self.check_errors(&loaded)?;
                            targets.extend(package_targets(&loaded.0, rules_only));
                        }
                        Err(error) => self.error(error.0)?,
                    }
                }
                Ok(targets)
            }
        }
    }

    fn check_errors(&self, loaded: &LoadedPackage) -> Result<(), QueryError> {
        let Some(error) = loaded.0.errors.first() else {
            return Ok(());
        };
        self.error(error.to_string())
    }

    /// The `.bzl` files a package loads, directly and through other `.bzl`
    /// files.
    pub fn loaded_files<'a>(&'a self, roots: Vec<Label>, seen: &'a mut BTreeSet<Label>) -> BoxFuture<'a, Result<(), QueryError>> {
        Box::pin(async move {
            let new: Vec<BzlLoadKey> = roots.into_iter().filter(|label| seen.insert(*label)).map(BzlLoadKey).collect();
            if new.is_empty() {
                return Ok(());
            }
            let mut next = Vec::new();
            for module in self.compute.compute_many(&new).await {
                next.extend(module?.loads.iter().copied());
            }
            self.loaded_files(next, seen).await
        })
    }
}

fn package_targets(package: &Package, rules_only: bool) -> TargetSet {
    package
        .targets
        .iter()
        .filter(|target| !rules_only || matches!(target, Target::Rule(_)))
        .map(|target| package.label(target.name()))
        .collect()
}

/// A rule's direct dependencies: the labels of its dependency attributes,
/// given or (with implicit deps) defaulted.
pub fn rule_deps(rule: &Rule, class: &RuleClass, options: &QueryOptions) -> Vec<Label> {
    let mut labels = Vec::new();
    for attr in &class.attrs {
        if !follows(attr, options) {
            continue;
        }
        match rule.attr(attr.name) {
            Some(value) => value.labels(&mut labels),
            None if options.implicit_deps => match rule.attr_or_computed(attr.name) {
                Some(value) => value.labels(&mut labels),
                None => default_labels(attr, &mut labels),
            },
            None => {}
        }
    }
    for (attr, value) in &rule.attrs {
        if class.attr(*attr).is_none() {
            value.labels(&mut labels);
        }
    }
    labels.extend(class.toolchain_types.iter().copied());
    let mut conditions = Vec::new();
    for (_, value) in &rule.attrs {
        value.conditions(&mut conditions);
    }
    labels.extend(conditions.into_iter().filter(|label| label.to_string() != bazel_packages::DEFAULT_CONDITION));
    labels.sort();
    labels.dedup();
    labels
}

/// Rules used to resolve dependencies, which package defaults such as
/// licenses don't apply to.
const DEPENDENCY_RESOLUTION_RULES: [&str; 7] = [
    "config_setting",
    "constraint_setting",
    "constraint_value",
    "platform",
    "toolchain",
    "toolchain_type",
    "alias",
];

/// The dependencies a rule gets from its package's defaults: the
/// licenses `package(default_applicable_licenses = ...)` applies to rules
/// that don't set their own, other than the licenses themselves and rules
/// used to resolve dependencies.
fn package_default_deps(package: &Package, rule: &Rule, kind: intern::Symbol, deps: &mut Vec<Label>) {
    let defaults = &package.args.default_applicable_licenses;
    if defaults.is_empty() || DEPENDENCY_RESOLUTION_RULES.contains(&kind.as_str()) {
        return;
    }
    if defaults.contains(&package.label(rule.name)) {
        return;
    }
    if rule.attr(intern::Symbol::intern("applicable_licenses")).is_none() {
        deps.extend(defaults.iter().copied());
    }
}

fn follows(attr: &AttrSchema, options: &QueryOptions) -> bool {
    if !attr.is_dependency || attr.name.as_str() == "visibility" {
        return false;
    }
    options.tool_deps || attr.cfg != Cfg::Exec
}

fn default_labels(attr: &AttrSchema, labels: &mut Vec<Label>) {
    match &attr.default {
        AttrDefault::Value(value) => value.labels(labels),
        AttrDefault::LateBound(Some(label)) => labels.push(*label),
        AttrDefault::LateBound(None) | AttrDefault::Computed => {}
    }
}

/// An attribute's value for a rule: given, or the default.
pub fn attr_value<'a>(rule: &'a Rule, schema: &'a AttrSchema) -> Option<&'a AttrValue> {
    if let Some(value) = rule.attr(schema.name) {
        return Some(value);
    }
    match &schema.default {
        AttrDefault::Value(value) => Some(value),
        _ => None,
    }
}
