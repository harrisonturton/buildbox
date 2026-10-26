//! `rule()`: rules defined in `.bzl` files.

use crate::attr_module::Attribute;
use crate::context::EvalContext;
use crate::convert::{self, LabelParser};
use crate::instantiate::instantiate;
use crate::native_rules;
use allocative::Allocative;
use bazel_label::Label;
use bazel_packages::{AspectRef, AttrDefault, AttrSchema, AttrType, AttrValue, CallFrame, Cfg, RuleClass, RuleClassRef};
use intern::Symbol;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::{
    Freeze, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, StringValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::fmt;
use std::sync::Arc;

/// A rule class defined with `rule()`. It takes its name from the global
/// it's assigned to, and can only be instantiated once it has one.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagable)]
#[starlark_pagable(bound = "V: starlark::pagable::StarlarkPagable")]
#[repr(C)]
pub struct StarlarkRuleGen<V: ValueLifetimeless> {
    pub implementation: V,
    pub attrs: Vec<(String, V)>,
    pub test: bool,
    pub executable: bool,
    /// The other arguments `rule()` was given, for analysis.
    pub options: Vec<(String, V)>,
    /// The `.bzl` file that called `rule()`, which resolves its attributes'
    /// default labels.
    #[freeze(identity)]
    #[allocative(skip)]
    #[starlark_pagable(serialize_with = "crate::paging::serialize_encoded", deserialize_with = "crate::paging::deserialize_encoded")]
    pub definition: Label,
    /// The rule's schema and the `.bzl` file that exported it, which
    /// identifies the rule class; set when it's assigned to a global.
    #[freeze(identity)]
    #[allocative(skip)]
    #[starlark_pagable(serialize_with = "crate::paging::serialize_once_lock", deserialize_with = "crate::paging::deserialize_once_lock")]
    pub exported: OnceLock<Arc<RuleClass>>,
    #[freeze(identity)]
    #[allocative(skip)]
    #[starlark_pagable(serialize_with = "crate::paging::serialize_once_lock", deserialize_with = "crate::paging::deserialize_once_lock")]
    pub exported_from: OnceLock<Label>,
    /// The calls that defined the rule: where `rule()` was called.
    #[freeze(identity)]
    #[allocative(skip)]
    #[starlark_pagable(serialize_with = "crate::paging::serialize_encoded", deserialize_with = "crate::paging::deserialize_encoded")]
    pub defined_at: Vec<CallFrame>,
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
    /// Run the rule's `initializer`, if it has one, on the arguments of a
    /// call. The values it returns replace those given.
    fn initialize(&self, mut given: Vec<(String, Value<'v>)>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Vec<(String, Value<'v>)>> {
        let Some(initializer) = self.option("initializer").filter(|value| !value.is_none()) else {
            return Ok(given);
        };
        let named: Vec<(&str, Value<'v>)> = given.iter().map(|(name, value)| (name.as_str(), *value)).collect();
        let result = eval.eval_function(initializer, &[], &named)?;
        let Some(overrides) = DictRef::from_value(result) else {
            return Err(starlark::Error::new_other(anyhow::anyhow!("a rule's initializer must return a dict")));
        };
        for (name, value) in overrides.iter() {
            let Some(name) = name.unpack_str() else {
                continue;
            };
            match given.iter_mut().find(|(existing, _)| existing == name) {
                Some(entry) => entry.1 = value,
                None => given.push((name.to_string(), value)),
            }
        }
        Ok(given)
    }

    /// The value `rule()` was given for `option`, such as `"toolchains"`.
    pub fn option(&self, option: &str) -> Option<Value<'v>> {
        self.options.iter().find(|(name, _)| name == option).map(|(_, value)| value.to_value())
    }

    pub fn class_ref(&self) -> Option<RuleClassRef> {
        let class = self.exported.get()?;
        let file = self.exported_from.get().copied().unwrap_or(self.definition);
        Some(RuleClassRef {
            kind: class.kind,
            definition: Some((file, class.kind)),
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
        let _ = self.exported_from.set(context.file);
        Ok(())
    }

    /// Tracebacks name a rule as Bazel's do: by the name it's exported as.
    fn name_for_call_stack(&self, me: Value<'v>) -> String {
        match self.exported.get() {
            Some(class) => class.kind.to_string(),
            None => me.to_repr(),
        }
    }

    fn invoke(&self, _me: Value<'v>, args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        let (Some(class_ref), Some(class)) = (self.class_ref(), self.exported.get()) else {
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "this rule class hasn't been exported: assign it to a global in a .bzl file"
            )));
        };
        args.no_positional_args(eval.heap())?;
        let attrs: Vec<(String, Value<'v>)> = self.attrs.iter().map(|(name, value)| (name.clone(), value.to_value())).collect();
        crate::aspect::check_parameters_given(&attrs, class).map_err(starlark::Error::new_other)?;
        let names = args.names_map()?;
        let given: Vec<(String, Value<'v>)> = names.iter().map(|(name, value)| (name.as_str().to_string(), *value)).collect();
        let given = self.initialize(given, eval)?;
        let kwargs: Vec<(&str, Value<'v>)> = given.iter().map(|(name, value)| (name.as_str(), *value)).collect();
        let computed = computed_defaults(&self.attrs, &kwargs, eval)?;
        let computed: Vec<(&str, Value<'v>)> = computed.iter().map(|(name, value)| (name.as_str(), *value)).collect();
        instantiate(eval, class_ref, class, &kwargs, &computed).map_err(starlark::Error::new_other)?;
        Ok(Value::new_none())
    }
}

/// Evaluate the computed defaults of the attributes a call didn't give:
/// functions whose parameters name other attributes, called with their
/// values.
fn computed_defaults<'v, V: ValueLike<'v>>(
    attrs: &[(String, V)],
    kwargs: &[(&str, Value<'v>)],
    eval: &mut Evaluator<'v, '_, '_>,
) -> starlark::Result<Vec<(String, Value<'v>)>> {
    let mut computed = Vec::new();
    for (name, descriptor) in attrs {
        if kwargs.iter().any(|(given, _)| given == name) {
            continue;
        }
        let Some(attribute) = Attribute::from_value(descriptor.to_value()) else {
            continue;
        };
        let Some(function) = attribute.default.filter(|default| is_computed(*default)) else {
            continue;
        };
        let mut arguments = Vec::new();
        for parameter in parameter_names(function) {
            let value = kwargs
                .iter()
                .find(|(given, _)| **given == parameter)
                .map(|(_, value)| *value)
                .or_else(|| default_of(attrs, &parameter))
                .unwrap_or_else(|| common_default(&parameter, eval.heap()));
            arguments.push((parameter, value));
        }
        let named: Vec<(&str, Value<'v>)> = arguments.iter().map(|(name, value)| (name.as_str(), *value)).collect();
        if let Ok(value) = eval.eval_function(function, &[], &named) {
            computed.push((name.clone(), value));
        }
    }
    Ok(computed)
}

/// The names of a Starlark function's parameters.
fn parameter_names(function: Value<'_>) -> Vec<String> {
    let Some(parameters) = function.parameters_spec() else {
        return Vec::new();
    };
    let types = vec![starlark::typing::Ty::any(); parameters.len()];
    let params = parameters.documentation(types, HashMap::new());
    params.pos_or_named.into_iter().chain(params.named_only).map(|param| param.name).collect()
}

/// An attribute's plain default value, for a computed default to read.
fn default_of<'v, V: ValueLike<'v>>(attrs: &[(String, V)], name: &str) -> Option<Value<'v>> {
    let (_, descriptor) = attrs.iter().find(|(attr, _)| attr == name)?;
    let attribute = Attribute::from_value(descriptor.to_value())?;
    attribute.default.filter(|default| !is_computed(*default))
}

/// A common attribute's default, for a computed default that reads it.
fn common_default<'v>(name: &str, heap: starlark::values::Heap<'v>) -> Value<'v> {
    let common = native_rules::common_attrs();
    let Some(attr) = common.iter().find(|attr| attr.name.as_str() == name) else {
        return Value::new_none();
    };
    match &attr.default {
        AttrDefault::Value(value) => convert::to_starlark(value, heap),
        _ => Value::new_none(),
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
    for subrule in crate::subrule::subrules(rule.option("subrules"))? {
        own.extend(subrule_attrs(subrule, context)?);
    }
    if let Some(setting) = build_setting(rule) {
        own.push(AttrSchema::new("build_setting_default", build_setting_type(setting)).mandatory().nonconfigurable());
    }
    let incoming_transition = rule.option("cfg").is_some_and(|cfg| crate::transition::Transition::from_value(cfg).is_some());
    if incoming_transition || own.iter().any(|attr| attr.cfg == Cfg::Transition) {
        own.push(transition_allowlist());
    }
    let mut class = native_rules::starlark_rule_class(kind, own, rule.test, rule.executable);
    class.outputs = predeclared_outputs(rule)?;
    let (toolchain_types, optional) = toolchain_types(rule, &parser)?;
    class.toolchain_types = toolchain_types;
    class.optional_toolchain_types = optional;
    class.exec_groups = exec_groups(rule)?;
    class.incoming_transition = incoming_transition;
    class.build_setting = build_setting(rule).map(crate::config_module::BuildSettingType::spec);
    class.advertised_providers = advertised_providers(rule);
    class.defined_at = rule.defined_at.clone();
    Ok(class)
}

/// The allowlist of packages whose rules may use Starlark transitions,
/// which Bazel makes every such rule depend on.
fn transition_allowlist() -> AttrSchema {
    let allowlist = Label::parse_canonical("@@bazel_tools//tools/allowlists/function_transition_allowlist:function_transition_allowlist").expect("a valid label");
    AttrSchema::new("$allowlist_function_transition", AttrType::Label).default(AttrValue::Label(allowlist))
}

/// The exec groups `rule(exec_groups = {...})` declares, and the `test`
/// group Bazel gives test rules that don't declare it.
fn exec_groups<'v, V: ValueLike<'v>>(rule: &StarlarkRuleGen<V>) -> anyhow::Result<Vec<Symbol>> {
    let mut names = exec_group_names(rule.option("exec_groups"), "rule")?;
    let test = Symbol::intern("test");
    if rule.test && !names.contains(&test) {
        names.push(test);
    }
    Ok(names)
}

/// The names of the exec groups `exec_groups = {...}` declares, for a
/// rule or an aspect, as `what` says.
pub(crate) fn exec_group_names(groups: Option<Value<'_>>, what: &str) -> anyhow::Result<Vec<Symbol>> {
    let mut names = Vec::new();
    let Some(groups) = groups.filter(|groups| !groups.is_none()) else {
        return Ok(names);
    };
    let Some(groups) = DictRef::from_value(groups) else {
        anyhow::bail!("{what}: exec_groups must be a dict, not {}", groups.get_type());
    };
    for name in groups.keys() {
        let Some(name) = name.unpack_str() else {
            anyhow::bail!("{what}: exec group names must be strings, not {}", name.get_type());
        };
        if !is_valid_exec_group_name(name) {
            anyhow::bail!("Exec group name '{name}' is not a valid name.");
        }
        names.push(Symbol::intern(name));
    }
    Ok(names)
}

/// Whether an exec group can have this name: it must be an identifier.
fn is_valid_exec_group_name(name: &str) -> bool {
    let mut chars = name.chars();
    let starts_well = chars.next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_');
    starts_well && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The hidden attributes a rule gets from a subrule it declares, whose
/// default labels resolve in the subrule's `.bzl` file.
fn subrule_attrs(subrule: &crate::subrule::Subrule<'_>, context: &EvalContext) -> anyhow::Result<Vec<AttrSchema>> {
    let mapping = context.mapping_for(subrule.definition.repo());
    let parser = LabelParser {
        package: subrule.definition.package,
        mapping: &*mapping,
    };
    let mut schemas = Vec::new();
    for (name, attribute) in subrule.rule_attrs()? {
        let Some(attribute) = Attribute::from_value(attribute) else {
            anyhow::bail!("attribute '{name}' of a subrule isn't an attr.*() value");
        };
        schemas.push(attr_schema(&name, attribute, &parser)?);
    }
    Ok(schemas)
}

pub(crate) fn attr_schema(name: &str, attribute: &Attribute<'_>, parser: &LabelParser<'_>) -> anyhow::Result<AttrSchema> {
    let ty = attribute.ty;
    let default = match attribute.default {
        None => AttrDefault::Value(AttrValue::default_for(ty)),
        Some(value) if is_computed(value) => AttrDefault::Computed,
        Some(value) if is_late_bound(value) => AttrDefault::LateBound(late_bound_default(value)),
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
        aspects: attached_aspects(name, attribute)?,
    })
}

/// The aspects `attr.label(aspects = [...])` applies to an attribute's
/// dependencies, but those of Bazel's own that bb only stands in for.
fn attached_aspects(name: &str, attribute: &Attribute<'_>) -> anyhow::Result<Vec<AspectRef>> {
    let Some(aspects) = attribute.aspects.filter(|aspects| !aspects.is_none()) else {
        return Ok(Vec::new());
    };
    let Some(items) = convert::items(aspects) else {
        anyhow::bail!("attribute '{name}': aspects must be a list");
    };
    let modelled = items.into_iter().filter(|item| item.downcast_ref::<crate::opaque::Opaque>().is_none());
    modelled.map(crate::aspect::aspect_ref).collect()
}

/// The keys of the providers `rule(provides = [...])` names.
fn advertised_providers<'v, V: ValueLike<'v>>(rule: &StarlarkRuleGen<V>) -> Vec<String> {
    let provides = rule.option("provides").and_then(convert::items).unwrap_or_default();
    provides.into_iter().filter_map(provider_key).collect()
}

/// The key a provider's instances are found by in a target: a Starlark
/// provider's, or a higher layer's provider's name.
pub(crate) fn provider_key(provider: Value<'_>) -> Option<String> {
    match crate::provider::Provider::from_value(provider) {
        Some(provider) => Some(provider.key()),
        None => native_provider_name(provider),
    }
}

/// The name of a provider a higher layer defines, such as `CcInfo`, by how
/// it prints: `<provider NAME>`.
fn native_provider_name(provider: Value<'_>) -> Option<String> {
    let printed = provider.to_str();
    printed.strip_prefix("<provider ")?.strip_suffix('>').map(str::to_string)
}

/// A build setting rule's `build_setting = config.<type>(...)`, if it has
/// one.
fn build_setting<'v, V: ValueLike<'v>>(rule: &StarlarkRuleGen<V>) -> Option<&'v crate::config_module::BuildSettingType> {
    let setting = rule.option("build_setting").filter(|value| !value.is_none())?;
    setting.downcast_ref::<crate::config_module::BuildSettingType>()
}

/// The type of a build setting's value, which its `build_setting_default`
/// has too. A setting that each use adds to still has one default value.
fn build_setting_type(setting: &crate::config_module::BuildSettingType) -> AttrType {
    match setting.name.as_str() {
        "bool" => AttrType::Bool,
        "int" => AttrType::Int,
        "string_list" | "string_set" => AttrType::StringList,
        _ => AttrType::String,
    }
}

fn is_late_bound(value: Value<'_>) -> bool {
    value.downcast_ref::<crate::bzl_api::LateBoundDefault>().is_some()
}

/// The label a late-bound default has with Bazel's default options.
fn late_bound_default(value: Value<'_>) -> Option<Label> {
    let late_bound = value.downcast_ref::<crate::bzl_api::LateBoundDefault>()?;
    let default = match (late_bound.fragment.as_str(), late_bound.name.as_str()) {
        ("proto", "proto_compiler") => "@@bazel_tools//tools/proto:protoc",
        ("proto", "proto_toolchain_for_cc") => "@@bazel_tools//tools/proto:cc_toolchain",
        ("proto", "proto_toolchain_for_java") => "@@bazel_tools//tools/proto:java_toolchain",
        ("proto", "proto_toolchain_for_java_lite") => "@@bazel_tools//tools/proto:javalite_toolchain",
        ("cpp", "zipper") => "@@bazel_tools//tools/zip:unzip_fdo",
        ("java", "java_toolchain_bytecode_optimizer") => "@@bazel_tools//tools/jdk:bytecode_optimizer",
        _ => return None,
    };
    Label::parse_canonical(default).ok()
}

fn is_computed(value: Value<'_>) -> bool {
    matches!(value.get_type(), "function" | "builtin_function_or_method")
}

/// The toolchain types `rule(toolchains = [...])` names, and those of them
/// that are optional.
fn toolchain_types<'v, V: ValueLike<'v>>(rule: &StarlarkRuleGen<V>, parser: &LabelParser<'_>) -> anyhow::Result<(Vec<Label>, Vec<Label>)> {
    let mut toolchains = rule.option("toolchains").and_then(convert::items).unwrap_or_default();
    for subrule in crate::subrule::subrules(rule.option("subrules"))? {
        toolchains.extend(subrule.rule_toolchains()?);
    }
    toolchain_labels(toolchains, parser)
}

/// The toolchain types a `toolchains = [...]` list names, and those of them
/// that are optional.
pub(crate) fn toolchain_labels(toolchains: Vec<Value<'_>>, parser: &LabelParser<'_>) -> anyhow::Result<(Vec<Label>, Vec<Label>)> {
    let mut labels = Vec::with_capacity(toolchains.len());
    let mut optional = Vec::new();
    for toolchain in toolchains {
        if let Some(toolchain_type) = toolchain.downcast_ref::<crate::toolchain_type::ToolchainType>() {
            labels.push(toolchain_type.label);
            if !toolchain_type.mandatory {
                optional.push(toolchain_type.label);
            }
        } else if toolchain.unpack_str().is_some() || toolchain.downcast_ref::<crate::label_value::LabelValue>().is_some() {
            labels.push(parser.label(toolchain)?);
        }
    }
    Ok((labels, optional))
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
            exported_from: OnceLock::new(),
            defined_at: crate::reported::call_frames(eval),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::parameter_names;
    use starlark::environment::{Globals, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};
    use starlark::values::Value;

    #[test]
    fn names_the_parameters_that_can_be_passed_by_name() {
        let names = Module::with_temp_heap(|module| {
            let ast = AstModule::parse("test.bzl", "def f(a, b = 1, *args, c, **kwargs): pass\nf".to_owned(), &Dialect::Extended).unwrap();
            let function = Evaluator::new(&module).eval_module(ast, &Globals::standard()).unwrap();
            parameter_names(function)
        });
        assert_eq!(names, ["a", "b", "c"]);
        assert!(parameter_names(Value::new_none()).is_empty());
    }
}
