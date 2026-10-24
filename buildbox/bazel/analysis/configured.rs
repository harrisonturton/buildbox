//! Configured targets: targets analyzed in a configuration.

use crate::action::{Action, StoredAction};
use crate::paging::PagedValue;
use crate::artifact::{self, Artifact};
use crate::attrs::resolve_attrs;
use crate::build_ctx::{self, Dep, RuleInput};
use crate::platform::PlatformKey;
use crate::providers;
use crate::toolchains::{self, ResolvedToolchains};
use bazel_config::ConfigId;
use bazel_label::Label;
use bazel_loading::{BzlLoadKey, BzlModule, PackageKey, RepoMappingKey};
use bazel_progress::{Event, Progress};
use bazel_packages::{AttrType, AttrValue, Cfg, Package, Rule, RuleClass, RuleClassRef, Target};
use intern::{PathId, Symbol};
use skycodec::{Decode, Encode, Fingerprint, Fingerprinter};
use skyframe::{Ctx, Key, SkyError, Storage, Value};
use starlark::environment::{FrozenModule, Module};
use starlark::values::{FrozenHeapName, OwnedFrozenValue, ValueLike};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A target in a configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct ConfiguredTargetKey {
    pub label: Label,
    pub config: ConfigId,
}

/// An analyzed target: its providers, and the actions that generate its
/// files.
///
/// The providers are a live Starlark value, which dependents' providers
/// refer to, so configured targets are held as they are rather than
/// encoded.
pub struct ConfiguredTarget {
    pub label: Label,
    pub config: ConfigId,
    /// The kind of rule, such as `genrule`, or `source file`.
    pub kind: Symbol,
    pub is_test: bool,
    /// The [`TargetValue`](crate::TargetValue) dependents see, which may
    /// be paged out; see [`ConfiguredTarget::providers`].
    value: PagedValue,
    pub actions: Vec<StoredAction>,
    /// Which action generates each output, by exec path, sorted.
    pub generating: Vec<(PathId, u32)>,
    /// Identifies what the target was analyzed from.
    pub fingerprint: Fingerprint,
    /// Why the target can't be built for the target platform, if it can't.
    /// Such a target has no providers or actions.
    pub incompatible: Option<Arc<Incompatibility>>,
    size: usize,
}

/// Why a target is incompatible with the target platform.
#[derive(Debug)]
pub struct Incompatibility {
    /// The target, then the dependencies leading to the one whose
    /// `target_compatible_with` the platform doesn't satisfy.
    pub chain: Vec<ConfiguredTargetKey>,
    /// The constraint the platform doesn't have.
    pub constraint: Label,
}

impl Value for ConfiguredTarget {
    fn to_storage(&self) -> Storage {
        Storage::Live {
            fingerprint: self.fingerprint,
            size: self.size,
        }
    }

    fn from_bytes(_bytes: &[u8]) -> Result<ConfiguredTarget, skycodec::DecodeError> {
        Err(skycodec::DecodeError::Invalid {
            what: "live configured target",
            value: 0,
            position: 0,
        })
    }
}

impl ConfiguredTarget {
    /// The action that generates an output, if this target has one.
    pub fn generating_action(&self, output: PathId) -> Option<(u32, &StoredAction)> {
        let index = self.generating.binary_search_by(|(path, _)| path.cmp(&output)).ok()?;
        let action = self.generating[index].1;
        Some((action, &self.actions[action as usize]))
    }

    /// The target's providers, read back if they're paged out.
    pub fn providers(&self) -> Result<Providers, SkyError> {
        self.value.get().map(Providers).map_err(|error| SkyError::new(format!("{}: reading back its providers: {error:#}", self.label)))
    }

    /// The target's providers' value, for paging them out.
    pub fn paged_value(&self) -> &PagedValue {
        &self.value
    }
}

/// A target's providers, and the frozen heap they're in.
pub struct Providers(OwnedFrozenValue);

impl Providers {
    /// The providers, as a [`TargetValue`](crate::TargetValue).
    pub fn get(&self) -> Option<&crate::target_value::TargetValue<'_>> {
        crate::target_value::TargetValue::from_value(self.0.value())
    }

    /// The value and the heap it's in, to use in another heap.
    pub fn owned(&self) -> &OwnedFrozenValue {
        &self.0
    }
}

impl Key for ConfiguredTargetKey {
    type Value = ConfiguredTarget;
    const KIND: &'static str = "CONFIGURED_TARGET";

    async fn compute(&self, ctx: &Ctx) -> Result<ConfiguredTarget, SkyError> {
        let configured = configure(ctx, *self).await;
        if let Ok(progress) = ctx.global::<Progress>() {
            progress.target_configured();
        }
        configured
    }
}

/// Analyze the target `key` names, in its configuration.
async fn configure(ctx: &Ctx, key: ConfiguredTargetKey) -> Result<ConfiguredTarget, SkyError> {
    if key.label == *crate::workspace_status::OWNER {
        return workspace_status(ctx, key);
    }
    let package = ctx.compute(&PackageKey(key.label.package)).await?;
    let Some(target) = package.0.target(key.label.name) else {
        return Err(SkyError::new(no_such_target(ctx, key.label, &package.0).await));
    };
    match target {
        Target::Rule(rule) => analyze_rule(ctx, key, &package.0, rule).await,
        Target::SourceFile(_) => file(ctx, key, "source file", Artifact::source(key.label)),
        Target::OutputFile { rule, .. } => {
            let owner = ConfiguredTargetKey {
                label: key.label.sibling(*rule),
                config: key.config,
            };
            ctx.compute(&owner).await?;
            let artifact = Artifact::derived(owner.label, &key.config.get(), artifact::name_path(key.label), false);
            file(ctx, key, "generated file", artifact)
        }
        Target::PackageGroup(group) => package_group(ctx, key, group).await,
    }
}

/// The error for a target its package doesn't declare, naming the
/// package's BUILD file by its path.
async fn no_such_target(ctx: &Ctx, label: Label, package: &Package) -> String {
    let build_file = match bazel_loading::build_file_path(ctx, label.package).await {
        Ok(Some(path)) => path,
        _ => std::path::PathBuf::from(package.build_file.to_string()),
    };
    bazel_loading::no_such_target(label, &build_file)
}

/// What implements a rule class.
pub enum Implementation {
    /// A function in a `.bzl` file.
    Starlark { module: Arc<BzlModule>, implementation: OwnedFrozenValue },
    /// A rule of Bazel's builtins, defined in `file`, whose Starlark
    /// implementation bb runs.
    Builtin { file: Label, implementation: OwnedFrozenValue },
    /// Built into Bazel, by kind.
    Native(Symbol),
}

async fn analyze_rule(ctx: &Ctx, key: ConfiguredTargetKey, package: &Package, rule: &Rule) -> Result<ConfiguredTarget, SkyError> {
    let label = key.label;
    if !package.errors.is_empty() {
        return Err(SkyError::new(format!(
            "target '{label}' contains an error and its package is in error"
        )));
    }
    let class_ref = package.rule_class(rule).clone();
    if class_ref.definition.is_none() && class_ref.kind.as_str() == "alias" {
        return alias(ctx, key, rule).await;
    }
    let class = bazel_loading::rule_class(ctx, &class_ref).await?;
    let attrs = resolve_attrs(ctx, rule, &class, key.config).await?;
    if let Some(constraint) = unsatisfied_constraint(ctx, &attrs, key.config).await? {
        let reason = Incompatibility { chain: vec![key], constraint };
        return incompatible(ctx, key, class_ref.kind, class.test, reason);
    }
    let implementation = implementation(ctx, &class_ref)
        .await
        .map_err(|error| format!("in {} rule {label}: {}", class_ref.kind, error.message()));
    let implementation = match implementation {
        Ok(implementation) => implementation,
        Err(message) => return Err(located(ctx, label, message).await),
    };
    let deps = match is_stub(&implementation) {
        true => BTreeMap::new(),
        false => analyze_deps(ctx, label, &class, &attrs, key.config).await?,
    };
    if let Some(reason) = incompatible_dep(key, &deps) {
        return incompatible(ctx, key, class_ref.kind, class.test, reason);
    }
    let config = key.config.get();
    let platform = ctx.compute(&PlatformKey(config.options.platform)).await?;
    let toolchains = toolchains::resolve(ctx, &class, &config).await?;
    let mapping = ctx.compute(&RepoMappingKey(label.repo())).await?;
    let mappings = match &implementation {
        Implementation::Starlark { module, .. } => bazel_loading::mappings(ctx, &module.repos).await?,
        Implementation::Builtin { .. } | Implementation::Native(_) => Default::default(),
    };
    let fingerprint = fingerprint(key, rule, &implementation, &deps, &toolchains);

    let input = RuleInput {
        label,
        config,
        class: class.clone(),
        attrs,
        deps,
        build_file: package.build_file,
        implementation,
        mapping: Arc::new(mapping.0.clone()),
        mappings,
        target_constraints: platform.values(),
        toolchains,
    };
    let name = bazel_starlark::heap_name("target", key.label, fingerprint);
    let analyzed = bazel_loading::blocking(move || evaluate(input, name)).await?;
    let analyzed = match analyzed {
        Ok(analyzed) => analyzed,
        Err(message) => return Err(located(ctx, label, message).await),
    };
    Ok(configured(ctx, key, class_ref.kind, class.test, analyzed, fingerprint))
}

/// An error in analyzing a rule, at where the rule was declared, as Bazel
/// reports it.
async fn located(ctx: &Ctx, label: Label, message: String) -> SkyError {
    match bazel_loading::target_location(ctx, label).await {
        Some(location) => SkyError::new(format!("{location}: {message}")),
        None => SkyError::new(message),
    }
}

/// The first constraint of the rule's `target_compatible_with` that the
/// target platform doesn't have.
async fn unsatisfied_constraint(ctx: &Ctx, attrs: &[(Symbol, AttrValue)], config: ConfigId) -> Result<Option<Label>, SkyError> {
    let name = Symbol::intern("target_compatible_with");
    let Some((_, AttrValue::LabelList(constraints))) = attrs.iter().find(|(attr, _)| *attr == name) else {
        return Ok(None);
    };
    let config = config.get();
    for constraint in constraints {
        let constraint = crate::platform::actual(ctx, *constraint).await?;
        if !crate::condition::target_platform_has(ctx, &config, constraint).await? {
            return Ok(Some(constraint));
        }
    }
    Ok(None)
}

/// A target is incompatible when any of its dependencies is.
fn incompatible_dep(key: ConfiguredTargetKey, deps: &BTreeMap<(Label, ConfigId), Dep>) -> Option<Incompatibility> {
    let reason = deps.values().find_map(|dep| match dep {
        Dep::Target(target) => target.incompatible.clone(),
        Dep::File(..) => None,
    })?;
    let mut chain = vec![key];
    chain.extend(reason.chain.iter().copied());
    Some(Incompatibility {
        chain,
        constraint: reason.constraint,
    })
}

/// Whether a rule is one bb stands in for with a rule that provides
/// nothing, whose dependencies needn't be analyzed.
fn is_stub(implementation: &Implementation) -> bool {
    matches!(implementation, Implementation::Native(kind) if crate::native::STUBBED_BUILTINS.contains(&kind.as_str()))
}

/// The configuration a dependency through an attribute is analyzed in.
fn dep_config(cfg: Cfg, config: ConfigId) -> ConfigId {
    match cfg {
        Cfg::Exec => config.get().exec(),
        Cfg::Target | Cfg::Transition => config,
    }
}

/// Analyze every dependency the rule's attributes name.
async fn analyze_deps(
    ctx: &Ctx,
    referrer: Label,
    class: &RuleClass,
    attrs: &[(Symbol, AttrValue)],
    config: ConfigId,
) -> Result<BTreeMap<(Label, ConfigId), Dep>, SkyError> {
    let mut wanted = Vec::new();
    for (schema, (_, value)) in class.attrs.iter().zip(attrs) {
        if !schema.is_dependency || !schema.ty.has_labels() || schema.ty == AttrType::Output || is_late_bound(schema.name) {
            continue;
        }
        let mut labels = Vec::new();
        value.labels(&mut labels);
        let dep_config = dep_config(schema.cfg, config);
        wanted.extend(labels.into_iter().filter(|label| !label.repo().is_unknown()).map(|label| (label, dep_config)));
        for label in labels_of_unknown_repos(value) {
            return Err(SkyError::new(format!(
                "attribute '{}': no repository visible as '@{}'",
                schema.name,
                label.repo().as_str().rsplit('+').next().unwrap_or_default()
            )));
        }
    }
    wanted.sort();
    wanted.dedup();
    let results = futures::future::join_all(wanted.iter().map(|(label, config)| dep(ctx, *label, *config))).await;
    let mut deps = BTreeMap::new();
    for (wanted, result) in wanted.into_iter().zip(results) {
        match result {
            Ok(dep) => deps.insert(wanted, dep),
            Err(DepError::Failed(error)) => return Err(error),
            Err(DepError::Missing(message)) => return Err(missing_dep(ctx, referrer, &message).await),
        };
    }
    Ok(deps)
}

/// Why a dependency can't be had.
enum DepError {
    /// It doesn't exist, as this message says.
    Missing(String),
    /// It failed to analyze.
    Failed(SkyError),
}

/// The error for a dependency that doesn't exist, as Bazel reports it:
/// where the target that refers to it was declared, and that it's
/// referred to.
async fn missing_dep(ctx: &Ctx, referrer: Label, message: &str) -> SkyError {
    match bazel_loading::target_location(ctx, referrer).await {
        Some(location) => SkyError::new(format!("{location}: {message} and referenced by '{referrer}'")),
        None => SkyError::new(format!("{message} and referenced by '{referrer}'")),
    }
}

/// Whether an attribute is late-bound, as native attributes named `:x`
/// are: chosen by options such as `--collect_code_coverage`, which bb
/// doesn't support, so they have no value.
fn is_late_bound(name: Symbol) -> bool {
    name.as_str().starts_with(':')
}

fn labels_of_unknown_repos(value: &AttrValue) -> Vec<Label> {
    let mut labels = Vec::new();
    value.labels(&mut labels);
    labels.retain(|label| label.repo().is_unknown());
    labels
}

/// One dependency: a file, or an analyzed rule.
async fn dep(ctx: &Ctx, label: Label, config: ConfigId) -> Result<Dep, DepError> {
    let package = match ctx.compute(&PackageKey(label.package)).await {
        Ok(package) => package,
        Err(error) => return Err(DepError::Missing(error.message().to_string())),
    };
    let analyzed = |key: ConfiguredTargetKey| async move { ctx.compute(&key).await.map_err(DepError::Failed) };
    match package.0.target(label.name) {
        None => Err(DepError::Missing(report_no_such_target(ctx, label, &package.0).await)),
        Some(Target::SourceFile(_)) => Ok(Dep::File(label, Artifact::source(label))),
        Some(Target::OutputFile { rule, .. }) => {
            let owner = label.sibling(*rule);
            analyzed(ConfiguredTargetKey { label: owner, config }).await?;
            Ok(Dep::File(label, Artifact::derived(owner, &config.get(), artifact::name_path(label), false)))
        }
        Some(_) => Ok(Dep::Target(analyzed(ConfiguredTargetKey { label, config }).await?)),
    }
}

/// The error for a dependency its package doesn't declare, reported, as
/// Bazel does, at that package's BUILD file.
async fn report_no_such_target(ctx: &Ctx, label: Label, package: &Package) -> String {
    let message = no_such_target(ctx, label, package).await;
    let build_file = bazel_loading::build_file_path(ctx, label.package).await;
    if let (Ok(progress), Ok(Some(build_file))) = (ctx.global::<Progress>(), build_file) {
        progress.report(Event::Error(format!("{}: {message}", build_file.display())));
    }
    message
}

/// The function that implements a rule class.
async fn implementation(ctx: &Ctx, class: &RuleClassRef) -> Result<Implementation, SkyError> {
    let Some((bzl, name)) = class.definition else {
        return Ok(Implementation::Native(class.kind));
    };
    let native = crate::native::STUBBED_BUILTINS.contains(&class.kind.as_str()) || crate::cc::NATIVE_RULES.contains(&class.kind.as_str());
    if bzl.repo().as_str() == bazel_loading::BUILTINS_REPO && native {
        return Ok(Implementation::Native(class.kind));
    }
    if bzl.repo().as_str() == bazel_loading::BUILTINS_REPO && crate::native::STARLARK_BUILTINS.contains(&class.kind.as_str()) {
        let builtins = ctx.compute(&bazel_loading::BuiltinsKey).await?;
        let implementation = builtins
            .rule_implementation(class.kind.as_str())
            .ok_or_else(|| SkyError::new(format!("builtins don't export a rule named '{}'", class.kind)))?;
        return Ok(Implementation::Builtin { file: bzl, implementation });
    }
    if bzl.repo().as_str() == bazel_loading::BUILTINS_REPO {
        return Err(SkyError::new(format!(
            "'{}' is implemented by Bazel's builtins, which bb can't analyze yet",
            class.kind
        )));
    }
    let module = ctx.compute(&BzlLoadKey(bzl)).await?;
    let missing = || SkyError::new(format!("{bzl} doesn't export a rule named '{name}'"));
    let rule = module.module.get_any_visibility(name.as_str()).map_err(|_| missing())?.0;
    let implementation = rule
        .try_map(|value| {
            let rule = bazel_starlark::rule_value::StarlarkRule::from_value(value.to_value()).ok_or(())?;
            rule.implementation.to_value().unpack_frozen().ok_or(())
        })
        .map_err(|()| missing())?;
    Ok(Implementation::Starlark { module, implementation })
}

/// Identifies what a rule's analysis depends on.
fn fingerprint(
    key: ConfiguredTargetKey,
    rule: &Rule,
    implementation: &Implementation,
    deps: &BTreeMap<(Label, ConfigId), Dep>,
    toolchains: &ResolvedToolchains,
) -> Fingerprint {
    let mut fingerprinter = Fingerprinter::new();
    fingerprinter.bytes(key.label.to_string().as_bytes());
    fingerprinter.bytes(format!("{:?}", key.config.get().options).as_bytes());
    fingerprinter.bytes(&skycodec::to_bytes(rule));
    match implementation {
        Implementation::Starlark { module, .. } => fingerprinter.bytes(module.fingerprint.as_bytes()),
        Implementation::Builtin { file, .. } => fingerprinter.bytes(file.to_string().as_bytes()),
        Implementation::Native(kind) => fingerprinter.bytes(kind.as_str().as_bytes()),
    };
    for dep in deps.values() {
        dep.fingerprint(&mut fingerprinter);
    }
    for (_, toolchain) in &toolchains.resolved {
        if let Some(toolchain) = toolchain {
            fingerprinter.bytes(toolchain.fingerprint.as_bytes());
        }
    }
    fingerprinter.finish()
}

/// An alias is its actual target, under its own label.
async fn alias(ctx: &Ctx, key: ConfiguredTargetKey, rule: &Rule) -> Result<ConfiguredTarget, SkyError> {
    let actual = rule.attr(Symbol::intern("actual")).cloned().unwrap_or(AttrValue::None);
    let AttrValue::Label(actual) = crate::attrs::resolve_value(ctx, &actual, key.config).await? else {
        return Err(SkyError::new(format!("{}: alias has no 'actual'", key.label)));
    };
    let target = ctx
        .compute(&ConfiguredTargetKey {
            label: actual,
            config: key.config,
        })
        .await?;
    Ok(ConfiguredTarget {
        label: key.label,
        config: key.config,
        kind: target.kind,
        is_test: target.is_test,
        value: PagedValue::new(target.providers()?.owned().clone()),
        actions: Vec::new(),
        generating: Vec::new(),
        fingerprint: target.fingerprint,
        incompatible: target.incompatible.clone(),
        size: 64,
    })
}

/// What evaluating a rule's implementation produced.
pub struct Analyzed {
    pub module: FrozenModule,
    pub actions: Vec<Action>,
}

/// The name the target value is kept under in its frozen module.
const TARGET: &str = "target";

fn configured(ctx: &Ctx, key: ConfiguredTargetKey, kind: Symbol, is_test: bool, analyzed: Analyzed, fingerprint: Fingerprint) -> ConfiguredTarget {
    let mut generating: Vec<(PathId, u32)> = Vec::new();
    for (index, action) in analyzed.actions.iter().enumerate() {
        generating.extend(action.outputs.iter().map(|output| (output.exec_path, index as u32)));
    }
    generating.sort();
    let value = analyzed.module.get(TARGET).expect("analysis sets the target");
    let size = analyzed.module.frozen_heap().allocated_bytes() + analyzed.actions.len() * size_of::<StoredAction>();
    let actions = analyzed.actions.into_iter().map(|action| StoredAction::new(ctx, action)).collect();
    ConfiguredTarget {
        label: key.label,
        config: key.config,
        kind,
        is_test,
        value: PagedValue::new(value),
        actions,
        generating,
        fingerprint,
        incompatible: None,
        size,
    }
}

/// The configured target of a target that can't be built for the target
/// platform: it provides nothing.
fn incompatible(ctx: &Ctx, key: ConfiguredTargetKey, kind: Symbol, is_test: bool, reason: Incompatibility) -> Result<ConfiguredTarget, SkyError> {
    let mut fingerprinter = Fingerprinter::new();
    fingerprinter.bytes(b"incompatible").bytes(key.label.to_string().as_bytes());
    fingerprinter.bytes(key.config.get().output_directory.as_bytes());
    for link in &reason.chain {
        fingerprinter.bytes(link.label.to_string().as_bytes());
    }
    fingerprinter.bytes(reason.constraint.to_string().as_bytes());
    let mut target = empty_target(ctx, key, kind.as_str(), is_test, fingerprinter.finish())?;
    target.incompatible = Some(Arc::new(reason));
    Ok(target)
}

/// Run the rule's implementation, on a blocking thread.
fn evaluate(input: RuleInput, name: FrozenHeapName) -> Result<Analyzed, String> {
    Module::with_temp_heap(|module| {
        module.frozen_heap().add_reference(bazel_starlark::provider::builtin_providers_heap());
        let actions = build_ctx::run(&module, &input).map_err(|error| format!("in {} rule {}: {error}", input.class.kind, input.label))?;
        let module = module.freeze_named(name).map_err(|error| format!("{}: {error:?}", input.label))?;
        Ok(Analyzed { module, actions })
    })
}

/// The configured target of a file: it has no actions of its own.
fn file(ctx: &Ctx, key: ConfiguredTargetKey, kind: &str, artifact: Artifact) -> Result<ConfiguredTarget, SkyError> {
    let mut fingerprinter = Fingerprinter::new();
    fingerprinter.bytes(kind.as_bytes()).bytes(artifact.exec_path.to_string().as_bytes());
    fingerprinter.bytes(format!("{:?}", artifact.config.map(|config| config.get().options.clone())).as_bytes());
    let fingerprint = fingerprinter.finish();
    let analyzed = Module::with_temp_heap(|module| -> anyhow::Result<Analyzed> {
        module.frozen_heap().add_reference(bazel_starlark::provider::builtin_providers_heap());
        let target = providers::file_target(key.label, artifact, module.heap())?;
        module.set(TARGET, module.heap().alloc(target));
        let module = module.freeze_named(bazel_starlark::heap_name("file", key.label, fingerprint)).map_err(|error| anyhow::anyhow!("{error:?}"))?;
        Ok(Analyzed { module, actions: Vec::new() })
    })
    .map_err(|error| SkyError::new(format!("{}: {error}", key.label)))?;
    Ok(configured(ctx, key, Symbol::intern(kind), false, analyzed, fingerprint))
}

/// The target that writes the status files.
fn workspace_status(ctx: &Ctx, key: ConfiguredTargetKey) -> Result<ConfiguredTarget, SkyError> {
    let fingerprint = Fingerprint::of(format!("workspace status {}", key.config.get().output_directory).as_bytes());
    let actions = crate::workspace_status::actions(&key.config.get());
    let mut target = empty_target(ctx, key, "workspace status", false, fingerprint)?;
    target.generating = actions
        .iter()
        .enumerate()
        .flat_map(|(index, action)| action.outputs.iter().map(move |output| (output.exec_path, index as u32)))
        .collect();
    target.generating.sort();
    target.actions = actions.into_iter().map(|action| StoredAction::new(ctx, action)).collect();
    Ok(target)
}

/// A `package_group`, which provides the packages it contains, with those
/// of the groups it includes, as `PackageSpecificationInfo`.
async fn package_group(ctx: &Ctx, key: ConfiguredTargetKey, group: &bazel_packages::PackageGroup) -> Result<ConfiguredTarget, SkyError> {
    let includes: Vec<ConfiguredTargetKey> = group.includes.iter().map(|label| ConfiguredTargetKey { label: *label, config: key.config }).collect();
    let mut included = Vec::with_capacity(includes.len());
    for target in ctx.compute_many(&includes).await {
        let target = target?;
        let providers = target.providers()?;
        let specification = providers
            .get()
            .and_then(|value| value.provider(crate::package_spec::PROVIDER))
            .and_then(|value| value.downcast_ref::<crate::package_spec::PackageSpecification>().cloned());
        match specification {
            Some(specification) => included.push(specification),
            None => return Err(SkyError::new(format!("{}: includes {}, which isn't a package_group", key.label, target.label))),
        }
    }
    let mapping = ctx.compute(&RepoMappingKey(key.label.repo())).await?;
    let includes: Vec<&crate::package_spec::PackageSpecification> = included.iter().collect();
    let specification = crate::package_spec::PackageSpecification::new(&group.packages, key.label.repo(), &mapping.0, &includes)
        .map_err(|error| SkyError::new(format!("{}: {error}", key.label)))?;
    let fingerprint = Fingerprint::of(format!("{}{:?}", key.label, group).as_bytes());
    let analyzed = Module::with_temp_heap(|module| -> anyhow::Result<Analyzed> {
        let defaults = providers::Defaults {
            files: Vec::new(),
            executable: None,
        };
        let provided = module.heap().alloc(specification);
        let target = providers::collect(key.label, provided, &defaults, module.heap())?;
        module.set(TARGET, module.heap().alloc(target));
        let module = module.freeze_named(bazel_starlark::heap_name("package group", key.label, fingerprint)).map_err(|error| anyhow::anyhow!("{error:?}"))?;
        Ok(Analyzed { module, actions: Vec::new() })
    })
    .map_err(|error| SkyError::new(format!("{}: {error}", key.label)))?;
    Ok(configured(ctx, key, Symbol::intern("package group"), false, analyzed, fingerprint))
}

/// A configured target with only an empty `DefaultInfo`.
fn empty_target(ctx: &Ctx, key: ConfiguredTargetKey, kind: &str, is_test: bool, fingerprint: Fingerprint) -> Result<ConfiguredTarget, SkyError> {
    let analyzed = Module::with_temp_heap(|module| -> anyhow::Result<Analyzed> {
        module.frozen_heap().add_reference(bazel_starlark::provider::builtin_providers_heap());
        let defaults = providers::Defaults {
            files: Vec::new(),
            executable: None,
        };
        let target = providers::collect(key.label, starlark::values::Value::new_none(), &defaults, module.heap())?;
        module.set(TARGET, module.heap().alloc(target));
        let module = module.freeze_named(bazel_starlark::heap_name("empty", key.label, fingerprint)).map_err(|error| anyhow::anyhow!("{error:?}"))?;
        Ok(Analyzed { module, actions: Vec::new() })
    })
    .map_err(|error| SkyError::new(format!("{}: {error}", key.label)))?;
    Ok(configured(ctx, key, Symbol::intern(kind), is_test, analyzed, fingerprint))
}
