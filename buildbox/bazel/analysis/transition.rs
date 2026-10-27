//! Applying configuration transitions: running a transition's
//! implementation over a configuration's options.
//!
//! The implementation gets the settings the transition reads, as Starlark
//! values, and the rule's attributes. It returns the settings it writes:
//! one dict for a transition to one configuration, or a dict of them, or a
//! list, for a split into several.

use bazel_config::{BuildConfiguration, BuildOptions, ConfigId, LabelParser, NativeOption, OptionType, OptionValue};
use bazel_label::{Label, LabelContext, RepoMappingTable, RepoName};
use crate::configured::ConfiguredTargetKey;
use crate::settings::BuildSetting;
use bazel_loading::{BzlLoadKey, PackageKey, RepoMappingKey};
use bazel_packages::{AttrValue, Cfg, RuleClassRef, SourcePosition, Target};
use bazel_starlark::attr_module::Attribute;
use bazel_starlark::label_value::LabelValue;
use bazel_starlark::rule_value::StarlarkRule;
use bazel_starlark::transition::{COMMAND_LINE_OPTION_PREFIX, Setting, Transition};
use bazel_starlark::{EvalContext, FileKind};
use intern::Symbol;
use skycodec::{Decode, Encode};
use skyframe::{Compute, Ctx, Key, SkyError, encoded_value};
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::{AllocList, ListRef};
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, OwnedFrozenValue, Value, ValueLike};
use std::collections::HashMap;
use std::sync::Arc;

/// The configurations the dependencies through attributes with
/// transitions or aspects are analyzed in, keyed as a split names them.
pub type AttrConfigs = Vec<(Symbol, Vec<(String, ConfigId)>)>;

/// The configurations of the dependencies through an attribute: those
/// `attr_configs` gives it, for a transition or aspects, or else the
/// execution platform's for tools, or the rule's own.
pub fn dep_configs(cfg: Cfg, attr: Symbol, config: ConfigId, exec_config: ConfigId, attr_configs: &AttrConfigs) -> Vec<(String, ConfigId)> {
    if let Some((_, configs)) = attr_configs.iter().find(|(name, _)| *name == attr) {
        return configs.clone();
    }
    match cfg {
        Cfg::Exec => vec![(String::new(), exec_config)],
        Cfg::Target | Cfg::Transition => vec![(String::new(), config)],
    }
}

/// The configurations the transitions of a Starlark rule's attributes give
/// the rule's dependencies, which `attrs` are the rule's values for.
pub async fn attr_configs(ctx: &Ctx, label: Label, rule: &OwnedFrozenValue, attrs: &[(Symbol, AttrValue)], config: ConfigId) -> Result<AttrConfigs, SkyError> {
    let transitions = attr_transitions(rule);
    if transitions.is_empty() {
        return Ok(Vec::new());
    }
    match transition_configs(ctx, &transitions, attrs, config).await {
        Ok(configs) => Ok(configs),
        Err(Failure::Graph(error)) => Err(error),
        Err(Failure::Transition { attr, error }) => Err(edge_error(ctx, label, config, attr, attrs, error).await),
    }
}

/// Why the configurations of transitions couldn't be had: a transition
/// failed, through an attribute, or something it needs did.
enum Failure {
    Transition { attr: Symbol, error: String },
    Graph(SkyError),
}

impl From<SkyError> for Failure {
    fn from(error: SkyError) -> Failure {
        Failure::Graph(error)
    }
}

/// A transition's error through an attribute, as Bazel reports it: the
/// error, then that the edge to the attribute's first dependency failed.
async fn edge_error(ctx: &Ctx, label: Label, config: ConfigId, attr: Symbol, attrs: &[(Symbol, AttrValue)], error: String) -> SkyError {
    let mut deps = Vec::new();
    if let Some((_, value)) = attrs.iter().find(|(name, _)| *name == attr) {
        value.labels(&mut deps);
    }
    let dep = deps.first().map(Label::to_string).unwrap_or_default();
    let edge = format!("on dependency edge {label} ({}) -|{attr}|-> {dep}: Errors encountered while applying Starlark transition", config.get().checksum);
    let edge = match bazel_loading::target_location(ctx, label).await {
        Some(location) => format!("{location}: {edge}"),
        None => edge,
    };
    SkyError::new(crate::configured::errors(&[&error, &edge])).with_origin(label.to_string())
}

/// A transition's error at where Bazel reports it: the implementation's
/// `def`, or the `transition()` call.
async fn located_error(ctx: &Ctx, transition: &OwnedFrozenValue, error: &TransitionError) -> String {
    let Some((location, implementation)) = Transition::from_value(transition.value()).map(|transition| (transition.location.clone(), transition.implementation.to_repr())) else {
        return error.message.clone();
    };
    let position = match error.at {
        ErrorAt::Call => location,
        ErrorAt::Implementation => def_position(ctx, &implementation).await,
    };
    let Some(position) = position else {
        return error.message.clone();
    };
    let path = match Label::parse_canonical(&position.file) {
        Ok(file) => bazel_loading::source_path(ctx, file).await,
        Err(_) => None,
    };
    let file = path.map(|path| path.display().to_string()).unwrap_or(position.file);
    format!("{file}:{}:{}: {}", position.line, position.column, error.message)
}

/// Where a function was defined: at its name in its `def`, found in its
/// file by how it prints, `<function NAME from FILE>`.
async fn def_position(ctx: &Ctx, printed: &str) -> Option<SourcePosition> {
    let (name, file) = printed.strip_prefix("<function ")?.strip_suffix('>')?.split_once(" from ")?;
    let label = Label::parse_canonical(file).ok()?;
    let source = bazel_loading::read_source(ctx, label).await?;
    let def = format!("def {name}(");
    source.lines().enumerate().find_map(|(index, line)| {
        let column = line.find(&def)?;
        Some(SourcePosition {
            file: file.to_string(),
            line: index as u32 + 1,
            column: (column + "def ".len()) as u32 + 1,
        })
    })
}

/// The configurations transitions give, each named by what it's for.
async fn transition_configs(ctx: &Ctx, transitions: &[(Symbol, OwnedFrozenValue)], attrs: &[(Symbol, AttrValue)], config: ConfigId) -> Result<AttrConfigs, Failure> {
    let (settings, mappings) = transitions_context(ctx, transitions).await?;
    let options = config.get().options.clone();
    let mut configs = Vec::with_capacity(transitions.len());
    for (attr, transition) in transitions {
        let input = OwnedInput {
            config,
            options: &options,
            start: &options,
            settings: &settings,
            mappings: &mappings,
        };
        let applied = match Module::with_temp_heap(|module| apply_owned(transition, &input, attrs, &module)) {
            Ok(applied) => applied,
            Err(error) => {
                let error = located_error(ctx, transition, &error).await;
                return Err(Failure::Transition { attr: *attr, error });
            }
        };
        let applied = applied.into_iter().map(|(key, options)| (key, ConfigId::intern(&options))).collect();
        configs.push((*attr, applied));
    }
    Ok(configs)
}

/// The build settings transitions read or write, and how the repositories
/// of the `.bzl` files that define them see others.
async fn transitions_context(ctx: &Ctx, transitions: &[(Symbol, OwnedFrozenValue)]) -> Result<(Vec<BuildSetting>, HashMap<RepoName, Arc<RepoMappingTable>>), SkyError> {
    let (labels, repos) = transitions_use(transitions);
    let settings = build_settings(ctx, &labels).await?;
    let mut mappings = HashMap::new();
    for repo in repos {
        let mapping = ctx.compute(&RepoMappingKey(repo)).await?;
        mappings.insert(repo, Arc::new(mapping.0.clone()));
    }
    Ok((settings, mappings))
}

/// The exec configuration of `config`: what tools of its targets are built
/// in, to run on `platform`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct ExecConfigKey {
    pub config: ConfigId,
    pub platform: Label,
}

impl Key for ExecConfigKey {
    type Value = TransitionedConfig;
    const KIND: &'static str = "EXEC_CONFIGURATION";

    async fn compute(&self, ctx: &Ctx) -> Result<TransitionedConfig, SkyError> {
        let builtins = ctx.compute(&bazel_loading::BuiltinsKey).await?;
        let Some(transition) = builtins.exec_transition.clone() else {
            return Ok(TransitionedConfig(self.config.get().exec(self.platform)));
        };
        let config = self.config.get();
        let baseline = exec_baseline(ctx, &transition, &config).await?;
        let mut options = exec_options(ctx, &transition, self.config, &config.options, self.platform).await?;
        options.baseline = Some(baseline);
        Ok(TransitionedConfig(ConfigId::intern(&options)))
    }
}

/// The exec configuration the directories of exec configurations are
/// named against: the exec transition of the top-level configuration, for
/// its host platform.
async fn exec_baseline(ctx: &Ctx, transition: &OwnedFrozenValue, config: &BuildConfiguration) -> Result<ConfigId, SkyError> {
    if config.is_exec {
        return Ok(config.options.baseline.unwrap_or(config.id));
    }
    let top = config.options.baseline.unwrap_or(config.id).get();
    let options = exec_options(ctx, transition, top.id, &top.options, top.host_platform).await?;
    Ok(ConfigId::intern(&options))
}

/// The options Bazel's exec transition gives, for tools that run on
/// `platform`. As in Bazel, they start from the native options' defaults,
/// with the Starlark flags and `--define`s of the options they come from,
/// so only what the transition propagates or sets carries over.
async fn exec_options(ctx: &Ctx, transition: &OwnedFrozenValue, config: ConfigId, options: &BuildOptions, platform: Label) -> Result<BuildOptions, SkyError> {
    let transitions = [(Symbol::intern("exec"), transition.clone())];
    let (settings, mappings) = transitions_context(ctx, &transitions).await?;
    let mut start = BuildOptions::default();
    start.starlark_flags = options.starlark_flags.clone();
    start.action_env = options.action_env.clone();
    let defines = options.get("define").map_err(|error| SkyError::new(error.to_string()))?.clone();
    start.set("define", defines).map_err(|error| SkyError::new(error.to_string()))?;
    let input = OwnedInput {
        config,
        options,
        start: &start,
        settings: &settings,
        mappings: &mappings,
    };
    let applied = Module::with_temp_heap(|module| apply_owned(transition, &input, &[], &module));
    let applied = applied.map_err(|error| SkyError::new(format!("applying the exec transition: {error}")))?;
    let Some((_, mut exec)) = applied.into_iter().next() else {
        return Err(SkyError::new("the exec transition gave no configuration"));
    };
    exec.baseline = None;
    exec.set("platforms", OptionValue::Labels(vec![platform])).map_err(|error| SkyError::new(error.to_string()))?;
    Ok(exec)
}

/// The key of a target requested in a configuration: in the configuration
/// its rule's incoming transition gives, if the rule has one, or in none
/// for a constraint.
pub async fn configured_key(compute: &impl Compute, label: Label, config: ConfigId) -> Result<ConfiguredTargetKey, SkyError> {
    let package = compute.compute(&PackageKey(label.package)).await?;
    let Some(Target::Rule(rule)) = package.0.target(label.name) else {
        return Ok(ConfiguredTargetKey { label, config });
    };
    let class_ref = package.0.rule_class(rule);
    if class_ref.definition.is_none() && NO_CONFIG_RULES.contains(&class_ref.kind.as_str()) {
        return Ok(ConfiguredTargetKey { label, config: bazel_config::no_config() });
    }
    let class = bazel_loading::rule_class(compute, class_ref).await?;
    if !class.incoming_transition {
        return Ok(ConfiguredTargetKey { label, config });
    }
    let transitioned = compute.compute(&RuleTransitionKey { label, config }).await?;
    Ok(ConfiguredTargetKey { label, config: transitioned.0 })
}

/// The rules Bazel analyzes in a configuration of no options, whatever
/// requests them: they define constraints, which mean the same in any.
const NO_CONFIG_RULES: [&str; 2] = ["constraint_setting", "constraint_value"];

/// The configuration a rule's incoming transition gives a target requested
/// in `config`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RuleTransitionKey {
    pub label: Label,
    pub config: ConfigId,
}

/// A configuration a transition gave.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TransitionedConfig(pub ConfigId);

encoded_value!(TransitionedConfig);

impl Key for RuleTransitionKey {
    type Value = TransitionedConfig;
    const KIND: &'static str = "RULE_TRANSITION";

    async fn compute(&self, ctx: &Ctx) -> Result<TransitionedConfig, SkyError> {
        let package = ctx.compute(&PackageKey(self.label.package)).await?;
        let Some(Target::Rule(rule)) = package.0.target(self.label.name) else {
            return Ok(TransitionedConfig(self.config));
        };
        let class_ref = package.0.rule_class(rule);
        let Some(transition) = rule_transition(ctx, class_ref).await? else {
            return Ok(TransitionedConfig(self.config));
        };
        let class = bazel_loading::rule_class(ctx, class_ref).await?;
        let attrs = crate::attrs::resolve_attrs(ctx, rule, &class, self.config).await?;
        let transitions = [(Symbol::intern("cfg"), transition)];
        let configs = match transition_configs(ctx, &transitions, &attrs, self.config).await {
            Ok(configs) => configs,
            Err(Failure::Graph(error)) => return Err(error),
            Err(Failure::Transition { error, .. }) => return Err(rule_transition_error(ctx, self.label, error).await),
        };
        match configs.first().map(|(_, configs)| configs.as_slice()) {
            Some([(_, config)]) => Ok(TransitionedConfig(*config)),
            _ => Err(SkyError::new(format!("in {} rule {}: a rule's transition can't split its targets", class_ref.kind, self.label))),
        }
    }
}

/// A rule's own transition's error, as Bazel reports it: the error, then
/// that applying it to the target failed.
async fn rule_transition_error(ctx: &Ctx, label: Label, error: String) -> SkyError {
    let failed = "Errors encountered while applying Starlark transition";
    let failed = match bazel_loading::target_location(ctx, label).await {
        Some(location) => format!("{location}: {failed}"),
        None => failed.to_string(),
    };
    SkyError::new(crate::configured::errors(&[&error, &failed])).with_origin(label.to_string())
}

/// The transition `rule(cfg = ...)` gives a Starlark rule, if any.
async fn rule_transition(ctx: &Ctx, class: &RuleClassRef) -> Result<Option<OwnedFrozenValue>, SkyError> {
    let Some((bzl, name)) = class.definition else {
        return Ok(None);
    };
    let module = ctx.compute(&BzlLoadKey(bzl)).await?;
    let Ok((rule, _)) = module.module.get_any_visibility(name.as_str()) else {
        return Ok(None);
    };
    let cfg = StarlarkRule::from_value(rule.value()).and_then(|rule| rule.option("cfg")).and_then(|cfg| cfg.unpack_frozen());
    let Some(cfg) = cfg.filter(|cfg| Transition::from_value(cfg.to_value()).is_some()) else {
        return Ok(None);
    };
    Ok(Some(rule.map(|_| cfg)))
}

/// The attributes of a rule that have transitions, and their transitions.
fn attr_transitions(rule: &OwnedFrozenValue) -> Vec<(Symbol, OwnedFrozenValue)> {
    let Some(starlark_rule) = StarlarkRule::from_value(rule.value()) else {
        return Vec::new();
    };
    let mut transitions = Vec::new();
    for (name, attribute) in &starlark_rule.attrs {
        let Some(cfg) = Attribute::from_value(attribute.to_value()).and_then(|attribute| attribute.cfg) else {
            continue;
        };
        if Transition::from_value(cfg.to_value()).is_none() {
            continue;
        }
        let Some(frozen) = cfg.to_value().unpack_frozen() else {
            continue;
        };
        transitions.push((Symbol::intern(name), rule.map(|_| frozen)));
    }
    transitions
}

/// The build settings transitions read or write, and the repositories of
/// the `.bzl` files that define them.
fn transitions_use(transitions: &[(Symbol, OwnedFrozenValue)]) -> (Vec<Label>, Vec<RepoName>) {
    let mut labels = Vec::new();
    let mut repos = Vec::new();
    for (_, transition) in transitions {
        let Some(transition) = Transition::from_value(transition.value()) else {
            continue;
        };
        for (_, setting) in transition.inputs.iter().chain(&transition.outputs) {
            if let Setting::Starlark(label) = setting {
                labels.push(*label);
            }
        }
        repos.push(transition.definition.repo());
    }
    labels.sort();
    labels.dedup();
    repos.sort();
    repos.dedup();
    (labels, repos)
}

/// Each build setting, resolved through aliases.
async fn build_settings(ctx: &Ctx, labels: &[Label]) -> Result<Vec<BuildSetting>, SkyError> {
    let mut settings = Vec::with_capacity(labels.len());
    for label in labels {
        let Some(setting) = crate::settings::build_setting(ctx, *label).await? else {
            return Err(SkyError::new(format!("transition setting '{label}' isn't a build setting")));
        };
        settings.push(setting);
    }
    Ok(settings)
}

/// What a transition kept in a `.bzl` file's heap is applied to: a
/// [`TransitionInput`], with the mappings of every repository involved.
struct OwnedInput<'a> {
    config: ConfigId,
    options: &'a BuildOptions,
    start: &'a BuildOptions,
    settings: &'a [BuildSetting],
    mappings: &'a HashMap<RepoName, Arc<RepoMappingTable>>,
}

/// Apply a transition kept in a `.bzl` file's heap, with the rule's
/// attributes as `attr`.
fn apply_owned(transition: &OwnedFrozenValue, input: &OwnedInput<'_>, attrs: &[(Symbol, AttrValue)], module: &Module) -> Result<Vec<(String, BuildOptions)>, TransitionError> {
    let heap = module.heap();
    let value = heap.access_owned_frozen_value(transition);
    let transition = Transition::from_value(value).ok_or_else(|| TransitionError::call("not a transition"))?;
    let definition = transition.definition;
    let mapping = input.mappings.get(&definition.repo()).ok_or_else(|| TransitionError::call(format!("no repository mapping for {definition}")))?;
    let attr = attr_struct(attrs, heap);
    let mut context = EvalContext::new(FileKind::Bzl, definition);
    context.mappings = input.mappings.clone();
    let input = TransitionInput {
        config: input.config,
        options: input.options,
        start: input.start,
        settings: input.settings,
        mapping,
    };
    let mut eval = Evaluator::new(module);
    eval.extra = Some(&context);
    apply(transition, &input, attr, &mut eval)
}

/// The `attr` a transition gets: the rule's attributes, with labels rather
/// than targets, leaving out Bazel's internal ones.
fn attr_struct<'v>(attrs: &[(Symbol, AttrValue)], heap: Heap<'v>) -> Value<'v> {
    let fields = attrs
        .iter()
        .filter(|(name, _)| !name.as_str().starts_with('$') && !name.as_str().starts_with(':'))
        .map(|(name, value)| (name.as_str(), bazel_starlark::convert::to_starlark(value, heap)));
    heap.alloc(AllocStruct(fields))
}

/// What a transition is applied to.
pub struct TransitionInput<'a> {
    pub config: ConfigId,
    /// The options the transition reads.
    pub options: &'a BuildOptions,
    /// The options it writes its outputs to: `options`, except for the
    /// exec transition.
    pub start: &'a BuildOptions,
    /// The Starlark build settings among the transition's inputs and
    /// outputs.
    pub settings: &'a [BuildSetting],
    /// How the `.bzl` file that defines the transition sees repositories,
    /// for labels it writes as strings.
    pub mapping: &'a RepoMappingTable,
}

/// Why a transition failed, and where Bazel reports it.
#[derive(Debug)]
pub struct TransitionError {
    pub message: String,
    pub at: ErrorAt,
}

/// Where Bazel reports a transition's error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorAt {
    /// At the implementation function's `def`: the implementation failed,
    /// or returned something that isn't settings.
    Implementation,
    /// At the `transition()` call: the settings it read or wrote can't be.
    Call,
}

impl TransitionError {
    fn implementation(error: impl std::fmt::Display) -> TransitionError {
        TransitionError {
            message: error.to_string(),
            at: ErrorAt::Implementation,
        }
    }

    fn call(error: impl std::fmt::Display) -> TransitionError {
        TransitionError {
            message: error.to_string(),
            at: ErrorAt::Call,
        }
    }
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The options a transition gives, keyed as a split names its
/// configurations: `""` for a transition to one.
pub fn apply<'v>(
    transition: &Transition<'v>,
    input: &TransitionInput<'_>,
    attr: Value<'v>,
    eval: &mut Evaluator<'v, '_, '_>,
) -> Result<Vec<(String, BuildOptions)>, TransitionError> {
    let returned = match transition.analysis_test_settings {
        Some(settings) => settings.to_value(),
        None => {
            let settings = settings_dict(transition, input, eval.heap()).map_err(TransitionError::call)?;
            eval.eval_function(transition.implementation.to_value(), &[settings, attr], &[])
                .map_err(TransitionError::implementation)?
        }
    };
    let invalid = |error: anyhow::Error| TransitionError::implementation(format!("invalid result from transition function: {error}"));
    let mut results = Vec::new();
    for (key, values) in returned_dicts(returned).map_err(invalid)? {
        let mut options = transitioned(transition, input, values)?;
        if transition.analysis_test_settings.is_some() {
            for_analysis_test(&mut options, input);
        }
        results.push((key, options));
    }
    Ok(results)
}

/// Mark options an analysis test's transition made as those of targets an
/// analysis test evaluates, as Bazel does, which names their output
/// directory too.
fn for_analysis_test(options: &mut BuildOptions, input: &TransitionInput<'_>) {
    options.set("evaluating for analysis test", OptionValue::Bool(true)).expect("a native option");
    options.baseline = input.options.baseline.or(Some(input.config));
}

/// The `settings` dict the implementation gets: each input's value.
fn settings_dict<'v>(transition: &Transition<'v>, input: &TransitionInput<'_>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let mut entries = Vec::with_capacity(transition.inputs.len());
    for (given, setting) in &transition.inputs {
        let value = match setting {
            Setting::Native(name) => native_value(input.options, name, heap)?,
            Setting::Starlark(label) => {
                let setting = build_setting(input, *label)?;
                crate::settings::starlark_value(setting.value(input.options), heap)
            }
        };
        entries.push((given.as_str(), value));
    }
    Ok(heap.alloc(AllocDict(entries)))
}

fn build_setting<'a>(input: &'a TransitionInput<'_>, label: Label) -> anyhow::Result<&'a BuildSetting> {
    input
        .settings
        .iter()
        .find(|setting| setting.label == label)
        .ok_or_else(|| anyhow::anyhow!("transition setting '{label}' isn't a build setting"))
}

/// The native option `name`'s value, as transitions see it.
pub fn native_value<'v>(options: &BuildOptions, name: &str, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let value = options.get(name).map_err(|_| unsupported_option(name))?;
    Ok(match value {
        OptionValue::Null => Value::new_none(),
        OptionValue::Bool(value) => Value::new_bool(*value),
        OptionValue::Int(value) => heap.alloc(*value as i32),
        OptionValue::String(value) => heap.alloc(value.as_str()),
        OptionValue::Label(label) => heap.alloc(LabelValue(*label)),
        OptionValue::Strings(values) => heap.alloc(AllocList(values.iter().map(String::as_str))),
        OptionValue::Labels(labels) => heap.alloc(AllocList(labels.iter().map(|label| LabelValue(*label)))),
        OptionValue::Assignments(pairs) => heap.alloc(AllocList(pairs.iter().map(|(name, value)| format!("{name}={value}")))),
    })
}

fn unsupported_option(name: &str) -> anyhow::Error {
    anyhow::anyhow!("transitions on '{COMMAND_LINE_OPTION_PREFIX}{name}' aren't supported by bb yet")
}

/// The dicts of settings an implementation returned, keyed by the name of
/// each configuration of a split.
fn returned_dicts(returned: Value<'_>) -> anyhow::Result<Vec<(String, DictRef<'_>)>> {
    if let Some(list) = ListRef::from_value(returned) {
        let mut dicts = Vec::with_capacity(list.len());
        for (index, item) in list.iter().enumerate() {
            dicts.push((index.to_string(), dict(item)?));
        }
        return Ok(dicts);
    }
    let returned = dict(returned)?;
    let is_split = returned.values().next().is_some_and(|value| DictRef::from_value(value).is_some());
    if !is_split {
        return Ok(vec![(String::new(), returned)]);
    }
    let mut dicts = Vec::with_capacity(returned.len());
    for (key, value) in returned.iter() {
        let Some(key) = key.unpack_str() else {
            anyhow::bail!("a split transition's configurations must be named by strings, not {}", key.get_type());
        };
        dicts.push((key.to_string(), dict(value)?));
    }
    Ok(dicts)
}

fn dict(value: Value<'_>) -> anyhow::Result<DictRef<'_>> {
    DictRef::from_value(value).ok_or_else(|| anyhow::anyhow!("transition function returned {}, want dictionary or list of dictionaries", value.get_type()))
}

/// The options with one returned dict's settings written. A transition
/// that changes nothing leaves the options as they were, so its targets
/// are the same as those without it.
fn transitioned(transition: &Transition<'_>, input: &TransitionInput<'_>, values: DictRef<'_>) -> Result<BuildOptions, TransitionError> {
    if values.len() == 0 {
        return Ok(input.start.clone());
    }
    let values = declared_outputs(transition, values).map_err(|error| TransitionError::implementation(format!("invalid result from transition function: {error}")))?;
    let package = transition.definition.package;
    let parse_label = |text: &str| {
        let context = LabelContext::new(package, input.mapping);
        Label::parse(text, &context).map_err(|error| error.to_string())
    };
    let mut options = input.start.clone();
    for (setting, value) in values {
        let set = match setting {
            Setting::Native(name) => set_native(&mut options, name, value, &parse_label),
            Setting::Starlark(label) => build_setting(input, *label).and_then(|setting| set_flag(&mut options, setting, value, &parse_label)),
        };
        set.map_err(TransitionError::call)?;
    }
    if options == *input.start {
        return Ok(options);
    }
    options.baseline = input.options.baseline.or(Some(input.config));
    Ok(options)
}

/// Each declared output with the value the dict gives it, which must give
/// all of them and nothing else.
fn declared_outputs<'t, 'v>(transition: &'t Transition<'_>, values: DictRef<'v>) -> anyhow::Result<Vec<(&'t Setting, Value<'v>)>> {
    let mut given_values: Vec<(&'v str, Value<'v>)> = Vec::with_capacity(values.len());
    for (key, value) in values.iter() {
        let Some(key) = key.unpack_str() else {
            anyhow::bail!("transition function returned a {} key, want strings", key.get_type());
        };
        if !transition.outputs.iter().any(|(given, _)| given == key) {
            anyhow::bail!("transition function returned undeclared output '{key}'");
        }
        given_values.push((key, value));
    }
    let mut outputs = Vec::with_capacity(transition.outputs.len());
    let mut missing = Vec::new();
    for (given, setting) in &transition.outputs {
        match given_values.iter().find(|(key, _)| key == given) {
            Some((_, value)) => outputs.push((setting, *value)),
            None => missing.push(given.as_str()),
        }
    }
    if !missing.is_empty() {
        anyhow::bail!("transition outputs [{}] were not defined by transition function", missing.join(", "));
    }
    Ok(outputs)
}

fn set_native(options: &mut BuildOptions, name: &str, value: Value<'_>, parse_label: LabelParser<'_>) -> anyhow::Result<()> {
    let option = bazel_config::native_option(name).ok_or_else(|| unsupported_option(name))?;
    let value = native_option_value(option, value, parse_label)?;
    options.set(name, value)?;
    Ok(())
}

/// A Starlark value a transition gave a native option: a value of the
/// option's own type, or text its converter reads, as Bazel accepts.
fn native_option_value(option: &NativeOption, value: Value<'_>, parse_label: LabelParser<'_>) -> anyhow::Result<OptionValue> {
    let invalid_type = || anyhow::anyhow!("Invalid value type for option '{}'", option.name);
    let convert = |text: &str| option.convert(text, parse_label).map_err(|error| anyhow::anyhow!("OptionsParsingError for option '{}': {error}", option.name));
    if value.is_none() {
        if option.is_list() {
            anyhow::bail!("'None' value not allowed for List-type option '{}'. Please use '[]' instead if trying to set option to empty value.", option.name);
        }
        return Ok(OptionValue::Null);
    }
    if let Some(text) = value.unpack_str() {
        return convert(text);
    }
    if let Some(items) = ListRef::from_value(value) {
        if !option.is_list() {
            return Err(invalid_type());
        }
        let texts: Vec<String> = items.iter().map(value_text).collect();
        return list_option_value(option, &texts, &convert);
    }
    match option.ty {
        OptionType::Bool => value.unpack_bool().map(OptionValue::Bool).ok_or_else(invalid_type),
        OptionType::Int => value.unpack_i32().map(|value| OptionValue::Int(value.into())).ok_or_else(invalid_type),
        OptionType::Label => value.downcast_ref::<LabelValue>().map(|label| OptionValue::Label(label.0)).ok_or_else(invalid_type),
        _ => Err(invalid_type()),
    }
}

/// A list's elements given to a list option: joined by commas for one
/// that reads a list, or one at a time for one given many times.
fn list_option_value(option: &NativeOption, texts: &[String], convert: &dyn Fn(&str) -> anyhow::Result<OptionValue>) -> anyhow::Result<OptionValue> {
    if texts.is_empty() {
        return Ok(empty_list(option.ty));
    }
    if !option.multiple {
        return convert(&texts.join(","));
    }
    let mut values = Vec::new();
    let mut labels = Vec::new();
    let mut pairs = Vec::new();
    for text in texts {
        match convert(text)? {
            OptionValue::Strings(more) => values.extend(more),
            OptionValue::Labels(more) => labels.extend(more),
            OptionValue::Assignments(more) => pairs.extend(more),
            other => return Ok(other),
        }
    }
    Ok(match option.ty {
        OptionType::Labels => OptionValue::Labels(labels),
        OptionType::Assignments => OptionValue::Assignments(pairs),
        _ => OptionValue::Strings(values),
    })
}

fn empty_list(ty: OptionType) -> OptionValue {
    match ty {
        OptionType::Labels => OptionValue::Labels(Vec::new()),
        OptionType::Assignments => OptionValue::Assignments(Vec::new()),
        _ => OptionValue::Strings(Vec::new()),
    }
}

/// A Starlark value as option text: strings as they are, labels in a form
/// that means the same in any repository, and other values as Starlark
/// prints them.
fn value_text(value: Value<'_>) -> String {
    if let Some(text) = value.unpack_str() {
        return text.to_string();
    }
    if let Some(label) = value.downcast_ref::<LabelValue>() {
        return label.0.canonical_form();
    }
    value.to_str()
}

/// Set a Starlark build setting to a value of its type.
fn set_flag(options: &mut BuildOptions, setting: &BuildSetting, value: Value<'_>, parse_label: LabelParser<'_>) -> anyhow::Result<()> {
    let converted = crate::settings::from_starlark(value, &setting.default, parse_label)
        .map_err(|error| anyhow::anyhow!("Error parsing value for option '{}': {error}", setting.label))?;
    let Some(converted) = converted else {
        anyhow::bail!("Invalid value type for option '{}': want {}, got {}", setting.label, setting.default.type_name(), value.get_type());
    };
    options.set_flag(setting.actual, converted, &setting.default);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bazel_config::FlagValue;
    use bazel_starlark::transition::TransitionGen;
    use starlark::environment::Globals;
    use starlark::syntax::{AstModule, Dialect};

    fn label(text: &str) -> Label {
        Label::parse_canonical(text).unwrap()
    }

    fn setting(given: &str) -> (String, Setting) {
        let setting = match given.strip_prefix(COMMAND_LINE_OPTION_PREFIX) {
            Some(name) => Setting::Native(name.to_string()),
            None => Setting::Starlark(label(given)),
        };
        (given.to_string(), setting)
    }

    /// Apply the transition `implementation` defines in the main
    /// repository, reading and writing the settings given, to the default
    /// options.
    fn run(implementation: &str, inputs: &[&str], outputs: &[&str]) -> anyhow::Result<Vec<(String, BuildOptions)>> {
        run_with(implementation, inputs, outputs, &BuildOptions::default(), label("//:defs.bzl"))
    }

    /// Apply the transition `implementation` defines in `definition` to
    /// `options`.
    fn run_with(implementation: &str, inputs: &[&str], outputs: &[&str], options: &BuildOptions, definition: Label) -> anyhow::Result<Vec<(String, BuildOptions)>> {
        let flag = BuildSetting {
            label: label("//:flag"),
            actual: label("//:flag"),
            default: FlagValue::String("default".to_string()),
            spec: bazel_packages::BuildSettingSpec { flag: true, multiple: false },
        };
        let config = ConfigId::intern(options);
        let mapping = RepoMappingTable::new(definition.repo(), Vec::new());
        Module::with_temp_heap(|module| {
            let ast = AstModule::parse("test.bzl", implementation.to_string(), &Dialect::Extended).map_err(|error| anyhow::anyhow!("{error}"))?;
            let mut eval = Evaluator::new(&module);
            eval.eval_module(ast, &Globals::standard()).map_err(|error| anyhow::anyhow!("{error}"))?;
            let function = module.get("impl").ok_or_else(|| anyhow::anyhow!("no impl"))?;
            let transition = TransitionGen {
                implementation: function,
                inputs: inputs.iter().map(|given| setting(given)).collect(),
                outputs: outputs.iter().map(|given| setting(given)).collect(),
                definition,
                exec: false,
                location: None,
                analysis_test_settings: None,
            };
            let input = TransitionInput {
                config,
                options,
                start: options,
                settings: std::slice::from_ref(&flag),
                mapping: &mapping,
            };
            let attr = module.heap().alloc(AllocStruct(Vec::<(&str, Value)>::new()));
            apply(&transition, &input, attr, &mut eval).map_err(|error| anyhow::anyhow!("{error}"))
        })
    }

    #[test]
    fn transitions_write_native_options_and_flags() {
        let code = "def impl(settings, attr):\n    return {'//command_line_option:copt': settings['//command_line_option:copt'] + ['-DX'], '//:flag': settings['//:flag'] + '!'}";
        let options = ["//command_line_option:copt", "//:flag"];
        let applied = run(code, &options, &options).unwrap();
        let [(key, options)] = applied.as_slice() else {
            panic!("{applied:?}");
        };
        assert_eq!(key, "");
        assert_eq!(options.strings("copt"), ["-DX"]);
        assert_eq!(options.starlark_flags, [(label("//:flag"), FlagValue::String("default!".to_string()))]);
        assert!(options.baseline.is_some());
    }

    #[test]
    fn splits_are_named_by_their_keys() {
        let code = "def impl(settings, attr):\n    return {'a': {'//:flag': 'x'}, 'b': {'//:flag': 'y'}}";
        let applied = run(code, &[], &["//:flag"]).unwrap();
        let keys: Vec<&str> = applied.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["a", "b"]);
        let code = "def impl(settings, attr):\n    return [{'//:flag': 'x'}, {'//:flag': 'y'}]";
        let keys: Vec<String> = run(code, &[], &["//:flag"]).unwrap().into_iter().map(|(key, _)| key).collect();
        assert_eq!(keys, ["0", "1"]);
    }

    #[test]
    fn transitions_that_change_nothing_keep_the_options() {
        let code = "def impl(settings, attr):\n    return {'//:flag': 'default', '//command_line_option:compilation_mode': 'fastbuild'}";
        let applied = run(code, &[], &["//:flag", "//command_line_option:compilation_mode"]).unwrap();
        assert_eq!(applied[0].1, BuildOptions::default());
    }

    #[test]
    fn lists_given_to_options_used_many_times_add_each_element() {
        let code = "def impl(settings, attr):\n    return {'//command_line_option:define': ['a=1', 'b=2'], '//command_line_option:platforms': ['@@p//:x']}";
        let applied = run(code, &[], &["//command_line_option:define", "//command_line_option:platforms"]).unwrap();
        let options = &applied[0].1;
        assert_eq!(options.define("b"), Some("2"));
        assert_eq!(options.labels("platforms"), [label("@@p//:x")]);
    }

    #[test]
    fn labels_given_back_keep_their_repository() {
        let mut options = BuildOptions::default();
        options.set("platforms", OptionValue::Labels(vec![label("//p:x")])).unwrap();
        let code = "def impl(settings, attr):\n    return {'//command_line_option:platforms': settings['//command_line_option:platforms']}";
        let platforms = ["//command_line_option:platforms"];
        let applied = run_with(code, &platforms, &platforms, &options, label("@@other+//:defs.bzl")).unwrap();
        assert_eq!(applied[0].1.labels("platforms"), [label("//p:x")]);
    }

    #[test]
    fn bad_results_are_errors_as_bazel_words_them() {
        let code = "def impl(settings, attr):\n    return {'//:other': 'x'}";
        let error = run(code, &[], &["//:flag"]).unwrap_err().to_string();
        assert_eq!(error, "invalid result from transition function: transition function returned undeclared output '//:other'");
        let code = "def impl(settings, attr):\n    return {}";
        assert!(run(code, &[], &["//:flag"]).is_ok());
        let code = "def impl(settings, attr):\n    return {'//command_line_option:copt': 3}";
        let error = run(code, &[], &["//command_line_option:copt"]).unwrap_err().to_string();
        assert_eq!(error, "Invalid value type for option 'copt'");
        let code = "def impl(settings, attr):\n    return {'//command_line_option:compilation_mode': 'fast'}";
        let error = run(code, &[], &["//command_line_option:compilation_mode"]).unwrap_err().to_string();
        assert_eq!(error, "OptionsParsingError for option 'compilation_mode': Not a valid compilation mode: 'fast' (should be fastbuild, dbg or opt)");
    }
}
