//! The `attr` module: attribute descriptors for `rule()`.

use crate::config_module::{ConfigChoice, ConfigKind};
use allocative::Allocative;
use bazel_packages::AttrType;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::{
    Freeze, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, StringValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};
use std::fmt;

/// What `attr.label(...)` and its siblings return: an attribute's type and
/// options, waiting for `rule()` to name it.
#[derive(Clone, Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagable)]
#[starlark_pagable(bound = "V: starlark::pagable::StarlarkPagable")]
pub struct AttributeGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[allocative(skip)]
    #[starlark_pagable(serialize_with = "crate::paging::serialize_encoded", deserialize_with = "crate::paging::deserialize_encoded")]
    pub ty: AttrType,
    pub default: Option<V>,
    pub mandatory: bool,
    pub configurable: Option<bool>,
    pub executable: bool,
    /// `"exec"`, `"target"`, or a transition.
    pub cfg: Option<V>,
    pub allow_files: Option<V>,
    pub allow_single_file: Option<V>,
    pub providers: Option<V>,
    pub aspects: Option<V>,
    pub values: Option<V>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<AttributeGen<To>> for AttributeGen<From> {}

starlark_complex_value!(pub Attribute);

impl<'v, V: ValueLike<'v>> fmt::Display for AttributeGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<attr.{}>", self.ty.name())
    }
}

#[starlark_value(type = "Attribute")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for AttributeGen<V> where Self: ProvidesStaticType<'v> {}

impl<'v, V: ValueLike<'v>> AttributeGen<V> {
    /// Whether the dependency is built for the execution platform: `cfg =
    /// "exec"`, or `config.exec(...)`.
    pub fn is_exec(&self) -> bool {
        self.cfg.is_some_and(|cfg| {
            let cfg = cfg.to_value();
            let config_exec = cfg.downcast_ref::<ConfigChoice>().is_some_and(|choice| choice.kind == ConfigKind::Exec);
            config_exec || matches!(cfg.unpack_str(), Some("exec") | Some("host"))
        })
    }

    /// Whether the dependency's configuration is one a transition gives: a
    /// Starlark transition, or one of Bazel's that bb only stands in for.
    pub fn has_transition(&self) -> bool {
        self.cfg.is_some_and(|cfg| {
            let cfg = cfg.to_value();
            cfg.downcast_ref::<crate::opaque::Opaque>().is_some() || crate::transition::Transition::from_value(cfg).is_some()
        })
    }
}

/// The options each attribute function takes, besides `doc`.
fn allowed_options(ty: AttrType) -> &'static [&'static str] {
    const LABEL: &[&str] = &[
        "default",
        "mandatory",
        "executable",
        "allow_files",
        "allow_single_file",
        "allow_rules",
        "providers",
        "cfg",
        "aspects",
        "flags",
        "skip_validations",
        "for_dependency_resolution",
        "materializer",
        "configurable",
    ];
    const LABEL_LIST: &[&str] = &[
        "default",
        "mandatory",
        "allow_empty",
        "allow_files",
        "allow_rules",
        "providers",
        "cfg",
        "aspects",
        "flags",
        "skip_validations",
        "for_dependency_resolution",
        "materializer",
        "configurable",
    ];
    match ty {
        AttrType::Label => LABEL,
        AttrType::LabelList | AttrType::LabelKeyedStringDict | AttrType::StringKeyedLabelDict => LABEL_LIST,
        AttrType::String | AttrType::Int => &["default", "mandatory", "values", "configurable"],
        AttrType::Output => &["mandatory"],
        AttrType::OutputList => &["mandatory", "allow_empty"],
        _ => &["default", "mandatory", "allow_empty", "configurable"],
    }
}

fn attribute<'v>(ty: AttrType, kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<Attribute<'v>> {
    let allowed = allowed_options(ty);
    let mut attribute = Attribute {
        ty,
        default: None,
        mandatory: false,
        configurable: None,
        executable: false,
        cfg: None,
        allow_files: None,
        allow_single_file: None,
        providers: None,
        aspects: None,
        values: None,
    };
    for (name, value) in kwargs {
        let name = name.as_str();
        if name == "doc" {
            continue;
        }
        if !allowed.contains(&name) {
            anyhow::bail!("attr.{}() got an unexpected keyword argument '{name}'", attr_function_name(ty));
        }
        let option = |value: Value<'v>| if value.is_none() { None } else { Some(value) };
        match name {
            "default" => attribute.default = option(value),
            "mandatory" => attribute.mandatory = value.to_bool(),
            "executable" => attribute.executable = value.to_bool(),
            "configurable" => attribute.configurable = value.unpack_bool(),
            "cfg" => attribute.cfg = option(value),
            "allow_files" => attribute.allow_files = option(value),
            "allow_single_file" => attribute.allow_single_file = option(value),
            "providers" => attribute.providers = option(value),
            "aspects" => attribute.aspects = option(value),
            "values" => attribute.values = option(value),
            _ => {}
        }
    }
    Ok(attribute)
}

fn attr_function_name(ty: AttrType) -> &'static str {
    match ty {
        AttrType::Bool => "bool",
        AttrType::Tristate => "int",
        other => other.name(),
    }
}

#[starlark_module]
fn attr_functions(builder: &mut GlobalsBuilder) {
    fn bool<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<Attribute<'v>> {
        attribute(AttrType::Bool, kwargs)
    }

    fn int<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<Attribute<'v>> {
        attribute(AttrType::Int, kwargs)
    }

    fn int_list<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<Attribute<'v>> {
        attribute(AttrType::IntList, kwargs)
    }

    fn string<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<Attribute<'v>> {
        attribute(AttrType::String, kwargs)
    }

    fn string_list<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<Attribute<'v>> {
        attribute(AttrType::StringList, kwargs)
    }

    fn string_dict<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<Attribute<'v>> {
        attribute(AttrType::StringDict, kwargs)
    }

    fn string_list_dict<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<Attribute<'v>> {
        attribute(AttrType::StringListDict, kwargs)
    }

    fn label<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<Attribute<'v>> {
        attribute(AttrType::Label, kwargs)
    }

    fn label_list<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<Attribute<'v>> {
        attribute(AttrType::LabelList, kwargs)
    }

    fn label_keyed_string_dict<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
    ) -> anyhow::Result<Attribute<'v>> {
        attribute(AttrType::LabelKeyedStringDict, kwargs)
    }

    fn string_keyed_label_dict<'v>(
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
    ) -> anyhow::Result<Attribute<'v>> {
        attribute(AttrType::StringKeyedLabelDict, kwargs)
    }

    fn output<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<Attribute<'v>> {
        attribute(AttrType::Output, kwargs)
    }

    fn output_list<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<Attribute<'v>> {
        attribute(AttrType::OutputList, kwargs)
    }
}

/// Add the `attr` namespace to `builder`.
pub(crate) fn register(builder: &mut GlobalsBuilder) {
    builder.namespace("attr", attr_functions);
}
