//! `aspect()`: aspects, which run over the dependencies of the targets
//! they're applied to, adding providers and actions to each.

use crate::attr_module::Attribute;
use crate::context::EvalContext;
use crate::convert::LabelParser;
use crate::rule_value::{attr_schema, provider_key, toolchain_labels};
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
    /// The toolchain types its implementation needs.
    pub toolchains: Vec<V>,
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
    /// The keys of `required_providers`' providers, set when it's
    /// exported.
    #[freeze(identity)]
    #[allocative(skip)]
    pub required: OnceLock<Vec<Vec<String>>>,
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
        let class = self.schema(variable_name, context).map_err(starlark::Error::new_other)?;
        let required = self.required_providers.iter().map(|providers| providers.iter().filter_map(|provider| provider_key(provider.to_value())).collect()).collect();
        let _ = self.class.set(Arc::new(class));
        let _ = self.required.set(required);
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
        let Some(required) = self.required.get() else {
            return true;
        };
        required.is_empty() || required.iter().any(|providers| providers.iter().all(|provider| advertised.contains(provider)))
    }

    /// Its attributes and toolchains, as the schema of a rule class named
    /// `name`, with labels relative to the `.bzl` file that defines it.
    fn schema(&self, name: &str, context: &EvalContext) -> anyhow::Result<RuleClass> {
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
        Ok(class)
    }
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

/// `required_providers`: a list of providers, or a list of lists of them.
fn required_providers<'v>(value: Option<Value<'v>>) -> anyhow::Result<Vec<Vec<Value<'v>>>> {
    let entries = items(value, "required_providers")?;
    if entries.iter().all(|entry| crate::convert::items(*entry).is_none()) {
        return Ok(match entries.is_empty() {
            true => Vec::new(),
            false => vec![entries],
        });
    }
    entries.into_iter().map(|entry| items(Some(entry), "required_providers")).collect()
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
    /// required_providers = [], requires = [], toolchains = [], ...)`.
    fn aspect<'v>(
        implementation: Value<'v>,
        #[starlark(require = named)] attr_aspects: Option<Value<'v>>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named)] required_providers: Option<Value<'v>>,
        #[starlark(require = named)] requires: Option<Value<'v>>,
        #[starlark(require = named)] toolchains: Option<Value<'v>>,
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
            required_providers: self::required_providers(required_providers)?,
            requires: items(requires, "requires")?,
            toolchains: items(toolchains, "toolchains")?,
            definition: context.caller_file(eval),
            exported: OnceLock::new(),
            class: OnceLock::new(),
            required: OnceLock::new(),
        })
    }
}
