//! Aspects: analyses applied over targets, along the attributes they
//! propagate along, adding providers and actions to each.
//!
//! A [`ConfiguredTargetKey`] whose configuration names aspects is the last
//! of them applied to its target, with the earlier ones applied before.
//! Its value has only the providers that aspect returns; a dependent sees
//! the target's and every aspect's merged.

use crate::build_ctx::{AspectToolchain, AspectView, Dep};
use crate::configured::{
    self, ConfiguredTarget, ConfiguredTargetKey, Implementation, RuleSetup, Setup, analyze_deps, configured, evaluate_reporting, exec_config, heap_name,
    rule_input, setup,
};
use crate::transition::{AttrConfigs, configured_key, dep_configs};
use bazel_config::{AspectDescriptor, ConfigId};
use bazel_label::Label;
use bazel_loading::{BzlLoadKey, BzlModule};
use bazel_progress::Progress;
use crate::propagation::{self, Propagation, PropagationFunction, PropagationTarget};
use bazel_packages::{AttrDefault, AttrSchema, AttrType, AttrValue, Package, Rule, RuleClass, RuleClassRef, Target};
use bazel_starlark::aspect::{Aspect, AspectGen, english_list};
use intern::Symbol;
use skycodec::{Fingerprint, Fingerprinter};
use skyframe::{Compute, Ctx, SkyError};
use starlark::collections::SmallMap;
use starlark::environment::Module;
use starlark::values::{OwnedFrozenValue, Value, ValueLike};
use std::collections::BTreeMap;
use std::sync::Arc;

/// An aspect, loaded from the `.bzl` file that exports it.
pub(crate) struct LoadedAspect {
    module: Arc<BzlModule>,
    value: OwnedFrozenValue,
}

impl LoadedAspect {
    /// The aspect `file` exports as `name`.
    pub async fn load(ctx: &impl Compute, file: Label, name: &str) -> Result<LoadedAspect, SkyError> {
        let module = ctx.compute(&BzlLoadKey(file)).await?;
        let missing = || SkyError::new(format!("{name} is not exported from {file}"));
        let value = module.module.get_any_visibility(name).map_err(|_| missing())?.0;
        if Aspect::from_value(value.value()).is_none() {
            return Err(SkyError::new(format!("{name} from {file} is not an aspect")));
        }
        Ok(LoadedAspect { module, value })
    }

    pub(crate) fn get(&self) -> &AspectGen<Value<'_>> {
        Aspect::from_value(self.value.value()).expect("checked when loaded")
    }

    /// The aspect's value, for a heap to access.
    pub(crate) fn owned(&self) -> &OwnedFrozenValue {
        &self.value
    }

    /// Its attributes and toolchains, as a rule class's.
    fn class(&self) -> Result<Arc<RuleClass>, SkyError> {
        match self.get().class.get() {
            Some(class) => Ok(class.clone()),
            None => Err(SkyError::new(format!("{} isn't exported", self.get()))),
        }
    }

    fn implementation(&self) -> Result<Implementation, SkyError> {
        let implementation = self
            .value
            .clone()
            .try_map(|value| {
                let aspect = Aspect::from_value(value.to_value()).ok_or(())?;
                aspect.implementation.to_value().unpack_frozen().ok_or(())
            })
            .map_err(|()| SkyError::new(format!("{}'s implementation isn't a function", self.get())))?;
        Ok(Implementation::Starlark {
            module: self.module.clone(),
            rule: self.value.clone(),
            implementation,
        })
    }
}

/// The aspects `--aspects` names, each `file%name`, with the values
/// `--aspects_parameters` gives their parameters, or else their defaults.
pub async fn top_level(ctx: &impl Compute, aspects: &[(Label, String)], parameters: &[(String, String)]) -> Result<Vec<AspectDescriptor>, SkyError> {
    let aspects = with_required(ctx, aspects).await?;
    let mut loaded = Vec::with_capacity(aspects.len());
    for (file, name) in &aspects {
        loaded.push(LoadedAspect::load(ctx, *file, name).await?);
    }
    let mut used = Vec::new();
    for ((file, name), aspect) in aspects.iter().zip(&loaded) {
        for schema in aspect.class()?.attrs.iter().filter(|schema| is_parameter(schema)) {
            match parameters.iter().any(|(param, _)| param == schema.name.as_str()) {
                true => used.push(schema.name.as_str().to_string()),
                false if schema.mandatory => {
                    return Err(SkyError::new(format!("Missing mandatory attribute '{}' for aspect '{file}%{name}'.", schema.name)));
                }
                false => {}
            }
        }
    }
    let unused: Vec<&str> = parameters.iter().map(|(param, _)| param.as_str()).filter(|param| !used.iter().any(|used| used == param)).collect();
    if !unused.is_empty() {
        return Err(SkyError::new(format!(
            "Parameters '[{}]' are not parameters of any of the top-level aspects but they are specified in --aspects_parameters.",
            unused.join(", ")
        )));
    }
    let mut descriptors = Vec::with_capacity(aspects.len());
    for ((file, name), aspect) in aspects.iter().zip(&loaded) {
        let shown = format!("{file}%{name}");
        let mut params = Vec::new();
        for schema in aspect.class()?.attrs.iter().filter(|schema| is_parameter(schema)) {
            let given = parameters.iter().rev().find(|(param, _)| param == schema.name.as_str());
            let text = match given {
                Some((_, value)) => parameter_text(&shown, schema, value)?,
                None => default_text(schema),
            };
            if let Some(text) = text {
                params.push((schema.name.as_str().to_string(), text));
            }
        }
        params.sort();
        descriptors.push(AspectDescriptor {
            file: *file,
            name: name.clone(),
            params,
        });
    }
    Ok(descriptors)
}

/// Aspects, each `file%name`, each after the aspects it requires: each
/// once.
async fn with_required(ctx: &impl Compute, aspects: &[(Label, String)]) -> Result<Vec<(Label, String)>, SkyError> {
    let mut all = Vec::with_capacity(aspects.len());
    for (file, name) in aspects {
        let aspect = LoadedAspect::load(ctx, *file, name).await?;
        let required = aspect.get().required_aspects().into_iter().map(|required| (required.file, required.name.to_string()));
        for aspect in required.chain([(*file, name.clone())]) {
            if !all.contains(&aspect) {
                all.push(aspect);
            }
        }
    }
    Ok(all)
}

/// Whether an aspect's attribute is a parameter: public, rather than
/// its own private matter.
fn is_parameter(schema: &AttrSchema) -> bool {
    let name = schema.name.as_str();
    !name.starts_with('_') && !name.starts_with('$') && !name.starts_with(':')
}

/// A parameter given on the command line, checked against its type and
/// allowed values, as a descriptor writes it.
fn parameter_text(aspect: &str, schema: &AttrSchema, value: &str) -> Result<Option<String>, SkyError> {
    let expected = |ty: &str| SkyError::new(format!("{aspect}: expected value of type '{ty}' for attribute '{}' but got '{value}'", schema.name));
    let text = match schema.ty {
        AttrType::String => quote(value),
        AttrType::Int => parse_int(value).ok_or_else(|| expected("int"))?.to_string(),
        AttrType::Bool => match value.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "t" | "y" => "True".to_string(),
            "false" | "0" | "no" | "f" | "n" => "False".to_string(),
            lowered => return Err(SkyError::new(format!("{aspect}: expected value of type 'bool' for attribute '{}' but got '{lowered}'", schema.name))),
        },
        _ => return Ok(None),
    };
    if !schema.values.is_empty() && schema.ty == AttrType::String && !schema.values.iter().any(|allowed| allowed == value) {
        return Err(SkyError::new(format!(
            "{aspect}: invalid value in '{}' attribute: has to be one of {} instead of '{value}'",
            schema.name,
            english_list(&schema.values)
        )));
    }
    Ok(Some(text))
}

/// An integer as Starlark's `int(value, 0)` reads it: in decimal, or with a
/// `0x`, `0o` or `0b` prefix.
fn parse_int(value: &str) -> Option<i64> {
    let (negative, digits) = match value.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let lowered = digits.to_ascii_lowercase();
    let (radix, digits) = match lowered.get(..2) {
        Some("0x") => (16, &lowered[2..]),
        Some("0o") => (8, &lowered[2..]),
        Some("0b") => (2, &lowered[2..]),
        _ => (10, lowered.as_str()),
    };
    let magnitude = i64::from_str_radix(digits, radix).ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

/// A parameter's default, as a descriptor writes it.
fn default_text(schema: &AttrSchema) -> Option<String> {
    match &schema.default {
        AttrDefault::Value(value) => value_text(value),
        AttrDefault::Computed | AttrDefault::LateBound(_) => None,
    }
}

/// The configurations of a rule's dependencies with the aspects its
/// attributes attach applied: `attr_configs`, with an entry for each
/// attribute that attaches aspects.
pub(crate) async fn with_attached_aspects(
    ctx: &Ctx,
    class: &RuleClass,
    attrs: &[(Symbol, AttrValue)],
    config: ConfigId,
    exec_config: ConfigId,
    mut attr_configs: AttrConfigs,
) -> Result<AttrConfigs, SkyError> {
    for schema in class.attrs.iter().filter(|schema| !schema.aspects.is_empty()) {
        let given: Vec<(Label, String)> = schema.aspects.iter().map(|aspect| (aspect.file, aspect.name.to_string())).collect();
        let mut descriptors = Vec::with_capacity(given.len());
        for (file, name) in with_required(ctx, &given).await? {
            descriptors.push(attached(ctx, file, &name, class, attrs).await?);
        }
        let configs = dep_configs(schema.cfg, schema.name, config, exec_config, &attr_configs);
        let configs = configs.into_iter().map(|(key, config)| (key, config.get().with_aspects(&descriptors))).collect();
        attr_configs.retain(|(name, _)| *name != schema.name);
        attr_configs.push((schema.name, configs));
    }
    Ok(attr_configs)
}

/// An aspect an attribute attaches, with the parameters the rule's
/// attributes of the same names and types give it.
async fn attached(ctx: &Ctx, file: Label, name: &str, rule_class: &RuleClass, attrs: &[(Symbol, AttrValue)]) -> Result<AspectDescriptor, SkyError> {
    let loaded = LoadedAspect::load(ctx, file, name).await?;
    let mut params = Vec::new();
    for schema in loaded.class()?.attrs.iter().filter(|schema| is_parameter(schema)) {
        let same_type = rule_class.attrs.iter().any(|attr| attr.name == schema.name && attr.ty == schema.ty);
        let value = attrs.iter().find(|(name, _)| *name == schema.name).and_then(|(_, value)| value_text(value));
        if let (true, Some(value)) = (same_type, value) {
            params.push((schema.name.as_str().to_string(), value));
        }
    }
    params.sort();
    Ok(AspectDescriptor {
        file,
        name: name.to_string(),
        params,
    })
}

/// A parameter's value, as a descriptor writes it: as Starlark would.
fn value_text(value: &AttrValue) -> Option<String> {
    match value {
        AttrValue::String(text) => Some(quote(text)),
        AttrValue::Int(value) => Some(value.to_string()),
        AttrValue::Bool(true) => Some("True".to_string()),
        AttrValue::Bool(false) => Some("False".to_string()),
        _ => None,
    }
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A parameter's value, read back from a descriptor as the attribute's
/// type.
fn parameter_value(schema: &AttrSchema, text: &str) -> Option<AttrValue> {
    match schema.ty {
        AttrType::String => bazel_config::unquoted(text).map(AttrValue::String),
        AttrType::Int => text.parse().ok().map(AttrValue::Int),
        AttrType::Bool => Some(AttrValue::Bool(text == "True")),
        _ => None,
    }
}

/// A dependency with aspects applied: the target, then each aspect's
/// application to it in turn.
pub(crate) async fn applied(ctx: &Ctx, target: Arc<ConfiguredTarget>, aspects: &[AspectDescriptor]) -> Result<Dep, SkyError> {
    Ok(Dep::Aspect(with_applications(ctx, target, aspects).await?))
}

/// A target, then each aspect's application to it, each after those
/// before it.
async fn with_applications(ctx: &Ctx, target: Arc<ConfiguredTarget>, aspects: &[AspectDescriptor]) -> Result<Vec<Arc<ConfiguredTarget>>, SkyError> {
    let plain = ConfiguredTargetKey {
        label: target.label,
        config: target.config,
    };
    let keys = application_keys(ctx, plain, aspects).await?;
    let mut chain = vec![target];
    for applied in ctx.compute_many(&keys).await {
        chain.push(applied?);
    }
    Ok(chain)
}

/// The key of each aspect's application to a target, `plain`, after those
/// before it. As Bazel keys them, each names only the aspects before it
/// that it sees, directly or through those it sees.
pub async fn application_keys(ctx: &impl Compute, plain: ConfiguredTargetKey, aspects: &[AspectDescriptor]) -> Result<Vec<ConfiguredTargetKey>, SkyError> {
    let mut loaded = Vec::with_capacity(aspects.len());
    for descriptor in aspects {
        loaded.push(LoadedAspect::load(ctx, descriptor.file, &descriptor.name).await?);
    }
    let config = plain.config.get();
    let mut keys = Vec::with_capacity(aspects.len());
    for last in 0..aspects.len() {
        let seen: Vec<AspectDescriptor> = seen_by(&loaded[..=last]).into_iter().map(|index| aspects[index].clone()).collect();
        keys.push(ConfiguredTargetKey {
            label: plain.label,
            config: config.with_aspects(&seen),
        });
    }
    Ok(keys)
}

/// The aspects the last of `aspects` sees, directly or through those it
/// sees, and itself: their indices, in order.
fn seen_by(aspects: &[LoadedAspect]) -> Vec<usize> {
    let last = aspects.len() - 1;
    let mut seen = vec![last];
    for earlier in (0..last).rev() {
        if seen.iter().any(|later| aspects[*later].get().sees(aspects[earlier].get())) {
            seen.push(earlier);
        }
    }
    seen.reverse();
    seen
}

/// Analyze the last aspect `key`'s configuration names, applied to its
/// target.
pub(crate) async fn analyze(ctx: &Ctx, key: ConfiguredTargetKey, package: &Package, target: &Target) -> Result<ConfiguredTarget, SkyError> {
    let config = key.config.get();
    let plain_key = ConfiguredTargetKey {
        label: key.label,
        config: config.without_aspects(),
    };
    let plain = ctx.compute(&plain_key).await?;
    if plain.incompatible.is_some() {
        return nothing_provided(ctx, key, plain.kind);
    }
    let mut aspects = Vec::with_capacity(config.aspects.len());
    for descriptor in &config.aspects {
        aspects.push(LoadedAspect::load(ctx, descriptor.file, &descriptor.name).await?);
    }
    let aspect = aspects.last().expect("a configuration naming aspects");
    let rule = match target {
        Target::Rule(rule) => rule,
        Target::OutputFile { rule, .. } if aspect.get().apply_to_generating_rules => return on_generating_rule(ctx, key, *rule).await,
        _ => return nothing_provided(ctx, key, plain.kind),
    };
    let class_ref = package.rule_class(rule).clone();
    if class_ref.definition.is_none() && class_ref.kind.as_str() == "alias" {
        return configured::alias(ctx, key, rule).await;
    }
    let class = bazel_loading::rule_class(ctx, &class_ref).await?;
    let descriptor = config.aspects.last().expect("a configuration naming aspects");
    if !aspect.get().applies_to(&class.advertised_providers) || !predicate_allows(ctx, aspect, descriptor, key.label, rule, &class_ref).await? {
        return nothing_provided(ctx, key, plain.kind);
    }
    let setup = match setup(ctx, plain_key, rule, &class_ref, class).await? {
        Setup::Ready(setup) => setup,
        Setup::Incompatible(_) => return nothing_provided(ctx, key, plain.kind),
    };
    match apply(ctx, key, package, rule, plain, setup, &aspects).await {
        Err(error) if error.origin().is_none() => Err(failed(plain_key, error)),
        applied => applied,
    }
}

/// Whether an aspect's `propagation_predicate`, if it has one, lets it
/// apply to a rule's target, `label`, as Bazel asks it: of the target's
/// label, kind and tags, and the aspect's parameters.
async fn predicate_allows(ctx: &Ctx, aspect: &LoadedAspect, descriptor: &AspectDescriptor, label: Label, rule: &Rule, class_ref: &RuleClassRef) -> Result<bool, SkyError> {
    let tags = rule.attr(Symbol::intern("tags")).cloned().unwrap_or(AttrValue::StringList(Vec::new()));
    let target = PropagationTarget {
        label,
        class_ref,
        attrs: vec![(Symbol::intern("tags"), tags, false)],
    };
    match propagation::call(ctx, aspect, PropagationFunction::Predicate, descriptor, &target).await? {
        Some(Propagation::Allowed(allowed)) => Ok(allowed),
        _ => Ok(true),
    }
}

/// An aspect applied to a generated file, which applies to the rule
/// that generates it: what its application to that rule, `rule`,
/// provides.
async fn on_generating_rule(ctx: &Ctx, key: ConfiguredTargetKey, rule: Symbol) -> Result<ConfiguredTarget, SkyError> {
    let config = key.config.get();
    let owner = configured_key(ctx, key.label.sibling(rule), config.without_aspects()).await?;
    let applied_key = ConfiguredTargetKey {
        label: owner.label,
        config: owner.config.get().with_aspects(&config.aspects),
    };
    let applied = ctx.compute(&applied_key).await?;
    if let Ok(progress) = ctx.global::<Progress>() {
        progress.aspect_applied();
    }
    configured::forwarded(key, &applied)
}

/// An aspect's error as Bazel reports it: then that the analysis of its
/// target failed, which Bazel doesn't place, with the target as the
/// error's origin.
fn failed(key: ConfiguredTargetKey, error: SkyError) -> SkyError {
    let failed = format!("Analysis of target '{}' (config: {}) failed", key.label, key.config.get().checksum);
    SkyError::with_kind(error.kind(), configured::errors(&[error.message(), &failed])).with_origin(key.label.to_string())
}

/// Apply the last of `aspects` to a rule's target, `plain`, set up as it
/// was analyzed.
async fn apply(
    ctx: &Ctx,
    key: ConfiguredTargetKey,
    package: &Package,
    rule: &Rule,
    plain: Arc<ConfiguredTarget>,
    mut setup: RuleSetup,
    aspects: &[LoadedAspect],
) -> Result<ConfiguredTarget, SkyError> {
    let config = key.config.get();
    let descriptor = config.aspects.last().expect("a configuration naming aspects");
    let aspect = aspects.last().expect("a configuration naming aspects");
    let kind = plain.kind;
    let plain_key = ConfiguredTargetKey {
        label: plain.label,
        config: plain.config,
    };
    let before = &config.aspects[..config.aspects.len() - 1];
    let targets = with_applications(ctx, plain, before).await?;
    let class_ref = package.rule_class(rule).clone();
    let edges = propagation_edges(ctx, key.label, &class_ref, &setup, &config.aspects, aspects).await?;
    let toolchains = on_toolchains(ctx, &setup.toolchains, &config.aspects, aspects, &edges).await?;
    propagate(&mut setup, &config.aspects, aspects, &edges);
    let mut deps = analyze_deps(ctx, key.label, &setup.class, &setup.attrs, setup.deps_config, setup.exec_config, &setup.attr_configs).await?;
    let own = own_setup(ctx, key.label, aspect, descriptor, setup.deps_config).await?;
    let own_deps = analyze_deps(ctx, key.label, &own.class, &own.attrs, own.deps_config, own.exec_config, &own.attr_configs).await?;
    let edges = configured::aspect_edges(&own, &own_deps)?;
    let exec_platform = own.toolchains.exec_platform.unwrap_or(config.host_platform);
    deps.extend(own_deps);
    let applications = configured::aspect_applications(&deps);
    let fingerprint = fingerprint(key, rule, &own.implementation, &deps, &targets, &toolchains, &own.toolchains);
    let view = rule_input(ctx, plain_key, package, setup, BTreeMap::new()).await?;
    let mut input = rule_input(ctx, key, package, own, deps).await?;
    input.aspect = Some(AspectView {
        rule: Box::new(view),
        targets,
        toolchains,
    });
    let analyzed = evaluate_reporting(ctx, key.label, input, heap_name("aspect", key, fingerprint)).await?;
    if let Ok(progress) = ctx.global::<Progress>() {
        progress.aspect_applied();
    }
    let mut applied = configured(ctx, key, kind, false, analyzed, fingerprint);
    applied.deps = edges;
    applied.aspects = applications;
    applied.exec_platform = Some(exec_platform);
    Ok(applied)
}

/// Where each aspect propagates from a target, if a function of it
/// decides: the attributes, and the toolchain types.
#[derive(Default)]
struct Edges {
    attrs: Option<Vec<String>>,
    toolchain_types: Option<Vec<Label>>,
}

/// Where each of `aspects` propagates from a rule's target, `label`, set up
/// as `setup`, as the functions they have in place of `attr_aspects` and
/// `toolchains_aspects` decide.
async fn propagation_edges(
    ctx: &Ctx,
    label: Label,
    class_ref: &RuleClassRef,
    setup: &RuleSetup,
    descriptors: &[AspectDescriptor],
    aspects: &[LoadedAspect],
) -> Result<Vec<Edges>, SkyError> {
    let target = PropagationTarget {
        label,
        class_ref,
        attrs: PropagationTarget::attrs_of(&setup.class.attrs, &setup.attrs),
    };
    let mut edges = Vec::with_capacity(aspects.len());
    for (descriptor, aspect) in descriptors.iter().zip(aspects) {
        let mut aspect_edges = Edges::default();
        if let Some(Propagation::Attrs(attrs)) = propagation::call(ctx, aspect, PropagationFunction::AttrAspects, descriptor, &target).await? {
            aspect_edges.attrs = Some(attrs);
        }
        if let Some(Propagation::ToolchainTypes(types)) = propagation::call(ctx, aspect, PropagationFunction::ToolchainsAspects, descriptor, &target).await? {
            aspect_edges.toolchain_types = Some(types);
        }
        edges.push(aspect_edges);
    }
    Ok(edges)
}

/// The toolchains of a target, `resolved` for it, as aspects applied to
/// it see them: each with the applications to its target of those that
/// propagate to toolchains of its type.
async fn on_toolchains(
    ctx: &Ctx,
    resolved: &crate::toolchains::ResolvedToolchains,
    descriptors: &[AspectDescriptor],
    aspects: &[LoadedAspect],
    edges: &[Edges],
) -> Result<Vec<AspectToolchain>, SkyError> {
    let mut toolchains = Vec::with_capacity(resolved.resolved.len());
    for (toolchain_type, target) in &resolved.resolved {
        let Some(target) = target else {
            toolchains.push(AspectToolchain {
                toolchain_type: *toolchain_type,
                target: None,
            });
            continue;
        };
        let actual = crate::platform::actual(ctx, *toolchain_type).await?;
        let mut propagating = Vec::new();
        for ((descriptor, aspect), edges) in descriptors.iter().zip(aspects).zip(edges) {
            if propagates_to_toolchains(ctx, aspect, edges, actual).await? {
                propagating.push(descriptor.clone());
            }
        }
        let mut applied = with_applications(ctx, target.clone(), &propagating).await?;
        applied.remove(0);
        toolchains.push(AspectToolchain {
            toolchain_type: *toolchain_type,
            target: Some((target.label, applied)),
        });
    }
    Ok(toolchains)
}

/// Whether an aspect propagates to the toolchains of a type, by its
/// actual label: whether its `toolchains_aspects`, or the function in its
/// place, names it, or an alias of it, or every type.
async fn propagates_to_toolchains(ctx: &Ctx, aspect: &LoadedAspect, edges: &Edges, toolchain_type: Label) -> Result<bool, SkyError> {
    let propagated = match &edges.toolchain_types {
        Some(types) => types.clone(),
        None if aspect.get().propagates_to_toolchains(toolchain_type) => return Ok(true),
        None => aspect.get().propagated_toolchains.get().cloned().unwrap_or_default(),
    };
    for propagated in propagated {
        if crate::platform::actual(ctx, propagated).await? == toolchain_type {
            return Ok(true);
        }
    }
    Ok(false)
}

/// An aspect's own analysis, set up as a rule's is: its attributes, with
/// the parameters `descriptor` gives, its toolchains, and the
/// configuration of its tools, for a target whose dependencies are built
/// in `deps_config`.
async fn own_setup(ctx: &Ctx, label: Label, aspect: &LoadedAspect, descriptor: &AspectDescriptor, deps_config: ConfigId) -> Result<RuleSetup, SkyError> {
    let class = aspect.class()?;
    let attrs = aspect_attrs(&class, descriptor);
    let config = deps_config.get();
    let toolchains = crate::toolchains::resolve(ctx, label, &class, &config, &[]).await?;
    let exec_platform = toolchains.exec_platform.unwrap_or(config.host_platform);
    let exec_config = exec_config(ctx, &class, deps_config, exec_platform).await?;
    Ok(RuleSetup {
        class,
        attrs,
        implementation: aspect.implementation()?,
        deps_config,
        attr_configs: Vec::new(),
        toolchains,
        exec_config,
    })
}

/// Apply the aspects to the dependencies of the attributes they propagate
/// along, as the rule's `ctx.rule.attr` sees them.
fn propagate(setup: &mut RuleSetup, descriptors: &[AspectDescriptor], aspects: &[LoadedAspect], edges: &[Edges]) {
    for schema in &setup.class.attrs {
        if !schema.is_dependency || !schema.ty.has_labels() || schema.ty.is_output() {
            continue;
        }
        let name = schema.name.as_str();
        let propagating: Vec<AspectDescriptor> = descriptors
            .iter()
            .zip(aspects)
            .zip(edges)
            .filter(|((_, aspect), edges)| match &edges.attrs {
                Some(attrs) => attrs.iter().any(|attr| attr == name),
                None => aspect.get().propagates_along(name),
            })
            .map(|((descriptor, _), _)| descriptor.clone())
            .collect();
        if propagating.is_empty() {
            continue;
        }
        let configs = dep_configs(schema.cfg, schema.name, setup.deps_config, setup.exec_config, &setup.attr_configs);
        let configs = configs.into_iter().map(|(key, config)| (key, config.get().with_aspects(&propagating))).collect();
        setup.attr_configs.retain(|(name, _)| *name != schema.name);
        setup.attr_configs.push((schema.name, configs));
    }
}

/// An aspect's attribute values: its parameters' from the descriptor, the
/// rest their defaults.
fn aspect_attrs(class: &RuleClass, descriptor: &AspectDescriptor) -> Vec<(Symbol, AttrValue)> {
    let mut attrs = Vec::with_capacity(class.attrs.len());
    for schema in &class.attrs {
        let given = descriptor.params.iter().find(|(name, _)| name.as_str() == schema.name.as_str());
        let value = match (given, &schema.default) {
            (Some((_, text)), _) => parameter_value(schema, text).unwrap_or(AttrValue::None),
            (None, AttrDefault::Value(value)) => value.clone(),
            (None, _) => AttrValue::None,
        };
        attrs.push((schema.name, value));
    }
    attrs
}

/// Identifies what an aspect's application depends on.
fn fingerprint(
    key: ConfiguredTargetKey,
    rule: &Rule,
    implementation: &Implementation,
    deps: &BTreeMap<(Label, ConfigId), Dep>,
    targets: &[Arc<ConfiguredTarget>],
    rule_toolchains: &[AspectToolchain],
    toolchains: &crate::toolchains::ResolvedToolchains,
) -> Fingerprint {
    let mut fingerprinter = Fingerprinter::new();
    fingerprinter.bytes(b"aspect");
    fingerprinter.bytes(configured::fingerprint(key, rule, implementation, deps, toolchains).as_bytes());
    for target in targets {
        fingerprinter.bytes(target.fingerprint.as_bytes());
    }
    for toolchain in rule_toolchains {
        fingerprinter.bytes(toolchain.toolchain_type.to_string().as_bytes());
        let applied = toolchain.target.iter().flat_map(|(_, applied)| applied);
        for target in applied {
            fingerprinter.bytes(target.fingerprint.as_bytes());
        }
    }
    fingerprinter.finish()
}

/// An aspect's application to a target it doesn't apply to: it provides
/// nothing.
fn nothing_provided(ctx: &Ctx, key: ConfiguredTargetKey, kind: Symbol) -> Result<ConfiguredTarget, SkyError> {
    let fingerprint = Fingerprint::of(format!("no aspect {} {:?}", key.label, key.config.get().options).as_bytes());
    let analyzed = Module::with_temp_heap(|module| -> anyhow::Result<configured::Analyzed> {
        let target = crate::target_value::TargetValue {
            label: key.label,
            providers: SmallMap::new(),
            is_file: false,
        };
        module.set(configured::TARGET, module.heap().alloc(target));
        let module = module.freeze_named(heap_name("no aspect", key, fingerprint)).map_err(|error| anyhow::anyhow!("{error:?}"))?;
        Ok(configured::Analyzed { module, actions: Vec::new() })
    })
    .map_err(|error| SkyError::new(format!("{}: {error}", key.label)))?;
    Ok(configured(ctx, key, kind, false, analyzed, fingerprint))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_listed_as_bazel_lists_them() {
        let values = |values: &[&str]| values.iter().map(|value| value.to_string()).collect::<Vec<_>>();
        assert_eq!(english_list(&values(&["a", "b"])), "'a' or 'b'");
        assert_eq!(english_list(&values(&["a", "b", "c"])), "'a', 'b', or 'c'");
    }

    #[test]
    fn ints_are_read_as_starlark_reads_them() {
        assert_eq!(parse_int("0x1f"), Some(31));
        assert_eq!(parse_int("-12"), Some(-12));
        assert_eq!(parse_int("1.5"), None);
    }

    #[test]
    fn quoted_parameters_read_back() {
        let text = quote(r#"a "b" \c"#);
        assert_eq!(bazel_config::unquoted(&text).as_deref(), Some(r#"a "b" \c"#));
    }
}
