//! Configured targets: targets analyzed in a configuration.

use bazel_starlark::print::Printed;
use bazel_starlark::provider::ProviderInstance;
use crate::action::{Action, RunfilesTree, StoredAction};
use crate::paging::PagedValue;
use crate::artifact::{self, Artifact};
use crate::attrs::resolve_attrs;
use crate::build_ctx::{self, Dep, RuleInput};
use crate::failure::{AttributeErrors, ImplementationFailed, ProvidersClash};
use crate::platform::PlatformKey;
use crate::providers;
use crate::toolchains::{self, ResolvedToolchains};
use crate::transition::{AttrConfigs, ExecConfigKey, configured_key, dep_configs};
use bazel_config::{AspectDescriptor, ConfigId};
use bazel_label::Label;
use bazel_loading::{BzlModule, PackageKey, RepoMappingKey};
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
    /// The targets it depends on directly, as `cquery` follows them.
    pub deps: Vec<ConfiguredDep>,
    /// The applications of aspects to its dependencies, whose own
    /// dependencies and actions `cquery` and `aquery` include.
    pub aspects: Vec<ConfiguredTargetKey>,
    /// The platform its actions run on, for a rule.
    pub exec_platform: Option<Label>,
    /// For an executable rule, its executable and runfiles tree.
    pub executable: Option<Executable>,
    /// For an executable rule, the arguments its `args` attribute gives
    /// the executable, expanded and split into words.
    pub args: Vec<String>,
    /// The environment its `RunEnvironmentInfo` gives its executable when
    /// it's run or tested.
    pub run_env: RunEnvironment,
    /// For a test, how its attributes, as configured, say to run it.
    pub test: Option<TestAttrs>,
    size: usize,
}

/// The executable an executable rule makes, and its runfiles tree, which
/// Bazel lays out with actions of the rule's own.
#[derive(Clone, Debug)]
pub struct Executable {
    pub file: Artifact,
    pub runfiles: Arc<RunfilesTree>,
}

/// The environment a target's `RunEnvironmentInfo` asks for: variables
/// with fixed values, and the names of those taken from the client's
/// environment, which take precedence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunEnvironment {
    pub fixed: Vec<(String, String)>,
    pub inherited: Vec<String>,
}

impl RunEnvironment {
    /// The variables an executable gets: the fixed ones, then the
    /// inherited ones the client's environment sets, from `client`.
    pub fn resolve(&self, client: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
        let mut env: BTreeMap<String, String> = self.fixed.iter().cloned().collect();
        for name in &self.inherited {
            if let Some(value) = client(name) {
                env.insert(name.clone(), value);
            }
        }
        env.into_iter().collect()
    }
}

/// How a test's attributes say to run it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestAttrs {
    /// `shard_count`: how many shards to split it into, or 0 for none.
    pub shard_count: u32,
    /// `flaky`: whether to try it again when it fails.
    pub flaky: bool,
}

impl TestAttrs {
    fn of(attrs: &[(Symbol, AttrValue)]) -> TestAttrs {
        let attr = |name: &str| attrs.iter().find(|(attr, _)| attr.as_str() == name).map(|(_, value)| value);
        let shard_count = match attr("shard_count") {
            Some(AttrValue::Int(count)) => u32::try_from(*count).unwrap_or(0),
            _ => 0,
        };
        let flaky = matches!(attr("flaky"), Some(AttrValue::Bool(true)));
        TestAttrs { shard_count, flaky }
    }
}

/// A configured target's direct dependency, as `cquery` follows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfiguredDep {
    pub label: Label,
    /// Its configuration, or `None` for a source file or a package group,
    /// which have none.
    pub config: Option<ConfigId>,
    /// Whether the BUILD file doesn't name it: an attribute's default
    /// value, or a dependency toolchain resolution adds.
    pub implicit: bool,
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
        let configured = configured.map_err(|error| unsupported_analysis_failure(*self, error));
        let is_aspect = !self.config.get().aspects.is_empty();
        if let (false, Ok(progress)) = (is_aspect, ctx.global::<Progress>()) {
            progress.target_configured();
        }
        configured
    }
}

/// A failure in a configuration with `--allow_analysis_failures`, which
/// Bazel would give dependents as an `AnalysisFailureInfo`: bb doesn't
/// support that, so it fails saying so.
fn unsupported_analysis_failure(key: ConfiguredTargetKey, error: SkyError) -> SkyError {
    if !key.config.get().options.bool("allow_analysis_failures") || error.origin().is_some() {
        return error;
    }
    SkyError::new(format!(
        "--allow_analysis_failures isn't supported by bb yet, so this failure can't be given to dependents as AnalysisFailureInfo:\n{}",
        error.message()
    ))
}

/// Analyze the target `key` names, in its configuration.
async fn configure(ctx: &Ctx, key: ConfiguredTargetKey) -> Result<ConfiguredTarget, SkyError> {
    if key.label == *crate::workspace_status::OWNER {
        return workspace_status(ctx, key).await;
    }
    let package = ctx.compute(&PackageKey(key.label.package)).await?;
    let Some(target) = package.0.target(key.label.name) else {
        return Err(SkyError::new(no_such_target(ctx, key.label, &package.0).await));
    };
    if !key.config.get().aspects.is_empty() {
        return crate::aspect::analyze(ctx, key, &package.0, target).await;
    }
    match target {
        Target::Rule(rule) => match analyze_rule(ctx, key, &package.0, rule).await {
            Err(error) if error.origin().is_none() => Err(analysis_failed(ctx, key, error).await),
            analyzed => analyzed,
        },
        Target::SourceFile(_) => file(ctx, key, "source file", Artifact::source(key.label)),
        Target::OutputFile { rule, .. } => {
            let owner = configured_key(ctx, key.label.sibling(*rule), key.config).await?;
            ctx.compute(&owner).await?;
            let artifact = Artifact::derived(owner.label, &key.config.get(), artifact::name_path(key.label), false);
            let mut target = file(ctx, key, "generated file", artifact)?;
            target.deps = vec![ConfiguredDep { label: owner.label, config: Some(owner.config), implicit: false }];
            Ok(target)
        }
        Target::PackageGroup(group) => package_group(ctx, key, group).await,
    }
}

/// A rule's error as Bazel reports it: then that the target's analysis
/// failed, at the rule, with the target as the error's origin.
pub(crate) async fn analysis_failed(ctx: &Ctx, key: ConfiguredTargetKey, error: SkyError) -> SkyError {
    let failed = format!("Analysis of target '{}' (config: {}) failed", key.label, key.config.get().checksum);
    let failed = match bazel_loading::target_location(ctx, key.label).await {
        Some(location) => format!("{location}: {failed}"),
        None => failed,
    };
    SkyError::with_kind(error.kind(), errors(&[error.message(), &failed])).with_origin(key.label.to_string())
}

/// A toolchain resolution's error, as Bazel reports it: at the rule, for
/// the target in its configuration, with the target as the origin.
async fn toolchain_error(ctx: &Ctx, key: ConfiguredTargetKey, error: SkyError) -> SkyError {
    let message = format!("While resolving toolchains for target {} ({}): {}", key.label, key.config.get().checksum, error.message());
    let message = match bazel_loading::target_location(ctx, key.label).await {
        Some(location) => format!("{location}: {message}"),
        None => message,
    };
    SkyError::with_kind(error.kind(), message).with_origin(key.label.to_string())
}

/// The kind of a package group's configured target.
pub(crate) const PACKAGE_GROUP: &str = "package group";

/// Several errors as one error's message, which the server reports as
/// Bazel does, each with its own `ERROR:`.
pub(crate) fn errors(errors: &[&str]) -> String {
    errors.join("\nERROR: ")
}

/// The error for a target its package doesn't declare, naming the
/// package's BUILD file by its path.
async fn no_such_target(ctx: &Ctx, label: Label, package: &Package) -> String {
    let build_file = match bazel_loading::build_file_path(ctx, label.package).await {
        Ok(Some(path)) => path,
        _ => std::path::PathBuf::from(package.build_file.to_string()),
    };
    let suggestion = bazel_loading::missing_target_suggestion(ctx, label, package).await;
    format!("{}{suggestion}", bazel_loading::no_such_target(label, &build_file))
}

/// What implements a rule class.
pub enum Implementation {
    /// A function in a `.bzl` file, and the rule it implements.
    Starlark {
        module: Arc<BzlModule>,
        rule: OwnedFrozenValue,
        implementation: OwnedFrozenValue,
    },
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
    let is_test = class.test;
    let setup = match setup(ctx, key, rule, &class_ref, class).await? {
        Setup::Ready(setup) => setup,
        Setup::Incompatible(reason) => return incompatible(ctx, key, class_ref.kind, is_test, reason),
    };
    let deps = match is_stub(&setup.implementation) {
        true => BTreeMap::new(),
        false => analyze_deps(ctx, label, &setup.class, &setup.attrs, setup.deps_config, setup.exec_config, &setup.attr_configs).await?,
    };
    if let Some(reason) = incompatible_dep(key, &deps) {
        return incompatible(ctx, key, class_ref.kind, is_test, reason);
    }
    check_dependencies(ctx, key, package, rule, &setup, &deps).await?;
    let fingerprint = fingerprint(key, rule, &setup.implementation, &deps, &setup.toolchains);
    let edges = dependency_edges(ctx, key, package, rule, &setup, &deps).await?;
    let applications = aspect_applications(&deps);
    let exec_platform = setup.toolchains.exec_platform.unwrap_or(key.config.get().host_platform);
    let test = is_test.then(|| TestAttrs::of(&setup.attrs));
    let input = rule_input(ctx, key, package, setup, deps).await?;
    let analyzed = evaluate_reporting(ctx, label, input, heap_name("target", key, fingerprint)).await?;
    let executable = executable(&analyzed);
    let run_env = run_environment(&analyzed);
    let mut target = configured(ctx, key, class_ref.kind, is_test, analyzed, fingerprint);
    target.executable = executable;
    target.run_env = run_env;
    target.test = test;
    target.deps = edges;
    target.exec_platform = Some(exec_platform);
    target.aspects = applications;
    Ok(target)
}

/// The executable a rule's analysis gave its `DefaultInfo`, with its
/// runfiles tree, as Bazel gives any rule that returns one; `None` if it
/// gave a source file, which runs without one.
fn executable(analyzed: &Analyzed) -> Option<Executable> {
    let value = analyzed.module.get(TARGET).ok()?;
    let target = crate::target_value::TargetValue::from_value(value.value())?;
    target.default_info("executable")?;
    let files_to_run = crate::runfiles::FilesToRun::from_value(target.default_info("files_to_run")?)?;
    let file = crate::file_value::FileValue::artifact(files_to_run.executable).filter(|file| !file.is_source())?;
    let runfiles = crate::runfiles::Runfiles::from_value(files_to_run.runfiles)?;
    let runfiles = crate::state::runfiles_tree(file, runfiles);
    Some(Executable { file, runfiles })
}

/// The environment a rule's `RunEnvironmentInfo` gives its executable.
fn run_environment(analyzed: &Analyzed) -> RunEnvironment {
    let Ok(value) = analyzed.module.get(TARGET) else {
        return RunEnvironment::default();
    };
    let Some(target) = crate::target_value::TargetValue::from_value(value.value()) else {
        return RunEnvironment::default();
    };
    let Some(info) = target.provider("RunEnvironmentInfo").and_then(ProviderInstance::from_value) else {
        return RunEnvironment::default();
    };
    let mut fixed = Vec::new();
    if let Some(dict) = info.field("environment").and_then(starlark::values::dict::DictRef::from_value) {
        fixed.extend(dict.iter().filter_map(|(key, value)| Some((key.unpack_str()?.to_string(), value.unpack_str()?.to_string()))));
    }
    let mut inherited = Vec::new();
    if let Some(list) = info.field("inherited_environment").and_then(starlark::values::list::ListRef::from_value) {
        inherited.extend(list.iter().filter_map(|name| name.unpack_str().map(str::to_string)));
    }
    RunEnvironment { fixed, inherited }
}

/// What a rule's analysis needs before its dependencies are analyzed.
pub(crate) struct RuleSetup {
    pub class: Arc<RuleClass>,
    pub attrs: Vec<(Symbol, AttrValue)>,
    pub implementation: Implementation,
    pub deps_config: ConfigId,
    pub attr_configs: AttrConfigs,
    pub toolchains: ResolvedToolchains,
    pub exec_config: ConfigId,
}

/// A rule set up for analysis, or why it can't be built for the target
/// platform.
pub(crate) enum Setup {
    Ready(RuleSetup),
    Incompatible(Incompatibility),
}

/// Set up a rule's analysis: resolve its attributes, check it's
/// compatible with the target platform, find its implementation, the
/// configurations of its dependencies and its toolchains.
pub(crate) async fn setup(ctx: &Ctx, key: ConfiguredTargetKey, rule: &Rule, class_ref: &RuleClassRef, class: Arc<RuleClass>) -> Result<Setup, SkyError> {
    let label = key.label;
    let attrs = resolve_attrs(ctx, rule, &class, key.config).await?;
    if let Some(constraint) = unsatisfied_constraint(ctx, &attrs, key.config).await? {
        return Ok(Setup::Incompatible(Incompatibility { chain: vec![key], constraint }));
    }
    let implementation = implementation(ctx, class_ref)
        .await
        .map_err(|error| format!("in {} rule {label}: {}", class_ref.kind, error.message()));
    let implementation = match implementation {
        Ok(implementation) => implementation,
        Err(message) => return Err(located(ctx, label, message).await),
    };
    let config = key.config.get();
    let deps_config = config.for_dependencies();
    let attr_configs = match &implementation {
        Implementation::Starlark { rule, .. } => crate::transition::attr_configs(ctx, label, rule, &attrs, deps_config).await?,
        Implementation::Builtin { .. } | Implementation::Native(_) => Vec::new(),
    };
    let toolchains = match toolchains::resolve(ctx, label, &class, &config, &attr_labels(&attrs, "exec_compatible_with")).await {
        Ok(toolchains) => toolchains,
        Err(error) if error.origin().is_some() => return Err(error),
        Err(error) => return Err(toolchain_error(ctx, key, error).await),
    };
    let exec_platform = toolchains.exec_platform.unwrap_or(config.host_platform);
    let exec_config = exec_config(ctx, &class, deps_config, exec_platform).await?;
    let attr_configs = crate::aspect::with_attached_aspects(ctx, &class, &attrs, deps_config, exec_config, attr_configs).await?;
    Ok(Setup::Ready(RuleSetup {
        class,
        attrs,
        implementation,
        deps_config,
        attr_configs,
        toolchains,
        exec_config,
    }))
}

/// Everything a rule's implementation is run with, from its setup and its
/// analyzed dependencies.
pub(crate) async fn rule_input(ctx: &Ctx, key: ConfiguredTargetKey, package: &Package, setup: RuleSetup, deps: BTreeMap<(Label, ConfigId), Dep>) -> Result<RuleInput, SkyError> {
    let config = key.config.get();
    let platform = ctx.compute(&PlatformKey(config.platform)).await?;
    let mapping = ctx.compute(&RepoMappingKey(key.label.repo())).await?;
    let main_mapping = ctx.compute(&RepoMappingKey(bazel_label::RepoName::MAIN)).await?;
    let mappings = match &setup.implementation {
        Implementation::Starlark { module, .. } => bazel_loading::mappings(ctx, &module.repos).await?,
        Implementation::Builtin { .. } | Implementation::Native(_) => Default::default(),
    };
    let helper = native_helper(ctx, key.label, &setup.class).await?;
    Ok(RuleInput {
        label: key.label,
        config,
        class: setup.class,
        attrs: setup.attrs,
        attr_configs: setup.attr_configs,
        deps_config: setup.deps_config,
        exec_config: setup.exec_config,
        deps,
        build_file: package.build_file,
        implementation: setup.implementation,
        mapping: Arc::new(mapping.0.clone()),
        main_mapping: Arc::new(main_mapping.0.clone()),
        mappings,
        target_constraints: platform.constraints.clone(),
        target_cpu: platform.cpu(),
        aspect: None,
        toolchains: setup.toolchains,
        helper,
    })
}

/// The Starlark function a native rule's implementation calls, if it
/// calls one: rules_java's merge of `JavaPluginInfo`s, which
/// `java_plugins_flag_alias` provides one of, as rules_java's provider.
async fn native_helper(ctx: &Ctx, label: Label, class: &RuleClass) -> Result<Option<OwnedFrozenValue>, SkyError> {
    if class.kind.as_str() != "java_plugins_flag_alias" {
        return Ok(None);
    }
    let mapping = ctx.compute(&RepoMappingKey(label.repo())).await?;
    let context = bazel_label::LabelContext::new(label.package, &mapping.0);
    let java_info = Label::parse("@rules_java//java/private:java_info.bzl", &context).map_err(|error| SkyError::new(format!("{label}: finding rules_java: {error}")))?;
    let module = ctx.compute(&bazel_loading::bzl_key_for_build(ctx, java_info).await?).await?;
    let merge = module.module.get_any_visibility("merge_plugin_info_without_outputs");
    let merge = merge.map_err(|_| SkyError::new(format!("{java_info} has no merge_plugin_info_without_outputs")))?;
    Ok(Some(merge.0))
}

/// Run an implementation, reporting what it printed, with its errors at
/// the target.
pub(crate) async fn evaluate_reporting(ctx: &Ctx, label: Label, input: RuleInput, name: FrozenHeapName) -> Result<Analyzed, SkyError> {
    let (analyzed, printed) = bazel_loading::blocking(move || evaluate(input, name)).await?;
    bazel_loading::report_printed(ctx, &printed).await;
    match analyzed {
        Ok(analyzed) => Ok(analyzed),
        Err(Failure::Implementation { target, failed }) => {
            let message = format!("in {target}: {}", failed.describe(ctx).await);
            Err(located(ctx, label, message).await)
        }
        Err(Failure::Clash(clash)) => Err(located(ctx, label, clash.to_string()).await.with_origin(label.to_string())),
        Err(Failure::Attributes { errors, implementation }) => {
            let mut messages = Vec::with_capacity(errors.len() + 1);
            for error in errors {
                messages.push(located(ctx, label, error).await.message().to_string());
            }
            if let Some((target, failed)) = implementation {
                let message = format!("in {target}: {}", failed.describe(ctx).await);
                messages.push(located(ctx, label, message).await.message().to_string());
            }
            let messages: Vec<&str> = messages.iter().map(String::as_str).collect();
            Err(SkyError::new(self::errors(&messages)))
        }
    }
}

/// Why running an implementation failed.
pub(crate) enum Failure {
    /// The implementation failed, as Bazel reports after
    /// `in <target>: `.
    Implementation { target: String, failed: ImplementationFailed },
    /// The providers of a dependency and of its aspects clash, which Bazel
    /// reports at the target alone.
    Clash(ProvidersClash),
    /// Its attributes had errors, reported each at the target, then how the
    /// implementation failed, if it did.
    Attributes { errors: Vec<String>, implementation: Option<(String, ImplementationFailed)> },
}

impl Failure {
    fn of(input: &RuleInput, error: anyhow::Error) -> Failure {
        let error = match error.downcast::<ProvidersClash>() {
            Ok(clash) => return Failure::Clash(clash),
            Err(error) => error,
        };
        let error = match error.downcast::<AttributeErrors>() {
            Ok(errors) => {
                return Failure::Attributes {
                    errors: errors.errors,
                    implementation: errors.implementation.map(|failed| (implementing(input), failed)),
                };
            }
            Err(error) => error,
        };
        let failed = error.downcast::<ImplementationFailed>().unwrap_or_else(|error| ImplementationFailed::reported(error.to_string()));
        Failure::Implementation {
            target: implementing(input),
            failed,
        }
    }
}

/// What Bazel says it was analyzing when an implementation fails:
/// `<kind> rule <label>`, or for aspects,
/// `<aspects> aspect on <kind> rule <label>`.
fn implementing(input: &RuleInput) -> String {
    let Some(view) = &input.aspect else {
        return format!("{} rule {}", input.class.kind, input.label);
    };
    let aspects: Vec<String> = input.config.aspects.iter().map(AspectDescriptor::description).collect();
    format!("{} aspect on {} rule {}", aspects.join(","), view.rule.class.kind, input.label)
}

/// The labels a label list attribute has, such as `exec_compatible_with`.
pub(crate) fn attr_labels(attrs: &[(Symbol, AttrValue)], name: &str) -> Vec<Label> {
    let name = Symbol::intern(name);
    match attrs.iter().find(|(attr, _)| *attr == name) {
        Some((_, AttrValue::LabelList(labels))) => labels.clone(),
        _ => Vec::new(),
    }
}

/// The configuration of a rule's tools, which run on `platform`: its exec
/// configuration, if it has attributes for tools.
pub(crate) async fn exec_config(ctx: &Ctx, class: &RuleClass, config: ConfigId, platform: Label) -> Result<ConfigId, SkyError> {
    if !class.attrs.iter().any(|schema| schema.cfg == Cfg::Exec) {
        return Ok(config);
    }
    let exec = ctx.compute(&ExecConfigKey { config, platform }).await?;
    Ok(exec.0)
}

/// An error in analyzing a rule, at where the rule was declared, as Bazel
/// reports it.
pub(crate) async fn located(ctx: &Ctx, label: Label, message: String) -> SkyError {
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
        Dep::Aspect(applied) => applied.first().and_then(|target| target.incompatible.clone()),
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

/// Analyze every dependency the rule's attributes name.
pub(crate) async fn analyze_deps(
    ctx: &Ctx,
    referrer: Label,
    class: &RuleClass,
    attrs: &[(Symbol, AttrValue)],
    config: ConfigId,
    exec_config: ConfigId,
    attr_configs: &AttrConfigs,
) -> Result<BTreeMap<(Label, ConfigId), Dep>, SkyError> {
    let mut wanted: Vec<(Label, ConfigId)> = wanted_deps(class, attrs, config, exec_config, attr_configs)?.into_iter().map(|(label, config, _)| (label, config)).collect();
    wanted.sort();
    wanted.dedup();
    let results = futures::future::join_all(wanted.iter().map(|(label, config)| dep(ctx, *label, *config))).await;
    let mut deps = BTreeMap::new();
    for (wanted, result) in wanted.into_iter().zip(results) {
        match result {
            Ok(dep) => deps.insert(wanted, dep),
            Err(DepError::Failed(error)) => return Err(error),
            Err(DepError::AspectFailed(error)) => return Err(error.with_origin(referrer.to_string())),
            Err(DepError::Missing(message)) => return Err(missing_dep(ctx, referrer, &message).await.with_origin(wanted.0.to_string())),
        };
    }
    Ok(deps)
}

/// The dependencies a rule's attributes name, each in the configuration
/// it's built in, with the attribute that names it.
fn wanted_deps(
    class: &RuleClass,
    attrs: &[(Symbol, AttrValue)],
    config: ConfigId,
    exec_config: ConfigId,
    attr_configs: &AttrConfigs,
) -> Result<Vec<(Label, ConfigId, Symbol)>, SkyError> {
    let mut wanted = Vec::new();
    for (schema, (_, value)) in class.attrs.iter().zip(attrs) {
        if !schema.is_dependency || !schema.ty.has_labels() || schema.ty == AttrType::Output || is_late_bound(schema.name) {
            continue;
        }
        let mut labels = Vec::new();
        value.labels(&mut labels);
        for (_, dep_config) in dep_configs(schema.cfg, schema.name, config, exec_config, attr_configs) {
            wanted.extend(labels.iter().map(|label| (*label, dep_config, schema.name)));
        }
    }
    Ok(wanted)
}

/// A rule's direct dependencies, as `cquery` follows them: what its
/// attributes name, its package's default licenses, which Bazel analyzes
/// in a configuration of no options, a test suite's implicit tests, the
/// conditions of its `select()`s, and for a rule that resolves toolchains, its toolchains
/// and the platforms it resolves them for. The BUILD file names those of attributes it sets and the
/// conditions; the rest are implicit.
async fn dependency_edges(ctx: &Ctx, key: ConfiguredTargetKey, package: &Package, rule: &Rule, setup: &RuleSetup, deps: &BTreeMap<(Label, ConfigId), Dep>) -> Result<Vec<ConfiguredDep>, SkyError> {
    let mut edges: Vec<ConfiguredDep> = Vec::new();
    for (label, config, attr) in wanted_deps(&setup.class, &setup.attrs, setup.deps_config, setup.exec_config, &setup.attr_configs)? {
        let implicit = rule.attr(attr).is_none();
        if let Some(dep) = deps.get(&(label, config)) {
            add_edge(&mut edges, dep_edge(label, config, dep, implicit));
        }
    }
    for license in package.default_licenses(rule) {
        add_edge(&mut edges, ConfiguredDep { label: license, config: Some(bazel_config::no_config()), implicit: true });
    }
    if setup.class.kind.as_str() == "test_suite" {
        for test in package.implicit_tests(rule) {
            add_edge(&mut edges, ConfiguredDep { label: test, config: Some(setup.deps_config), implicit: false });
        }
    }
    for condition in conditions(rule, &setup.class) {
        let condition = configured_key(ctx, condition, setup.deps_config).await?;
        add_edge(&mut edges, ConfiguredDep { label: condition.label, config: Some(condition.config), implicit: false });
    }
    if !resolves_toolchains(&setup.class) {
        return Ok(edges);
    }
    for toolchain_type in setup.class.toolchain_types.iter().chain(&setup.class.optional_toolchain_types) {
        add_edge(&mut edges, ConfiguredDep { label: *toolchain_type, config: Some(setup.deps_config), implicit: true });
    }
    for target in setup.toolchains.resolved.iter().filter_map(|(_, target)| target.as_ref()) {
        add_edge(&mut edges, ConfiguredDep { label: target.label, config: Some(target.config), implicit: true });
    }
    let config = key.config.get();
    let no_config = bazel_config::no_config();
    let exec_platform = setup.toolchains.exec_platform.unwrap_or(config.host_platform);
    for platform in [config.requested_platform(), exec_platform] {
        add_edge(&mut edges, ConfiguredDep { label: platform, config: Some(no_config), implicit: true });
    }
    Ok(edges)
}

/// The applications of aspects to a target's dependencies: each but the
/// first of what a dependency with aspects applied is.
pub(crate) fn aspect_applications(deps: &BTreeMap<(Label, ConfigId), Dep>) -> Vec<ConfiguredTargetKey> {
    let mut applications = Vec::new();
    for dep in deps.values() {
        if let Dep::Aspect(applied) = dep {
            applications.extend(applied.iter().skip(1).map(|target| ConfiguredTargetKey { label: target.label, config: target.config }));
        }
    }
    applications.sort();
    applications.dedup();
    applications
}

/// The edges to the dependencies an aspect's own attributes name, which
/// are implicit: the BUILD file doesn't name them.
pub(crate) fn aspect_edges(own: &RuleSetup, deps: &BTreeMap<(Label, ConfigId), Dep>) -> Result<Vec<ConfiguredDep>, SkyError> {
    let mut edges = Vec::new();
    for (label, config, _) in wanted_deps(&own.class, &own.attrs, own.deps_config, own.exec_config, &own.attr_configs)? {
        if let Some(dep) = deps.get(&(label, config)) {
            add_edge(&mut edges, dep_edge(label, config, dep, true));
        }
    }
    Ok(edges)
}

/// The conditions of the `select()`s of a rule's attributes, as given or
/// by default, but for `//conditions:default`.
fn conditions(rule: &Rule, class: &RuleClass) -> Vec<Label> {
    let mut conditions = Vec::new();
    for schema in &class.attrs {
        match (rule.attr(schema.name), &schema.default) {
            (Some(value), _) | (None, bazel_packages::AttrDefault::Value(value)) => value.conditions(&mut conditions),
            _ => {}
        }
    }
    conditions.retain(|condition| condition.to_string() != bazel_packages::DEFAULT_CONDITION);
    conditions.sort();
    conditions.dedup();
    conditions
}

/// The edge to `label`, a dependency analyzed as `dep`: in the target's
/// own configuration, or a file's, which a source file hasn't. An alias
/// is its own node, which depends on its actual target.
fn dep_edge(label: Label, config: ConfigId, dep: &Dep, implicit: bool) -> ConfiguredDep {
    let config = match dep {
        Dep::Target(target) if target.kind.as_str() == PACKAGE_GROUP => None,
        Dep::Target(target) => Some(target.config),
        Dep::Aspect(applied) => Some(applied.first().map_or(config, |target| target.config)),
        Dep::File(_, artifact) => artifact.config,
    };
    ConfiguredDep { label, config, implicit }
}

/// Add an edge, once: a dependency named both implicitly and explicitly
/// is explicit, as Bazel has it.
fn add_edge(edges: &mut Vec<ConfiguredDep>, edge: ConfiguredDep) {
    match edges.iter_mut().find(|existing| existing.label == edge.label && existing.config == edge.config) {
        Some(existing) => existing.implicit &= edge.implicit,
        None => edges.push(edge),
    }
}

/// Whether a rule resolves toolchains, as Bazel's do but for those that
/// configure the build: build settings, platforms and constraints,
/// toolchains, config settings and aliases.
fn resolves_toolchains(class: &RuleClass) -> bool {
    const CONFIGURING: &[&str] = &[
        "alias",
        "config_setting",
        "config_feature_flag",
        "constraint_setting",
        "constraint_value",
        "environment",
        "label_flag",
        "label_setting",
        "platform",
        "toolchain",
        "toolchain_type",
    ];
    class.build_setting.is_none() && !CONFIGURING.contains(&class.kind.as_str())
}

/// Why a dependency can't be had.
enum DepError {
    /// It doesn't exist, as this message says.
    Missing(String),
    /// It failed to analyze.
    Failed(SkyError),
    /// An aspect applied to it failed, which Bazel reports as the
    /// failure of the target that applies it.
    AspectFailed(SkyError),
}

/// Fail as Bazel does when the rule's dependencies aren't what its
/// attributes allow or aren't visible to it, reporting the warnings the
/// checks found too.
async fn check_dependencies(ctx: &Ctx, key: ConfiguredTargetKey, package: &Package, rule: &Rule, setup: &RuleSetup, deps: &BTreeMap<(Label, ConfigId), Dep>) -> Result<(), SkyError> {
    let checked = crate::dep_checks::check(ctx, key, package, rule, setup, deps).await?;
    let location = bazel_loading::target_location(ctx, key.label).await;
    let at = |message: &str| match &location {
        Some(location) => format!("{location}: {message}"),
        None => message.to_string(),
    };
    if let Ok(progress) = ctx.global::<Progress>() {
        for warning in &checked.warnings {
            progress.report(Event::Warning(at(warning)));
        }
    }
    if checked.errors.is_empty() {
        return Ok(());
    }
    let located: Vec<String> = checked.errors.iter().map(|error| at(error)).collect();
    let located: Vec<&str> = located.iter().map(String::as_str).collect();
    Err(SkyError::new(errors(&located)))
}

/// The error for a dependency that doesn't exist, as Bazel reports it:
/// where the target that refers to it was declared, and that it's
/// referred to. It's the dependency's error, as the missing label is its
/// origin.
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

/// One dependency: a file, or an analyzed rule, with the aspects its
/// configuration names applied.
async fn dep(ctx: &Ctx, label: Label, config: ConfigId) -> Result<Dep, DepError> {
    let aspects = config.get().aspects.clone();
    if aspects.is_empty() {
        return plain_dep(ctx, label, config).await;
    }
    let plain_config = config.get().without_aspects();
    let target = match plain_dep(ctx, label, plain_config).await? {
        Dep::Target(target) => target,
        Dep::File(label, artifact) if !artifact.is_source() => {
            let file = ConfiguredTargetKey { label, config: plain_config };
            ctx.compute(&file).await.map_err(DepError::Failed)?
        }
        plain => return Ok(plain),
    };
    crate::aspect::applied(ctx, target, &aspects).await.map_err(DepError::AspectFailed)
}

/// One dependency, without aspects.
async fn plain_dep(ctx: &Ctx, label: Label, config: ConfigId) -> Result<Dep, DepError> {
    let package = match ctx.compute(&PackageKey(label.package)).await {
        Ok(package) => package,
        Err(error) => return Err(DepError::Missing(error.message().to_string())),
    };
    let analyzed = |key: ConfiguredTargetKey| async move { ctx.compute(&key).await.map_err(DepError::Failed) };
    match package.0.target(label.name) {
        None => Err(DepError::Missing(report_no_such_target(ctx, label, &package.0).await)),
        Some(Target::SourceFile(_)) => Ok(Dep::File(label, Artifact::source(label))),
        Some(Target::OutputFile { rule, .. }) => {
            let owner = configured_key(ctx, label.sibling(*rule), config).await.map_err(DepError::Failed)?;
            analyzed(owner).await?;
            Ok(Dep::File(label, Artifact::derived(owner.label, &owner.config.get(), artifact::name_path(label), false)))
        }
        Some(_) => {
            let key = configured_key(ctx, label, config).await.map_err(DepError::Failed)?;
            Ok(Dep::Target(analyzed(key).await?))
        }
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
    let (module, global) = match bazel_loading::is_build_file(bzl) {
        true => (ctx.compute(&bazel_loading::BuildModuleKey(bzl.package)).await?, bazel_starlark::testing::analysis_test_global(name.as_str())),
        false => (ctx.compute(&bazel_loading::bzl_key_for_build(ctx, bzl).await?).await?, name.as_str().to_string()),
    };
    let missing = || SkyError::new(format!("{bzl} doesn't export a rule named '{name}'"));
    let rule = module.module.get_any_visibility(&global).map_err(|_| missing())?.0;
    let implementation = rule
        .clone()
        .try_map(|value| {
            let rule = bazel_starlark::rule_value::StarlarkRule::from_value(value.to_value()).ok_or(())?;
            rule.implementation.to_value().unpack_frozen().ok_or(())
        })
        .map_err(|()| missing())?;
    Ok(Implementation::Starlark { module, rule, implementation })
}

/// Identifies what a rule's analysis depends on.
pub(crate) fn fingerprint(
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

/// An alias is its actual target, under its own label, which depends on
/// it and the conditions of a `select()` choosing it.
pub(crate) async fn alias(ctx: &Ctx, key: ConfiguredTargetKey, rule: &Rule) -> Result<ConfiguredTarget, SkyError> {
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
    let mut alias = forwarded(key, &target)?;
    let mut conditions = Vec::new();
    for (_, value) in &rule.attrs {
        value.conditions(&mut conditions);
    }
    for condition in conditions.into_iter().filter(|condition| condition.to_string() != bazel_packages::DEFAULT_CONDITION) {
        let condition = configured_key(ctx, condition, key.config).await?;
        add_edge(&mut alias.deps, ConfiguredDep { label: condition.label, config: Some(condition.config), implicit: false });
    }
    Ok(alias)
}

/// The target `key` names, providing what `target` provides: an alias's,
/// or an aspect's on a generated file, which its generating rule's
/// application provides.
pub(crate) fn forwarded(key: ConfiguredTargetKey, target: &ConfiguredTarget) -> Result<ConfiguredTarget, SkyError> {
    let mut fingerprinter = Fingerprinter::new();
    fingerprinter.bytes(b"forwarded").bytes(key.label.to_string().as_bytes()).bytes(target.fingerprint.as_bytes());
    Ok(ConfiguredTarget {
        label: key.label,
        config: key.config,
        kind: target.kind,
        is_test: target.is_test,
        value: PagedValue::new(target.providers()?.owned().clone()),
        actions: Vec::new(),
        generating: Vec::new(),
        fingerprint: fingerprinter.finish(),
        incompatible: target.incompatible.clone(),
        deps: vec![ConfiguredDep {
            label: target.label,
            config: Some(target.config),
            implicit: false,
        }],
        exec_platform: target.exec_platform,
        aspects: Vec::new(),
        executable: None,
        args: target.args.clone(),
        run_env: target.run_env.clone(),
        test: target.test,
        size: 64,
    })
}

/// What evaluating a rule's implementation produced.
pub struct Analyzed {
    pub module: FrozenModule,
    pub actions: Vec<Action>,
    /// An executable rule's expanded `args`.
    pub args: Vec<String>,
}

/// The name the target value is kept under in its frozen module.
pub(crate) const TARGET: &str = "target";

pub(crate) fn configured(ctx: &Ctx, key: ConfiguredTargetKey, kind: Symbol, is_test: bool, analyzed: Analyzed, fingerprint: Fingerprint) -> ConfiguredTarget {
    let mut generating: Vec<(PathId, u32)> = Vec::new();
    for (index, action) in analyzed.actions.iter().enumerate() {
        generating.extend(action.outputs.iter().map(|output| (output.exec_path, index as u32)));
    }
    generating.sort();
    generating.dedup_by_key(|(path, _)| *path);
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
        deps: Vec::new(),
        exec_platform: None,
        aspects: Vec::new(),
        executable: None,
        args: analyzed.args,
        run_env: RunEnvironment::default(),
        test: None,
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

/// Run the rule's implementation, on a blocking thread, returning what
/// it printed too.
pub(crate) fn evaluate(input: RuleInput, name: FrozenHeapName) -> (Result<Analyzed, Failure>, Vec<Printed>) {
    let mut printed = Vec::new();
    let analyzed = Module::with_temp_heap(|module| {
        module.frozen_heap().add_reference(bazel_starlark::provider::builtin_providers_heap());
        let ran = build_ctx::run(&module, &input, &mut printed).map_err(|error| Failure::of(&input, error))?;
        let module = module.freeze_named(name).map_err(|error| Failure::of(&input, anyhow::anyhow!("{}: {error:?}", input.label)))?;
        Ok(Analyzed {
            module,
            actions: ran.actions,
            args: ran.args,
        })
    });
    (analyzed, printed)
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
        let module = module.freeze_named(heap_name("file", key, fingerprint)).map_err(|error| anyhow::anyhow!("{error:?}"))?;
        Ok(Analyzed { module, actions: Vec::new(), args: Vec::new() })
    })
    .map_err(|error| SkyError::new(format!("{}: {error}", key.label)))?;
    Ok(configured(ctx, key, Symbol::intern(kind), false, analyzed, fingerprint))
}

/// The target that writes the status files.
async fn workspace_status(ctx: &Ctx, key: ConfiguredTargetKey) -> Result<ConfiguredTarget, SkyError> {
    let values = ctx.compute(&crate::workspace_status::WorkspaceStatusKey).await?;
    let fingerprint = Fingerprint::of(format!("workspace status {values:?}").as_bytes());
    let actions = crate::workspace_status::actions(&values);
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
        let module = module.freeze_named(heap_name("package group", key, fingerprint)).map_err(|error| anyhow::anyhow!("{error:?}"))?;
        Ok(Analyzed { module, actions: Vec::new(), args: Vec::new() })
    })
    .map_err(|error| SkyError::new(format!("{}: {error}", key.label)))?;
    Ok(configured(ctx, key, Symbol::intern(PACKAGE_GROUP), false, analyzed, fingerprint))
}

/// The name of the frozen heap of `key`'s providers. Heaps are told apart
/// by name when they're paged, so the name has the configuration, which
/// targets in different configurations may otherwise share, such as a
/// source file's.
pub(crate) fn heap_name(what: &str, key: ConfiguredTargetKey, fingerprint: Fingerprint) -> starlark::values::FrozenHeapName {
    let target = format!("{} {}", key.label, key.config.get().output_directory);
    bazel_starlark::heap_name(what, target, fingerprint)
}

/// A configured target with only an empty `DefaultInfo`.
pub(crate) fn empty_target(ctx: &Ctx, key: ConfiguredTargetKey, kind: &str, is_test: bool, fingerprint: Fingerprint) -> Result<ConfiguredTarget, SkyError> {
    let analyzed = Module::with_temp_heap(|module| -> anyhow::Result<Analyzed> {
        module.frozen_heap().add_reference(bazel_starlark::provider::builtin_providers_heap());
        let defaults = providers::Defaults {
            files: Vec::new(),
            executable: None,
        };
        let target = providers::collect(key.label, starlark::values::Value::new_none(), &defaults, module.heap())?;
        module.set(TARGET, module.heap().alloc(target));
        let module = module.freeze_named(heap_name("empty", key, fingerprint)).map_err(|error| anyhow::anyhow!("{error:?}"))?;
        Ok(Analyzed { module, actions: Vec::new(), args: Vec::new() })
    })
    .map_err(|error| SkyError::new(format!("{}: {error}", key.label)))?;
    Ok(configured(ctx, key, Symbol::intern(kind), is_test, analyzed, fingerprint))
}
