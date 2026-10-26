//! `aspect()`: aspects, which run over the dependencies of the targets
//! they're applied to, adding providers and actions to each.

use crate::attr_module::Attribute;
use crate::context::EvalContext;
use crate::convert::LabelParser;
use crate::rule_value::{attr_schema, exec_group_names, provider_key, toolchain_labels};
use allocative::Allocative;
use bazel_label::Label;
use bazel_packages::{AspectRef, AttrType, RuleClass};
use intern::Symbol;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::{Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, StringValue, Trace, Value, ValueLifetimeless, ValueLike, starlark_value};
use std::fmt;
use std::sync::{Arc, OnceLock};

/// An aspect. It takes its name from the global it's assigned to, which
/// identifies it, with its file, wherever it's applied.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct AspectGen<V: ValueLifetimeless> {
    pub implementation: V,
    /// The attributes it propagates along, by name, or `*` for all.
    #[freeze(identity)]
    #[allocative(skip)]
    pub attr_aspects: Vec<String>,
    /// In place of `attr_aspects`, a function of each target it's applied
    /// to that returns them.
    pub attr_aspects_function: Option<V>,
    /// Its own attributes, as `attr.*()` values: private ones with
    /// defaults, and public ones it takes as parameters.
    pub attrs: Vec<(String, V)>,
    /// The providers a target's rule must advertise for the aspect to apply
    /// to it: any one of these lists, all of whose providers it advertises.
    /// Empty to apply to every rule.
    pub required_providers: Vec<Vec<V>>,
    /// The aspects it requires, which apply before it.
    pub requires: Vec<V>,
    /// The providers it advertises, which aspects applied after it may
    /// ask to see.
    pub provides: Vec<V>,
    /// The providers an aspect applied before it must advertise for it to
    /// see that aspect's: any one of these lists, all of whose providers
    /// it advertises.
    pub required_aspect_providers: Vec<Vec<V>>,
    /// Whether, along an attribute naming a generated file, it propagates
    /// to the rule that generates the file.
    #[freeze(identity)]
    #[allocative(skip)]
    pub apply_to_generating_rules: bool,
    /// The toolchain types its implementation needs.
    pub toolchains: Vec<V>,
    /// The toolchain types it propagates to the toolchains of, in the
    /// targets it's applied to, as labels relative to its `.bzl` file, or
    /// `*` for every toolchain.
    #[freeze(identity)]
    #[allocative(skip)]
    pub toolchains_aspects: Vec<String>,
    /// In place of `toolchains_aspects`, a function of each target it's
    /// applied to that returns them.
    pub toolchains_aspects_function: Option<V>,
    /// `exec_groups = {name: exec_group()}`: those its actions can run in.
    pub exec_groups: Option<V>,
    /// A function of a target, as `propagation_predicate` gives it, that
    /// decides whether the aspect is applied to it.
    pub propagation_predicate: Option<V>,
    /// The `.bzl` file that called `aspect()`.
    #[freeze(identity)]
    #[allocative(skip)]
    pub definition: Label,
    /// The global it's assigned to, and the file that exports it.
    #[freeze(identity)]
    #[allocative(skip)]
    pub exported: OnceLock<AspectRef>,
    /// Its attributes and toolchains as a rule class's, set when it's
    /// exported, named for the aspect.
    #[freeze(identity)]
    #[allocative(skip)]
    pub class: OnceLock<Arc<RuleClass>>,
    /// The keys of the providers it needs and advertises, set when it's
    /// exported.
    #[freeze(identity)]
    #[allocative(skip)]
    pub keys: OnceLock<ProviderKeys>,
    /// `toolchains_aspects`, as labels, set when it's exported.
    #[freeze(identity)]
    #[allocative(skip)]
    pub propagated_toolchains: OnceLock<Vec<Label>>,
}

/// An aspect's providers, by their keys.
#[derive(Debug, Default)]
pub struct ProviderKeys {
    /// `required_providers`.
    pub required: Vec<Vec<String>>,
    /// `required_aspect_providers`.
    pub required_aspect: Vec<Vec<String>>,
    /// `provides`.
    pub advertised: Vec<String>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<AspectGen<To>> for AspectGen<From> {}

starlark_complex_value!(pub Aspect);

impl<'v, V: ValueLike<'v>> fmt::Display for AspectGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.exported.get() {
            Some(exported) => write!(f, "<aspect {}%{}>", exported.file, exported.name),
            None => write!(f, "<aspect>"),
        }
    }
}

#[starlark_value(type = "Aspect")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for AspectGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn export_as(&self, variable_name: &str, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<()> {
        if self.exported.get().is_some() {
            return Ok(());
        }
        let context = EvalContext::get(eval).map_err(starlark::Error::new_other)?;
        let (class, propagated_toolchains) = self.schema(variable_name, context).map_err(starlark::Error::new_other)?;
        let keys = ProviderKeys {
            required: keys_of(&self.required_providers),
            required_aspect: keys_of(&self.required_aspect_providers),
            advertised: self.provides.iter().filter_map(|provider| provider_key(provider.to_value())).collect(),
        };
        let _ = self.class.set(Arc::new(class));
        let _ = self.keys.set(keys);
        let _ = self.propagated_toolchains.set(propagated_toolchains);
        let _ = self.exported.set(AspectRef {
            file: context.file,
            name: Symbol::intern(variable_name),
        });
        Ok(())
    }
}

impl<'v, V: ValueLike<'v>> AspectGen<V> {
    /// Whether it propagates along an attribute.
    pub fn propagates_along(&self, attr: &str) -> bool {
        self.attr_aspects.iter().any(|name| name == "*" || name == attr)
    }

    /// Whether it applies to targets of a rule that advertises these
    /// providers.
    pub fn applies_to(&self, advertised: &[String]) -> bool {
        let Some(keys) = self.keys.get() else {
            return true;
        };
        keys.required.is_empty() || satisfied(&keys.required, advertised)
    }

    /// Whether, applied after `earlier`, it sees `earlier`'s providers:
    /// because it requires it, or because `earlier` advertises the
    /// providers it asks to see.
    pub fn sees(&self, earlier: &AspectGen<V>) -> bool {
        let (Some(keys), Some(earlier_keys)) = (self.keys.get(), earlier.keys.get()) else {
            return false;
        };
        let required = earlier.exported.get().is_some_and(|earlier| self.directly_required().any(|aspect| aspect.exported.get() == Some(earlier)));
        required || satisfied(&keys.required_aspect, &earlier_keys.advertised)
    }

    /// The aspects it requires, each after those they require in turn, as
    /// Bazel applies them before it: each once.
    pub fn required_aspects(&self) -> Vec<AspectRef> {
        let mut required = Vec::new();
        self.add_required(&mut required);
        required
    }

    fn add_required(&self, required: &mut Vec<AspectRef>) {
        for aspect in self.directly_required() {
            aspect.add_required(required);
            if let Some(exported) = aspect.exported.get().filter(|exported| !required.contains(exported)) {
                required.push(exported.clone());
            }
        }
    }

    fn directly_required(&self) -> impl Iterator<Item = &'v AspectGen<Value<'v>>> + '_ {
        self.requires.iter().filter_map(|aspect| Aspect::from_value(aspect.to_value()))
    }

    /// Whether it propagates to the toolchains of this type, its actual
    /// label, of the targets it's applied to.
    pub fn propagates_to_toolchains(&self, toolchain_type: Label) -> bool {
        let all = self.toolchains_aspects.iter().any(|name| name == "*");
        all || self.propagated_toolchains.get().is_some_and(|types| types.contains(&toolchain_type))
    }

    /// Its attributes, toolchains and exec groups, as the schema of a rule
    /// class named `name`, with labels relative to the `.bzl` file that
    /// defines it; and the toolchain types it propagates to.
    fn schema(&self, name: &str, context: &EvalContext) -> anyhow::Result<(RuleClass, Vec<Label>)> {
        let mapping = context.mapping_for(self.definition.repo());
        let parser = LabelParser {
            package: self.definition.package,
            mapping: &*mapping,
        };
        let mut attrs = Vec::with_capacity(self.attrs.len());
        for (attr, value) in &self.attrs {
            let Some(attribute) = Attribute::from_value(value.to_value()) else {
                anyhow::bail!("attribute '{attr}' of aspect {name} isn't an attr.*() value");
            };
            attrs.push(attr_schema(attr, attribute, &parser)?);
        }
        attrs.sort_by(|a, b| a.name.cmp(&b.name));
        let toolchains = self.toolchains.iter().map(|toolchain| toolchain.to_value()).collect();
        let (toolchain_types, optional_toolchain_types) = toolchain_labels(toolchains, &parser)?;
        let mut class = crate::native_rules::rule_class(name, Vec::new(), false, false);
        class.attrs = attrs;
        class.toolchain_types = toolchain_types;
        class.optional_toolchain_types = optional_toolchain_types;
        class.exec_groups = exec_group_names(self.exec_groups.map(|groups| groups.to_value()), "aspect")?;
        let mut propagated = Vec::with_capacity(self.toolchains_aspects.len());
        for text in self.toolchains_aspects.iter().filter(|text| *text != "*") {
            let label = parser.parse(text).map_err(|error| anyhow::anyhow!("Unable to parse label '{text}' in attribute 'toolchains_aspects': {error}"))?;
            propagated.push(label);
        }
        Ok((class, propagated))
    }
}

/// Whether `advertised` has every provider of any one of the `required`
/// lists.
fn satisfied(required: &[Vec<String>], advertised: &[String]) -> bool {
    required.iter().any(|providers| providers.iter().all(|provider| advertised.contains(provider)))
}

/// The keys of the providers in lists of them.
fn keys_of<'v, V: ValueLike<'v>>(lists: &[Vec<V>]) -> Vec<Vec<String>> {
    lists.iter().map(|providers| providers.iter().filter_map(|provider| provider_key(provider.to_value())).collect()).collect()
}

/// The aspect an `attr.label(aspects = [...])` entry is, which must be
/// exported by the time the rule is.
pub fn aspect_ref(value: Value<'_>) -> anyhow::Result<AspectRef> {
    let Some(aspect) = Aspect::from_value(value) else {
        anyhow::bail!("expected an aspect, got {} of type '{}'", value.to_repr(), value.get_type());
    };
    match aspect.exported.get() {
        Some(exported) => Ok(exported.clone()),
        None => anyhow::bail!("an aspect must be exported, assigned to a global in a .bzl file, before a rule uses it"),
    }
}

fn items<'v>(value: Option<Value<'v>>, what: &str) -> anyhow::Result<Vec<Value<'v>>> {
    let Some(value) = value.filter(|value| !value.is_none()) else {
        return Ok(Vec::new());
    };
    crate::convert::items(value).ok_or_else(|| anyhow::anyhow!("aspect(): {what} must be a list, not {}", value.get_type()))
}

/// A function, as Bazel takes one in place of a list.
fn function(value: Option<Value<'_>>) -> Option<Value<'_>> {
    value.filter(|value| value.get_type() == "function")
}

/// A list naming what an aspect propagates to, in which `*` for all must
/// be alone, as Bazel requires.
fn all_or_listed(names: Vec<String>, noun: &str, what: &str) -> anyhow::Result<Vec<String>> {
    if names.len() > 1 && names.iter().any(|name| name == "*") {
        anyhow::bail!("'*' must be the only {noun} in '{what}' list");
    }
    Ok(names)
}

/// The strings of a list, `what`, as Bazel requires them.
fn strings(values: Vec<Value<'_>>, what: &str) -> anyhow::Result<Vec<String>> {
    let mut strings = Vec::with_capacity(values.len());
    for (index, value) in values.into_iter().enumerate() {
        let Some(text) = value.unpack_str() else {
            anyhow::bail!("at index {index} of {what}, got element of type {}, want string", value.get_type());
        };
        strings.push(text.to_string());
    }
    Ok(strings)
}

/// Check an aspect's attributes as Bazel does: its public ones, its
/// parameters, must be strings, ints or bools, with defaults among their
/// `values`; its private ones must have defaults.
fn check_attrs(attrs: &[(String, Value<'_>)]) -> anyhow::Result<()> {
    for (name, value) in attrs {
        let Some(attribute) = Attribute::from_value(*value) else {
            continue;
        };
        if name.starts_with('_') {
            if attribute.default.is_none() {
                anyhow::bail!("Aspect attribute '{name}' has no default value.");
            }
            continue;
        }
        if !matches!(attribute.ty, AttrType::String | AttrType::Int | AttrType::Bool) {
            anyhow::bail!("Aspect parameter attribute '{name}' must have type 'bool', 'int' or 'string'.");
        }
        let (Some(default), Some(values)) = (attribute.default, allowed_values(&attribute)) else {
            continue;
        };
        if !values.iter().any(|value| value.equals(default).unwrap_or(false)) {
            anyhow::bail!("Aspect parameter attribute '{name}' has a bad default value: {}", not_allowed(&values, default));
        }
    }
    Ok(())
}

/// The values an attribute's `values` allows, if it restricts them.
fn allowed_values<'v>(attribute: &Attribute<'v>) -> Option<Vec<Value<'v>>> {
    crate::convert::items(attribute.values?)
}

/// Why a value isn't allowed, as Bazel words it.
pub fn not_allowed(values: &[Value<'_>], value: Value<'_>) -> String {
    let values: Vec<String> = values.iter().map(|value| value.to_str()).collect();
    format!("has to be one of {} instead of '{}'", english_list(&values), value.to_str())
}

/// Values as Bazel lists them in errors: `'a', 'b', or 'c'`.
pub fn english_list(values: &[String]) -> String {
    let quoted: Vec<String> = values.iter().map(|value| format!("'{value}'")).collect();
    match quoted.as_slice() {
        [] => "nothing".to_string(),
        [one] => one.clone(),
        [first, second] => format!("{first} or {second}"),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
    }
}

/// Check the aspects `attr.label(aspects = [...])` attaches, as Bazel does
/// when it attaches them: each must be exported, and given once, counting
/// those they require.
pub fn check_attached(aspects: Value<'_>) -> anyhow::Result<()> {
    let Some(items) = crate::convert::items(aspects) else {
        return Ok(());
    };
    let mut added = Vec::new();
    for item in items {
        if item.downcast_ref::<crate::opaque::Opaque>().is_some() {
            continue;
        }
        let Some(aspect) = Aspect::from_value(item) else {
            anyhow::bail!("expected an aspect, got {} of type '{}'", item.to_repr(), item.get_type());
        };
        add_attached(aspect, None, &mut added)?;
    }
    Ok(())
}

/// Add an aspect to those attached, after those it requires, as Bazel's
/// `AspectsList` does: by name, and whether another aspect required it.
fn add_attached(aspect: &AspectGen<Value<'_>>, required_by: Option<&str>, added: &mut Vec<(String, Option<String>)>) -> anyhow::Result<()> {
    let Some(exported) = aspect.exported.get() else {
        anyhow::bail!("Aspects should be top-level values in extension files that define them.");
    };
    let name = format!("{}%{}", exported.file, exported.name);
    for required in aspect.directly_required() {
        add_attached(required, Some(&name), added)?;
    }
    match added.iter().find(|(added, _)| *added == name) {
        None => added.push((name, required_by.map(str::to_string))),
        Some(_) if required_by.is_some() => {}
        Some((_, Some(by))) => anyhow::bail!("aspect {name} was added before as a required aspect of aspect {by}"),
        Some((_, None)) => anyhow::bail!("aspect {name} added more than once"),
    }
    Ok(())
}

/// Check, as Bazel does where a rule is used, that the aspects its
/// attributes attach can take their parameters from it: they must be
/// bools or restricted to `values`, and those without defaults must be
/// attributes of the rule, `class`, too.
pub fn check_parameters_given(attrs: &[(String, Value<'_>)], class: &RuleClass) -> anyhow::Result<()> {
    for (_, value) in attrs {
        let aspects = Attribute::from_value(*value).and_then(|attribute| attribute.aspects).and_then(crate::convert::items).unwrap_or_default();
        for aspect in aspects.into_iter().filter_map(Aspect::from_value) {
            for required in aspect.directly_required() {
                check_parameters_of(required, class)?;
            }
            check_parameters_of(aspect, class)?;
        }
    }
    Ok(())
}

fn check_parameters_of(aspect: &AspectGen<Value<'_>>, class: &RuleClass) -> anyhow::Result<()> {
    let Some(exported) = aspect.exported.get() else {
        return Ok(());
    };
    let name = format!("{}%{}", exported.file, exported.name);
    for (attr, value) in aspect.attrs.iter().filter(|(attr, _)| !attr.starts_with('_')) {
        let Some(attribute) = Attribute::from_value(*value) else {
            continue;
        };
        if attribute.ty != AttrType::Bool && attribute.values.is_none() {
            anyhow::bail!("Aspect {name}: Aspect parameter attribute '{attr}' must use the 'values' restriction.");
        }
        let has_default = attribute.default.is_some_and(|default| default.to_bool());
        let given = class.attrs.iter().any(|schema| schema.name.as_str() == attr && schema.ty == attribute.ty);
        if (!has_default || attribute.mandatory) && !given {
            let ty = match attribute.ty {
                AttrType::Bool => "boolean",
                other => other.name(),
            };
            anyhow::bail!("Aspect {name} requires rule {} to specify attribute '{attr}' with type {ty}.", class.kind);
        }
    }
    Ok(())
}

/// `required_providers` or `required_aspect_providers`: a list of
/// providers, or a list of lists of them.
fn required_providers<'v>(value: Option<Value<'v>>, what: &str) -> anyhow::Result<Vec<Vec<Value<'v>>>> {
    let entries = items(value, what)?;
    if entries.iter().all(|entry| crate::convert::items(*entry).is_none()) {
        return Ok(match entries.is_empty() {
            true => Vec::new(),
            false => vec![entries],
        });
    }
    entries.into_iter().map(|entry| items(Some(entry), what)).collect()
}

/// The aspect's attributes, from `attrs = {name: attr.*()}`.
fn declared_attrs<'v>(attrs: Option<Value<'v>>) -> anyhow::Result<Vec<(String, Value<'v>)>> {
    let Some(attrs) = attrs.filter(|attrs| !attrs.is_none()) else {
        return Ok(Vec::new());
    };
    let Some(dict) = DictRef::from_value(attrs) else {
        anyhow::bail!("aspect(): attrs must be a dict, not {}", attrs.get_type());
    };
    let mut declared = Vec::with_capacity(dict.len());
    for (name, attribute) in dict.iter() {
        let Some(name) = name.unpack_str() else {
            anyhow::bail!("aspect(): attribute names must be strings");
        };
        declared.push((name.to_string(), attribute));
    }
    Ok(declared)
}

#[starlark_module]
pub(crate) fn aspect_function(builder: &mut GlobalsBuilder) {
    /// `aspect(implementation, attr_aspects = [], attrs = {},
    /// required_providers = [], required_aspect_providers = [],
    /// provides = [], requires = [], apply_to_generating_rules = False,
    /// toolchains = [], toolchains_aspects = [], exec_groups = None,
    /// propagation_predicate = None, ...)`.
    fn aspect<'v>(
        implementation: Value<'v>,
        #[starlark(require = named)] attr_aspects: Option<Value<'v>>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named)] required_providers: Option<Value<'v>>,
        #[starlark(require = named)] required_aspect_providers: Option<Value<'v>>,
        #[starlark(require = named)] provides: Option<Value<'v>>,
        #[starlark(require = named)] requires: Option<Value<'v>>,
        #[starlark(require = named, default = false)] apply_to_generating_rules: bool,
        #[starlark(require = named)] toolchains: Option<Value<'v>>,
        #[starlark(require = named)] toolchains_aspects: Option<Value<'v>>,
        #[starlark(require = named)] exec_groups: Option<Value<'v>>,
        #[starlark(require = named)] propagation_predicate: Option<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Aspect<'v>> {
        let _ = kwargs;
        let context = EvalContext::get(eval)?;
        let (attr_aspects, attr_aspects_function) = match function(attr_aspects) {
            Some(function) => (Vec::new(), Some(function)),
            None => (all_or_listed(strings(items(attr_aspects, "attr_aspects")?, "attr_aspects")?, "string", "attr_aspects")?, None),
        };
        let (toolchains_aspects, toolchains_aspects_function) = match function(toolchains_aspects) {
            Some(function) => (Vec::new(), Some(function)),
            None => (all_or_listed(strings(items(toolchains_aspects, "toolchains_aspects")?, "toolchains_aspects")?, "item", "toolchains_aspects")?, None),
        };
        let attrs = declared_attrs(attrs)?;
        check_attrs(&attrs)?;
        let required_providers = self::required_providers(required_providers, "required_providers")?;
        if apply_to_generating_rules && !required_providers.is_empty() {
            anyhow::bail!("An aspect cannot simultaneously have required providers and apply to generating rules.");
        }
        let propagation_predicate = propagation_predicate.filter(|predicate| !predicate.is_none());
        if let Some(predicate) = propagation_predicate.filter(|predicate| predicate.get_type() != "function") {
            anyhow::bail!("in call to aspect(), parameter 'propagation_predicate' got value of type '{}', want 'function or NoneType'", predicate.get_type());
        }
        if apply_to_generating_rules && propagation_predicate.is_some() {
            anyhow::bail!("An aspect cannot simultaneously have a propagation predicate and apply to generating rules.");
        }
        Ok(AspectGen {
            implementation,
            attr_aspects,
            attr_aspects_function,
            attrs,
            required_providers,
            requires: items(requires, "requires")?,
            provides: items(provides, "provides")?,
            required_aspect_providers: self::required_providers(required_aspect_providers, "required_aspect_providers")?,
            apply_to_generating_rules,
            toolchains: items(toolchains, "toolchains")?,
            toolchains_aspects,
            toolchains_aspects_function,
            exec_groups,
            propagation_predicate,
            definition: context.caller_file(eval),
            exported: OnceLock::new(),
            class: OnceLock::new(),
            keys: OnceLock::new(),
            propagated_toolchains: OnceLock::new(),
        })
    }
}
