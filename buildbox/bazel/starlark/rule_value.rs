//! `rule()`: rules defined in `.bzl` files.

use crate::attr_module::Attribute;
use crate::context::EvalContext;
use crate::convert::{self, LabelParser};
use crate::instantiate::instantiate;
use crate::native_rules;
use allocative::Allocative;
use bazel_label::Label;
use bazel_packages::{AttrDefault, AttrSchema, AttrType, AttrValue, Cfg, RuleClass, RuleClassRef};
use intern::Symbol;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::{
    Freeze, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, StringValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};
use std::sync::OnceLock;
use std::fmt;
use std::sync::Arc;

/// A rule class defined with `rule()`. It takes its name from the global
/// it's assigned to, and can only be instantiated once it has one.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct StarlarkRuleGen<V: ValueLifetimeless> {
    pub implementation: V,
    pub attrs: Vec<(String, V)>,
    pub test: bool,
    pub executable: bool,
    /// The other arguments `rule()` was given, for analysis.
    pub options: Vec<(String, V)>,
    /// The `.bzl` file that called `rule()`.
    #[freeze(identity)]
    #[allocative(skip)]
    pub definition: Label,
    #[freeze(identity)]
    #[allocative(skip)]
    pub exported: OnceLock<Arc<RuleClass>>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<StarlarkRuleGen<To>> for StarlarkRuleGen<From> {}

starlark_complex_value!(pub StarlarkRule);

impl<'v, V: ValueLike<'v>> fmt::Display for StarlarkRuleGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.exported.get() {
            Some(class) => write!(f, "<rule {}>", class.kind),
            None => write!(f, "<rule>"),
        }
    }
}

impl<'v, V: ValueLike<'v>> StarlarkRuleGen<V> {
    /// The value `rule()` was given for `option`, such as `"toolchains"`.
    pub fn option(&self, option: &str) -> Option<Value<'v>> {
        self.options.iter().find(|(name, _)| name == option).map(|(_, value)| value.to_value())
    }

    pub fn class_ref(&self) -> Option<RuleClassRef> {
        let class = self.exported.get()?;
        Some(RuleClassRef {
            kind: class.kind,
            definition: Some((self.definition, class.kind)),
        })
    }
}

#[starlark_value(type = "rule")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for StarlarkRuleGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn export_as(&self, variable_name: &str, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<()> {
        if self.exported.get().is_some() {
            return Ok(());
        }
        let context = EvalContext::get(eval).map_err(starlark::Error::new_other)?;
        let class = schema(self, variable_name, context).map_err(starlark::Error::new_other)?;
        let _ = self.exported.set(Arc::new(class));
        Ok(())
    }

    fn invoke(&self, _me: Value<'v>, args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        let (Some(class_ref), Some(class)) = (self.class_ref(), self.exported.get()) else {
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "this rule class hasn't been exported: assign it to a global in a .bzl file"
            )));
        };
        args.no_positional_args(eval.heap())?;
        let names = args.names_map()?;
        let kwargs: Vec<(&str, Value<'v>)> = names.iter().map(|(name, value)| (name.as_str(), *value)).collect();
        instantiate(eval, class_ref, class, &kwargs).map_err(starlark::Error::new_other)?;
        Ok(Value::new_none())
    }
}

/// The rule class a `rule()` call describes, named `kind`.
fn schema<'v, V: ValueLike<'v>>(rule: &StarlarkRuleGen<V>, kind: &str, context: &EvalContext) -> anyhow::Result<RuleClass> {
    let mapping = context.mapping_for(rule.definition.repo());
    let parser = LabelParser {
        package: rule.definition.package,
        mapping: &*mapping,
    };
    let mut own = Vec::with_capacity(rule.attrs.len());
    for (name, attribute) in &rule.attrs {
        let attribute = attribute.to_value();
        let Some(attribute) = Attribute::from_value(attribute) else {
            anyhow::bail!("attribute '{name}' of rule {kind} isn't an attr.*() value");
        };
        own.push(attr_schema(name, attribute, &parser)?);
    }
    if let Some(ty) = build_setting_type(rule) {
        own.push(AttrSchema::new("build_setting_default", ty).mandatory().nonconfigurable());
    }
    own.sort_by(|a, b| a.name.cmp(&b.name));
    let mut class = native_rules::rule_class(kind, own, rule.test, rule.executable);
    class.outputs = predeclared_outputs(rule)?;
    class.toolchain_types = toolchain_types(rule, &parser)?;
    Ok(class)
}

fn attr_schema(name: &str, attribute: &Attribute<'_>, parser: &LabelParser<'_>) -> anyhow::Result<AttrSchema> {
    let ty = attribute.ty;
    let default = match attribute.default {
        None => AttrDefault::Value(AttrValue::default_for(ty)),
        Some(value) if is_computed(value) => AttrDefault::Computed,
        Some(value) if is_late_bound(value) => AttrDefault::LateBound(None),
        Some(value) => AttrDefault::Value(
            convert::attr_value(value, ty, parser, name).map_err(|error| anyhow::anyhow!("default of attribute '{name}': {error}"))?,
        ),
    };
    let cfg = if attribute.is_exec() {
        Cfg::Exec
    } else if attribute.has_transition() {
        Cfg::Transition
    } else {
        Cfg::Target
    };
    let values = attribute
        .values
        .and_then(|values| convert::strings(values.to_value()))
        .unwrap_or_default();
    Ok(AttrSchema {
        name: Symbol::intern(name),
        ty,
        default,
        mandatory: attribute.mandatory,
        configurable: attribute.configurable.unwrap_or(!name.starts_with('_')),
        cfg,
        executable: attribute.executable,
        is_dependency: ty.has_labels(),
        values,
    })
}

/// The type of a build setting rule's value, from its
/// `build_setting = config.<type>(...)`.
fn build_setting_type<'v, V: ValueLike<'v>>(rule: &StarlarkRuleGen<V>) -> Option<AttrType> {
    let setting = rule.option("build_setting").filter(|value| !value.is_none())?;
    let opaque = setting.downcast_ref::<crate::opaque::Opaque>()?;
    let ty = match opaque.type_name.trim_end_matches("()") {
        "config.bool" => AttrType::Bool,
        "config.int" => AttrType::Int,
        "config.string" => AttrType::String,
        "config.string_list" | "config.string_set" => AttrType::StringList,
        "config.label" | "config.label_flag" => AttrType::Label,
        _ => AttrType::String,
    };
    Some(ty)
}

fn is_late_bound(value: Value<'_>) -> bool {
    value
        .downcast_ref::<crate::opaque::Opaque>()
        .is_some_and(|opaque| opaque.type_name == "LateBoundDefault")
}

fn is_computed(value: Value<'_>) -> bool {
    matches!(value.get_type(), "function" | "builtin_function_or_method")
}

/// The toolchain types `rule(toolchains = [...])` names.
fn toolchain_types<'v, V: ValueLike<'v>>(rule: &StarlarkRuleGen<V>, parser: &LabelParser<'_>) -> anyhow::Result<Vec<Label>> {
    let Some(toolchains) = rule.option("toolchains").and_then(convert::items) else {
        return Ok(Vec::new());
    };
    let mut labels = Vec::with_capacity(toolchains.len());
    for toolchain in toolchains {
        if let Some(toolchain_type) = toolchain.downcast_ref::<crate::toolchain_type::ToolchainType>() {
            labels.push(toolchain_type.label);
        } else if toolchain.unpack_str().is_some() || toolchain.downcast_ref::<crate::label_value::LabelValue>().is_some() {
            labels.push(parser.label(toolchain)?);
        }
    }
    Ok(labels)
}

fn predeclared_outputs<'v, V: ValueLike<'v>>(rule: &StarlarkRuleGen<V>) -> anyhow::Result<Vec<(Symbol, String)>> {
    let Some(outputs) = rule.option("outputs") else {
        return Ok(Vec::new());
    };
    let Some(dict) = DictRef::from_value(outputs) else {
        return Ok(Vec::new());
    };
    let mut templates = Vec::new();
    for (name, template) in dict.iter() {
        let (Some(name), Some(template)) = (name.unpack_str(), template.unpack_str()) else {
            anyhow::bail!("rule outputs must map names to file name templates");
        };
        templates.push((Symbol::intern(name), template.to_string()));
    }
    Ok(templates)
}

#[starlark_module]
pub(crate) fn rule_function(builder: &mut GlobalsBuilder) {
    /// `rule(implementation, attrs = {}, test = False, executable = False, ...)`.
    fn rule<'v>(
        implementation: Value<'v>,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<StarlarkRule<'v>> {
        let context = EvalContext::get(eval)?;
        let mut attrs = Vec::new();
        let mut test = false;
        let mut executable = false;
        let mut options = Vec::new();
        for (name, value) in kwargs {
            match name.as_str() {
                "attrs" if !value.is_none() => {
                    let Some(dict) = DictRef::from_value(value) else {
                        anyhow::bail!("rule(): attrs must be a dict");
                    };
                    for (attr, descriptor) in dict.iter() {
                        let Some(attr) = attr.unpack_str() else {
                            anyhow::bail!("rule(): attribute names must be strings");
                        };
                        attrs.push((attr.to_string(), descriptor));
                    }
                }
                "test" => test = value.to_bool(),
                "executable" => executable = value.to_bool(),
                "doc" => {}
                other => options.push((other.to_string(), value)),
            }
        }
        Ok(StarlarkRule {
            implementation,
            attrs,
            test,
            executable: executable || test,
            options,
            definition: context.caller_file(eval),
            exported: OnceLock::new(),
        })
    }
}
