//! Aspects: analyses applied over targets, along the attributes they
//! propagate along, adding providers and actions to each.
//!
//! A [`ConfiguredTargetKey`] whose configuration names aspects is the last
//! of them applied to its target, with the earlier ones applied before.
//! Its value has only the providers that aspect returns; a dependent sees
//! the target's and every aspect's merged.

use crate::build_ctx::{AspectView, Dep};
use crate::configured::{
    self, ConfiguredTarget, ConfiguredTargetKey, Implementation, RuleSetup, Setup, analyze_deps, configured, evaluate_reporting, exec_config, heap_name,
    rule_input, setup,
};
use crate::transition::{AttrConfigs, dep_configs};
use bazel_config::{AspectDescriptor, ConfigId};
use bazel_label::Label;
use bazel_loading::{BzlLoadKey, BzlModule};
use bazel_progress::Progress;
use bazel_packages::{AttrDefault, AttrSchema, AttrType, AttrValue, Package, Rule, RuleClass, Target};
use bazel_starlark::aspect::{Aspect, AspectGen};
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

    fn get(&self) -> &AspectGen<Value<'_>> {
        Aspect::from_value(self.value.value()).expect("checked when loaded")
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

/// Values as Bazel lists them in errors: `'a', 'b', or 'c'`.
fn english_list(values: &[String]) -> String {
    let quoted: Vec<String> = values.iter().map(|value| format!("'{value}'")).collect();
    match quoted.as_slice() {
        [] => "nothing".to_string(),
        [one] => one.clone(),
        [first, second] => format!("{first} or {second}"),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
    }
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
    let Target::Rule(rule) = target else {
        return nothing_provided(ctx, key, plain.kind);
    };
    if plain.incompatible.is_some() {
        return nothing_provided(ctx, key, plain.kind);
    }
    let class_ref = package.rule_class(rule).clone();
    if class_ref.definition.is_none() && class_ref.kind.as_str() == "alias" {
        return configured::alias(ctx, key, rule).await;
    }
    let mut aspects = Vec::with_capacity(config.aspects.len());
    for descriptor in &config.aspects {
        aspects.push(LoadedAspect::load(ctx, descriptor.file, &descriptor.name).await?);
    }
    let class = bazel_loading::rule_class(ctx, &class_ref).await?;
    let aspect = aspects.last().expect("a configuration naming aspects");
    if !aspect.get().applies_to(&class.advertised_providers) {
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
    propagate(&mut setup, &config.aspects, aspects);
    let mut deps = analyze_deps(ctx, key.label, &setup.class, &setup.attrs, setup.deps_config, setup.exec_config, &setup.attr_configs).await?;
    let own = own_setup(ctx, key.label, aspect, descriptor, setup.deps_config).await?;
    deps.extend(analyze_deps(ctx, key.label, &own.class, &own.attrs, own.deps_config, own.exec_config, &own.attr_configs).await?);
    let fingerprint = fingerprint(key, rule, &own.implementation, &deps, &targets, &own.toolchains);
    let view = rule_input(ctx, plain_key, package, setup, BTreeMap::new()).await?;
    let mut input = rule_input(ctx, key, package, own, deps).await?;
    input.aspect = Some(AspectView {
        rule: Box::new(view),
        targets,
    });
    let analyzed = evaluate_reporting(ctx, key.label, input, heap_name("aspect", key, fingerprint)).await?;
    if let Ok(progress) = ctx.global::<Progress>() {
        progress.aspect_applied();
    }
    Ok(configured(ctx, key, kind, false, analyzed, fingerprint))
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
fn propagate(setup: &mut RuleSetup, descriptors: &[AspectDescriptor], aspects: &[LoadedAspect]) {
    for schema in &setup.class.attrs {
        if !schema.is_dependency || !schema.ty.has_labels() || schema.ty.is_output() {
            continue;
        }
        let propagating: Vec<AspectDescriptor> = descriptors
            .iter()
            .zip(aspects)
            .filter(|(_, aspect)| aspect.get().propagates_along(schema.name.as_str()))
            .map(|(descriptor, _)| descriptor.clone())
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
    toolchains: &crate::toolchains::ResolvedToolchains,
) -> Fingerprint {
    let mut fingerprinter = Fingerprinter::new();
    fingerprinter.bytes(b"aspect");
    fingerprinter.bytes(configured::fingerprint(key, rule, implementation, deps, toolchains).as_bytes());
    for target in targets {
        fingerprinter.bytes(target.fingerprint.as_bytes());
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
