//! Starlark build settings: the targets whose values configurations hold,
//! which flags, transitions and `config_setting`s set and read.

use bazel_config::{BuildOptions, FlagValue};
use bazel_label::Label;
use bazel_loading::PackageKey;
use bazel_packages::{AttrValue, BuildSettingSpec, Target};
use bazel_starlark::label_value::LabelValue;
use intern::Symbol;
use skyframe::{Compute, SkyError};
use starlark::values::list::AllocList;
use starlark::values::{Heap, Value, ValueLike};

/// A build setting: the target it is, through aliases, its default, and
/// how it's set.
#[derive(Clone, Debug)]
pub struct BuildSetting {
    /// The label it was named by.
    pub label: Label,
    pub actual: Label,
    pub default: FlagValue,
    pub spec: BuildSettingSpec,
}

impl BuildSetting {
    /// Its value in `options`.
    pub fn value<'a>(&'a self, options: &'a BuildOptions) -> &'a FlagValue {
        options.flag(self.actual).unwrap_or(&self.default)
    }
}

/// The build setting `label` names, through aliases, or `None` if it names
/// a target that isn't one.
pub async fn build_setting(compute: &impl Compute, label: Label) -> Result<Option<BuildSetting>, SkyError> {
    let actual = crate::platform::actual(compute, label).await?;
    let package = compute.compute(&PackageKey(actual.package)).await?;
    let Some(Target::Rule(rule)) = package.0.target(actual.name) else {
        return Ok(None);
    };
    let class = bazel_loading::rule_class(compute, package.0.rule_class(rule)).await?;
    let Some(spec) = class.build_setting else {
        return Ok(None);
    };
    let default = rule.attr(Symbol::intern("build_setting_default")).and_then(|default| flag_value(default, spec));
    let Some(default) = default else {
        return Err(SkyError::new(format!("build setting {actual} has no default")));
    };
    Ok(Some(BuildSetting { label, actual, default, spec }))
}

/// A build setting's value given as an attribute's, such as its default.
/// A setting each use adds to has a list of the one value.
pub fn flag_value(value: &AttrValue, spec: BuildSettingSpec) -> Option<FlagValue> {
    Some(match value {
        AttrValue::Bool(value) => FlagValue::Bool(*value),
        AttrValue::Int(value) => FlagValue::Int(*value),
        AttrValue::String(value) if spec.multiple => FlagValue::Strings(vec![value.clone()]),
        AttrValue::String(value) => FlagValue::String(value.clone()),
        AttrValue::StringList(values) => FlagValue::Strings(values.clone()),
        AttrValue::Label(label) => FlagValue::Label(*label),
        _ => return None,
    })
}

/// A build setting's value as Starlark has it.
pub fn starlark_value<'v>(value: &FlagValue, heap: Heap<'v>) -> Value<'v> {
    match value {
        FlagValue::Bool(value) => Value::new_bool(*value),
        FlagValue::Int(value) => heap.alloc(*value as i32),
        FlagValue::String(value) => heap.alloc(value.as_str()),
        FlagValue::Strings(values) => heap.alloc(AllocList(values.iter().map(String::as_str))),
        FlagValue::Label(label) => heap.alloc(LabelValue(*label)),
    }
}

/// A Starlark value given to a build setting of `like`'s type, or `None` if
/// it's of another type. Labels may be strings, relative to
/// `parse_label`'s package.
pub fn from_starlark(value: Value<'_>, like: &FlagValue, parse_label: bazel_config::LabelParser<'_>) -> anyhow::Result<Option<FlagValue>> {
    Ok(match like {
        FlagValue::Bool(_) => value.unpack_bool().map(FlagValue::Bool),
        FlagValue::Int(_) => value.unpack_i32().map(|value| FlagValue::Int(value.into())),
        FlagValue::String(_) => value.unpack_str().map(|value| FlagValue::String(value.to_string())),
        FlagValue::Strings(_) => bazel_starlark::convert::strings(value).map(FlagValue::Strings),
        FlagValue::Label(_) => match (value.downcast_ref::<LabelValue>(), value.unpack_str()) {
            (Some(label), _) => Some(FlagValue::Label(label.0)),
            (None, Some(text)) => Some(FlagValue::Label(parse_label(text).map_err(|error| anyhow::anyhow!("{error}"))?)),
            (None, None) => None,
        },
    })
}
