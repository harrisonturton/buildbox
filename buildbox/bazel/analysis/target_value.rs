//! `Target`: an analyzed dependency, as rule implementations see it.

use allocative::Allocative;
use bazel_label::Label;
use bazel_starlark::label_value::LabelValue;
use bazel_starlark::provider::{Provider, ProviderInstance};
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::starlark_complex_value;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value, ValueLifetimeless,
    ValueLike, starlark_value,
};
use std::fmt;

/// The key `DefaultInfo` is stored under.
pub const DEFAULT_INFO: &str = "DefaultInfo";

/// A configured target: its label, and the providers its rule returned,
/// by [provider key](bazel_starlark::provider::ProviderGen::key).
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct TargetValueGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[allocative(skip)]
    pub label: Label,
    pub providers: SmallMap<String, V>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<TargetValueGen<To>> for TargetValueGen<From> {}

starlark_complex_value!(pub TargetValue);

impl<'v, V: ValueLike<'v>> fmt::Display for TargetValueGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<target {}>", self.label)
    }
}

impl<'v, V: ValueLike<'v>> TargetValueGen<V> {
    pub fn provider(&self, key: &str) -> Option<Value<'v>> {
        self.providers.get(key).map(|value| value.to_value())
    }

    /// A field of the target's `DefaultInfo`.
    pub fn default_info(&self, field: &str) -> Option<Value<'v>> {
        let info = ProviderInstance::from_value(self.provider(DEFAULT_INFO)?)?;
        info.field(field)
    }
}

fn provider_key(value: Value<'_>) -> starlark::Result<String> {
    match Provider::from_value(value) {
        Some(provider) => Ok(provider.key()),
        None => Err(starlark::Error::new_other(anyhow::anyhow!(
            "a target is indexed by a provider, not {}",
            value.get_type()
        ))),
    }
}

#[starlark_value(type = "Target")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for TargetValueGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "label" => Some(heap.alloc(LabelValue(self.label))),
            "files" | "files_to_run" | "data_runfiles" | "default_runfiles" => self.default_info(attribute),
            "output_groups" => self.provider("OutputGroupInfo"),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        ["label", "files", "files_to_run", "data_runfiles", "default_runfiles"].iter().map(|name| name.to_string()).collect()
    }

    fn at(&self, index: Value<'v>, _heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let key = provider_key(index)?;
        match self.provider(&key) {
            Some(value) => Ok(value),
            None => Err(starlark::Error::new_other(anyhow::anyhow!(
                "<target {}> (rule '{}') doesn't contain declared provider '{}'",
                self.label,
                self.label.name,
                Provider::from_value(index).map(|provider| provider.name().to_string()).unwrap_or_default()
            ))),
        }
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(self.providers.contains_key(&provider_key(other)?))
    }
}
