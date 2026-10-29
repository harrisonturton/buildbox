//! The providers of the rules that describe platforms: `constraint_setting`
//! gives `ConstraintSettingInfo`, `constraint_value` gives
//! `ConstraintValueInfo` and `platform` gives `PlatformInfo`, whose
//! `constraints` are a `ConstraintCollection`.

use crate::build_ctx::{Prepared, RuleInput};
use crate::providers;
use crate::target_value::TargetValue;
use allocative::Allocative;
use bazel_label::Label;
use bazel_packages::AttrValue;
use bazel_starlark::label_value::LabelValue;
use bazel_starlark::provider::ProviderInstance;
use starlark::coerce::Coerce;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::list::AllocList;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Trace, Value, ValueLifetimeless,
    ValueLike, starlark_value,
};
use std::fmt;

/// `constraint_setting`: its label and default value, as
/// `ConstraintSettingInfo`.
pub fn constraint_setting<'v>(input: &RuleInput, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let default = match input.attr("default_constraint_value") {
        Some(AttrValue::Label(label)) => Some(*label),
        _ => None,
    };
    let info = setting_info(input.label, default, heap)?;
    Ok(heap.alloc(AllocList([info])))
}

/// `constraint_value`: its label and setting, as `ConstraintValueInfo`.
pub fn constraint_value<'v>(input: &RuleInput, prepared: &Prepared<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let setting = prepared
        .targets(input, "constraint_setting")
        .first()
        .and_then(|target| TargetValue::from_value(*target)?.provider(SETTING_INFO))
        .ok_or_else(|| anyhow::anyhow!("{}: its constraint_setting isn't a constraint_setting", input.label))?;
    let info = value_info(input.label, setting, heap)?;
    Ok(heap.alloc(AllocList([info])))
}

/// `platform`: its label and constraints, those of its parent with its
/// own `constraint_values` in their settings' places, as `PlatformInfo`.
pub fn platform<'v>(input: &RuleInput, prepared: &Prepared<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let mut constraints: Vec<(Value<'v>, Value<'v>)> = Vec::new();
    for parent in prepared.targets(input, "parents") {
        let collection = TargetValue::from_value(parent)
            .and_then(|target| target.provider(PLATFORM_INFO))
            .and_then(|info| ProviderInstance::from_value(info)?.field("constraints"))
            .and_then(ConstraintCollection::from_value);
        if let Some(collection) = collection {
            constraints.extend(collection.constraints.iter().copied());
        }
    }
    let mut own: Vec<Label> = Vec::new();
    for target in prepared.targets(input, "constraint_values") {
        let Some(value) = TargetValue::from_value(target).and_then(|target| target.provider(VALUE_INFO)) else {
            anyhow::bail!("{}: {} isn't a constraint_value", input.label, TargetValue::from_value(target).map(|target| target.label.to_string()).unwrap_or_default());
        };
        let setting = field(value, "constraint")?;
        let setting_label = label_field(setting)?;
        if own.contains(&setting_label) {
            anyhow::bail!("{}: Duplicate constraint values detected: constraint_setting {setting_label} has more than one value", input.label);
        }
        own.push(setting_label);
        constraints.retain(|(existing, _)| label_field(*existing).ok() != Some(setting_label));
        constraints.push((setting, value));
    }
    let collection = heap.alloc(ConstraintCollection { constraints });
    let info = providers::instance(PLATFORM_INFO, vec![("label", heap.alloc(LabelValue(input.label))), ("constraints", collection)], heap)?;
    Ok(heap.alloc(AllocList([info])))
}

const SETTING_INFO: &str = "ConstraintSettingInfo";
const VALUE_INFO: &str = "ConstraintValueInfo";
const PLATFORM_INFO: &str = "PlatformInfo";

/// A `ConstraintSettingInfo`. Its default value's info names a copy of the
/// setting without the default, since provider values can't refer back to
/// themselves.
fn setting_info<'v>(label: Label, default: Option<Label>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let bare = |heap: Heap<'v>| {
        providers::instance(
            SETTING_INFO,
            vec![("label", heap.alloc(LabelValue(label))), ("has_default_constraint_value", Value::new_bool(false)), ("default_constraint_value", Value::new_none())],
            heap,
        )
    };
    let Some(default) = default else {
        return bare(heap);
    };
    let default_info = value_info(default, bare(heap)?, heap)?;
    providers::instance(
        SETTING_INFO,
        vec![("label", heap.alloc(LabelValue(label))), ("has_default_constraint_value", Value::new_bool(true)), ("default_constraint_value", default_info)],
        heap,
    )
}

fn value_info<'v>(label: Label, setting: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    providers::instance(VALUE_INFO, vec![("label", heap.alloc(LabelValue(label))), ("constraint", setting)], heap)
}

fn field<'v>(info: Value<'v>, name: &str) -> anyhow::Result<Value<'v>> {
    ProviderInstance::from_value(info)
        .and_then(|info| info.field(name))
        .ok_or_else(|| anyhow::anyhow!("expected a provider with '{name}'"))
}

/// The `label` of a constraint setting's or value's info.
fn label_field(info: Value<'_>) -> anyhow::Result<Label> {
    field(info, "label")?
        .downcast_ref::<LabelValue>()
        .map(|label| label.0)
        .ok_or_else(|| anyhow::anyhow!("expected a label"))
}

/// A platform's constraints: for each setting it has a value for, the
/// setting's and the value's infos.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagable)]
#[starlark_pagable(bound = "V: starlark::pagable::StarlarkPagable")]
#[repr(C)]
pub struct ConstraintCollectionGen<V: ValueLifetimeless> {
    constraints: Vec<(V, V)>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<ConstraintCollectionGen<To>> for ConstraintCollectionGen<From> {}

starlark_complex_value!(pub ConstraintCollection);

impl<'v, V: ValueLike<'v>> fmt::Display for ConstraintCollectionGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<ConstraintCollection>")
    }
}

impl<'v, V: ValueLike<'v>> ConstraintCollectionGen<V> {
    /// The value the collection has for a setting, or the setting's
    /// default.
    fn get(&self, setting: Value<'v>) -> anyhow::Result<Value<'v>> {
        let label = label_field(setting)?;
        for (candidate, value) in &self.constraints {
            if label_field(candidate.to_value())? == label {
                return Ok(value.to_value());
            }
        }
        let has_default = field(setting, "has_default_constraint_value")?.unpack_bool().unwrap_or(false);
        match has_default {
            true => field(setting, "default_constraint_value"),
            false => Ok(Value::new_none()),
        }
    }
}

#[starlark_value(type = "ConstraintCollection")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ConstraintCollectionGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("ConstraintCollection", constraint_collection_methods);
        Some(METHODS.methods())
    }

    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "constraint_settings" => Some(heap.alloc(AllocList(self.constraints.iter().map(|(setting, _)| setting.to_value())))),
            _ => None,
        }
    }
}

#[starlark_module]
fn constraint_collection_methods(builder: &mut MethodsBuilder) {
    /// `get(constraint)`: the value for a `ConstraintSettingInfo`, its
    /// default, or `None`.
    fn get<'v>(this: Value<'v>, constraint: Value<'v>) -> anyhow::Result<Value<'v>> {
        collection(this)?.get(constraint)
    }

    /// `has(constraint)`: whether there's a value for a
    /// `ConstraintSettingInfo`, its default included.
    fn has<'v>(this: Value<'v>, constraint: Value<'v>) -> anyhow::Result<bool> {
        Ok(!collection(this)?.get(constraint)?.is_none())
    }

    /// `has_constraint_value(constraint_value)`: whether the value for the
    /// `ConstraintValueInfo`'s setting is it.
    fn has_constraint_value<'v>(this: Value<'v>, constraint_value: Value<'v>) -> anyhow::Result<bool> {
        let setting = field(constraint_value, "constraint")?;
        let found = collection(this)?.get(setting)?;
        if found.is_none() {
            return Ok(false);
        }
        Ok(label_field(found)? == label_field(constraint_value)?)
    }
}

fn collection<'v>(value: Value<'v>) -> anyhow::Result<&'v ConstraintCollection<'v>> {
    ConstraintCollection::from_value(value).ok_or_else(|| anyhow::anyhow!("expected a ConstraintCollection"))
}
