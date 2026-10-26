//! The targets a query reads, loaded through the build graph, and the
//! dependency edges between them.

use bazel_label::{Label, PackageId, RepoMappingTable, TargetPattern, LabelContext};
use bazel_fs::RootedPath;
use bazel_loading::{BzlLoadKey, LoadedPackage, PackageKey, RepoRootKey, packages_beneath, rule_class};
use bazel_packages::{AttrDefault, AttrSchema, AttrValue, Cfg, Package, PackageError, Rule, RuleClass, RuleClassRef, Target};
use futures::future::BoxFuture;
use skyframe::{Compute, SkyError};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Errors to report, in the order they were found.
    reports: Mutex<Vec<String>>,
    /// Whether there were errors that keeping going went past.
    kept_going: AtomicBool,
    /// Whether a pattern named targets of a package with errors, which
    /// fails the query once it's evaluated, unless keeping going.
    in_error: AtomicBool,
    /// The error of the first pattern naming targets of a package with
    /// errors, which fails a build.
    pattern_failure: Mutex<Option<String>>,
    /// The packages whose errors have been reported.
    reported: Mutex<HashSet<PackageId>>,
    /// Whether the transitive closure being computed met errors.
    closure_failed: AtomicBool,
    /// Why evaluating the query failed, if it failed as Bazel puts it:
    /// "Evaluation of query failed", with the detail after a colon.
    evaluation_failure: Mutex<Option<Option<String>>>,
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
            reports: Mutex::new(Vec::new()),
            kept_going: AtomicBool::new(false),
            in_error: AtomicBool::new(false),
            pattern_failure: Mutex::new(None),
            reported: Mutex::new(HashSet::new()),
            closure_failed: AtomicBool::new(false),
            evaluation_failure: Mutex::new(None),
        }
    }

    pub fn compute(&self) -> &C {
        &self.compute
    }

    /// The errors found, other than the packages' own, in the order they
    /// were found.
    pub fn reports(&self) -> Vec<String> {
        self.reports.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Whether keeping going went past errors.
    pub fn kept_going(&self) -> bool {
        self.kept_going.load(Ordering::Relaxed)
    }

    /// Why evaluating the query failed, if it failed as "Evaluation of
    /// query failed", and the detail Bazel gives after it, if any: a
    /// pattern naming targets of a package with errors fails it, unless
    /// keeping going, once the query has been evaluated.
    pub fn evaluation_failure(&self) -> Option<Option<String>> {
        let failure = self.evaluation_failure.lock().unwrap_or_else(|p| p.into_inner()).clone();
        match failure {
            Some(failure) => Some(failure),
            None => self.in_error.load(Ordering::Relaxed).then_some(None),
        }
    }

    /// The error of the first pattern resolved since this was last asked
    /// that named targets of a package with errors, as a build fails with
    /// it.
    pub fn pattern_failure(&self) -> Option<String> {
        self.pattern_failure.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    fn report(&self, message: String) {
        self.reports.lock().unwrap_or_else(|p| p.into_inner()).push(message);
    }

    /// Note that evaluating the query failed, with `detail`. The first
    /// failure is the one reported.
    fn fail_evaluation(&self, detail: Option<String>) {
        let mut failure = self.evaluation_failure.lock().unwrap_or_else(|p| p.into_inner());
        if failure.is_none() {
            *failure = Some(detail);
        }
    }

    /// Fail with `message`, unless keeping going, in which case it's
    /// reported as skipped: why, after the errors that led to it, which
    /// [`Universe::errors`] gives.
    pub fn error(&self, pattern: &str, message: String) -> Result<(), QueryError> {
        if !self.options.keep_going {
            return Err(QueryError(message));
        }
        let (_, message) = bazel_loading::split_reported(&message);
        self.report(format!("Skipping '{pattern}': {message}"));
        self.kept_going.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Note an error that doesn't fail the query when keeping going, which
    /// has been reported.
    fn kept_going_past(&self) -> Result<(), ()> {
        match self.options.keep_going {
            true => {
                self.kept_going.store(true, Ordering::Relaxed);
                Ok(())
            }
            false => Err(()),
        }
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

    /// The target a label names, which may be in a package with errors.
    pub async fn target(&self, label: Label) -> Result<TargetRef, QueryError> {
        let package = self.package(label.package).await?;
        self.report_package(&package.0).await;
        let Ok(index) = package.0.targets.binary_search_by(|target| target.name().cmp(&label.name)) else {
            let build_file = self.build_file(&package.0).await?;
            return Err(QueryError(bazel_loading::no_such_target(label, &build_file)));
        };
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

    /// The errors to report, in order: those of packages loaded that no
    /// pattern or dependency led to, then those found.
    pub async fn errors(&self) -> Vec<String> {
        let (loaded, unloaded) = {
            let packages = self.packages.lock().unwrap_or_else(|p| p.into_inner());
            let mut failed: Vec<Arc<LoadedPackage>> = packages.values().filter_map(|package| package.as_ref().ok()).filter(|package| !package.0.errors.is_empty()).cloned().collect();
            failed.sort_by_key(|package| package.0.id);
            let mut unloaded: Vec<(PackageId, String)> = packages.iter().filter_map(|(id, package)| Some((*id, package.as_ref().err()?.clone()))).collect();
            unloaded.sort();
            (failed, unloaded)
        };
        let mut errors = Vec::new();
        for (id, message) in unloaded {
            let (reported, _) = bazel_loading::split_reported(&message);
            if !reported.is_empty() && self.reported.lock().unwrap_or_else(|p| p.into_inner()).insert(id) {
                errors.push(reported.to_string());
            }
        }
        for package in loaded {
            if self.reported.lock().unwrap_or_else(|p| p.into_inner()).insert(package.0.id) {
                errors.extend(self.package_errors(&package.0).await);
            }
        }
        errors.extend(self.reports());
        errors
    }

    /// Report the errors of `package` as Bazel does when it's loaded, the
    /// first time it's used.
    async fn report_package(&self, package: &Package) {
        if package.errors.is_empty() || !self.reported.lock().unwrap_or_else(|p| p.into_inner()).insert(package.id) {
            return;
        }
        for error in self.package_errors(package).await {
            self.report(error);
        }
    }

    /// The errors of `package`, with files named by paths.
    async fn package_errors(&self, package: &Package) -> Vec<String> {
        let mut errors = Vec::with_capacity(package.errors.len());
        for error in &package.errors {
            let paths = self.paths(error).await;
            errors.push(error.report(&|file| paths.get(file).cloned().unwrap_or_else(|| file.to_string())));
        }
        errors
    }

    /// The paths of the files `error` names, by their labels.
    async fn paths(&self, error: &PackageError) -> HashMap<String, String> {
        let mut paths = HashMap::new();
        for file in error.files() {
            let Ok(label) = Label::parse_canonical(file) else {
                continue;
            };
            let Ok(dir) = self.package_dir(label.package).await else {
                continue;
            };
            paths.insert(file.to_string(), dir.join(label.name.as_str()).display().to_string());
        }
        paths
    }

    /// Report that `package` contains errors, quoting its first, as
    /// Bazel does when a pattern names its targets.
    async fn report_package_contains_errors(&self, package: &Package) {
        let Some(error) = package.errors.first() else {
            return;
        };
        let paths = self.paths(error).await;
        let described = error.describe(&|file| paths.get(file).cloned().unwrap_or_else(|| file.to_string()));
        self.report(format!("package contains errors: {}: {described}", bazel_loading::display_package(package.id)));
    }

    /// Report that a package a recursive pattern reached contains errors,
    /// as Bazel does without saying which when keeping going, or when the
    /// package defines targets despite its errors.
    fn report_package_in_recursion(&self, package: &Package) {
        let defines_targets = package.targets.iter().any(|target| target.name() != package.build_file.name);
        if self.options.keep_going || defines_targets {
            self.report(format!("package contains errors: {}", bazel_loading::display_package(package.id)));
        }
    }

    /// The error of a pattern naming targets of a package with errors.
    fn pattern_in_error(pattern: &str, package: PackageId) -> QueryError {
        let package = bazel_loading::display_package(package);
        QueryError(format!("Error evaluating '{pattern}': error loading package '{package}': Package '{package}' contains errors"))
    }

    /// Note a pattern naming targets of `package`, which has errors: a
    /// query fails once evaluated, unless keeping going, and a build fails.
    fn package_in_error(&self, pattern: &str, package: PackageId) {
        let mut failure = self.pattern_failure.lock().unwrap_or_else(|p| p.into_inner());
        if failure.is_none() {
            *failure = Some(Self::pattern_in_error(pattern, package).0);
        }
        drop(failure);
        if self.kept_going_past().is_err() {
            self.in_error.store(true, Ordering::Relaxed);
        }
    }

    /// The targets of `labels` that exist, recording the others as errors.
    pub async fn targets(&self, labels: &TargetSet) -> Result<Vec<TargetRef>, QueryError> {
        self.load(labels.iter().map(|label| label.package)).await;
        let mut found = Vec::with_capacity(labels.len());
        for label in labels {
            match self.target(*label).await {
                Ok(target) => found.push(target),
                Err(error) => self.error(&label.to_string(), error.0)?,
            }
        }
        Ok(found)
    }

    /// The targets of `labels` that exist, as a step of a transitive
    /// closure, in which each label but the roots was reached from the
    /// rule `referrers` gives. Errors are reported as Bazel reports them:
    /// at the rule referring to the target, and failing the closure.
    pub async fn closure_targets(&self, labels: &TargetSet, referrers: &HashMap<Label, TargetRef>) -> Result<Vec<TargetRef>, QueryError> {
        self.load(labels.iter().map(|label| label.package)).await;
        let mut found = Vec::with_capacity(labels.len());
        for label in labels {
            let referrer = referrers.get(label);
            match self.target(*label).await {
                Ok(target) => {
                    if !target.package().errors.is_empty() {
                        self.closure_target_in_error(*label, referrer).await;
                    }
                    found.push(target);
                }
                Err(error) => self.closure_target_missing(*label, referrer, error).await?,
            }
        }
        Ok(found)
    }

    async fn closure_target_in_error(&self, label: Label, referrer: Option<&TargetRef>) {
        if let Some(referrer) = referrer {
            let at = self.referrer_position(referrer).await;
            self.report(format!(
                "{at}Target '{label}' contains an error and its package is in error and referenced by '{}'",
                referrer.label()
            ));
        }
        self.closure_failed.store(true, Ordering::Relaxed);
    }

    async fn closure_target_missing(&self, label: Label, referrer: Option<&TargetRef>, error: QueryError) -> Result<(), QueryError> {
        let Some(referrer) = referrer else {
            return self.error(&label.to_string(), error.0);
        };
        let at = self.referrer_position(referrer).await;
        self.report(format!("{at}{} and referenced by '{}'", error.0, referrer.label()));
        if self.kept_going_past().is_ok() {
            self.closure_failed.store(true, Ordering::Relaxed);
            return Ok(());
        }
        self.fail_evaluation(Some(format!("preloading transitive closure failed: {}", error.0)));
        Err(error)
    }

    /// Fail a transitive closure in which there were errors: when keeping
    /// going, the evaluation fails but its result is still printed.
    pub fn finish_closure(&self) -> Result<(), QueryError> {
        if !self.closure_failed.swap(false, Ordering::Relaxed) {
            return Ok(());
        }
        let detail = "errors were encountered while computing transitive closure";
        self.fail_evaluation(Some(detail.to_string()));
        match self.kept_going_past() {
            Ok(()) => Ok(()),
            Err(()) => Err(QueryError(detail.to_string())),
        }
    }

    /// `path:line:column: ` for where a rule referring to a target is,
    /// or `path: ` for another kind of target.
    async fn referrer_position(&self, referrer: &TargetRef) -> String {
        let path = match self.build_file(referrer.package()).await {
            Ok(path) => path.display().to_string(),
            Err(_) => referrer.package().build_file.to_string(),
        };
        match referrer.target() {
            Target::Rule(rule) if rule.location.line > 0 => format!("{path}:{}:{}: ", rule.location.line, rule.location.column),
            _ => format!("{path}: "),
        }
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
                let target = match self.target(label).await {
                    Ok(target) => target,
                    Err(error) => {
                        self.error(text, error.0)?;
                        return Ok(TargetSet::new());
                    }
                };
                if !target.package().errors.is_empty() {
                    self.package_in_error(text, label.package);
                }
                Ok(TargetSet::from([label]))
            }
            TargetPattern::Package { package, rules_only } => {
                let loaded = self.package(package).await?;
                if !loaded.0.errors.is_empty() {
                    self.report_package(&loaded.0).await;
                    self.report_package_contains_errors(&loaded.0).await;
                    self.package_in_error(text, package);
                }
                Ok(package_targets(&loaded.0, rules_only))
            }
            TargetPattern::Recursive { repo, prefix, rules_only } => {
                let ids = packages_beneath(&self.compute, repo, prefix).await?;
                if ids.is_empty() {
                    return Err(QueryError(format!("no targets found beneath '{}'", prefix)));
                }
                self.load(ids.iter().copied()).await;
                let mut targets = TargetSet::new();
                let mut in_error = Vec::new();
                for id in ids {
                    match self.package(id).await {
                        Ok(loaded) => {
                            targets.extend(package_targets(&loaded.0, rules_only));
                            if !loaded.0.errors.is_empty() {
                                self.report_package(&loaded.0).await;
                                self.report_package_in_recursion(&loaded.0);
                                in_error.push(loaded);
                            }
                        }
                        Err(error) => self.error(text, error.0)?,
                    }
                }
                for loaded in &in_error {
                    self.report_package_contains_errors(&loaded.0).await;
                }
                let Some(first) = in_error.first().map(|loaded| loaded.0.id) else {
                    return Ok(targets);
                };
                match self.kept_going_past() {
                    Ok(()) => Ok(targets),
                    Err(()) => Err(Self::pattern_in_error(text, first)),
                }
            }
        }
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
