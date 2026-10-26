//! `aspect()`: aspects, which run over the dependencies of the targets
//! they're applied to, adding providers and actions to each.

use crate::attr_module::Attribute;
use crate::context::EvalContext;
use crate::convert::LabelParser;
use crate::rule_value::{attr_schema, exec_group_names, provider_key, toolchain_labels};
use allocative::Allocative;
use bazel_label::Label;
use bazel_packages::{AspectRef, RuleClass};
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
    /// targets it's applied to.
    pub toolchains_aspects: Vec<V>,
    /// `exec_groups = {name: exec_group()}`: those its actions can run in.
    pub exec_groups: Option<V>,
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
        self.propagated_toolchains.get().is_some_and(|types| types.contains(&toolchain_type))
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
        let propagated = self.toolchains_aspects.iter().map(|toolchain_type| parser.label(toolchain_type.to_value())).collect::<anyhow::Result<_>>()?;
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
    /// ...)`.
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
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Aspect<'v>> {
        let _ = kwargs;
        let context = EvalContext::get(eval)?;
        let attr_aspects = items(attr_aspects, "attr_aspects")?;
        let attr_aspects = attr_aspects
            .iter()
            .map(|name| name.unpack_str().map(str::to_string).ok_or_else(|| anyhow::anyhow!("aspect(): attr_aspects must be strings")))
            .collect::<anyhow::Result<_>>()?;
        Ok(AspectGen {
            implementation,
            attr_aspects,
            attrs: declared_attrs(attrs)?,
            required_providers: self::required_providers(required_providers, "required_providers")?,
            requires: items(requires, "requires")?,
            provides: items(provides, "provides")?,
            required_aspect_providers: self::required_providers(required_aspect_providers, "required_aspect_providers")?,
            apply_to_generating_rules,
            toolchains: items(toolchains, "toolchains")?,
            toolchains_aspects: items(toolchains_aspects, "toolchains_aspects")?,
            exec_groups,
            definition: context.caller_file(eval),
            exported: OnceLock::new(),
            class: OnceLock::new(),
            keys: OnceLock::new(),
            propagated_toolchains: OnceLock::new(),
        })
    }
}
