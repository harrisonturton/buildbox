//! `Target`: an analyzed dependency, as rule implementations see it.

use allocative::Allocative;
use bazel_label::Label;
use bazel_starlark::label_value::LabelValue;
use bazel_starlark::provider::{Provider, ProviderInstance};
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::starlark_complex_value;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Trace, Value, ValueLifetimeless,
    ValueLike, starlark_value,
};
use std::fmt;

/// The key `DefaultInfo` is stored under.
pub const DEFAULT_INFO: &str = "DefaultInfo";

/// A configured target: its label, and the providers its rule returned,
/// by [provider key](bazel_starlark::provider::ProviderGen::key).
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagable)]
#[starlark_pagable(bound = "V: starlark::pagable::StarlarkPagable")]
#[repr(C)]
pub struct TargetValueGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[allocative(skip)]
    #[starlark_pagable(serialize_with = "bazel_starlark::paging::serialize_encoded", deserialize_with = "bazel_starlark::paging::deserialize_encoded")]
    pub label: Label,
    pub providers: SmallMap<String, V>,
    /// Whether the target is a file rather than a rule. Files have no
    /// runfiles of their own; see [`crate::providers::collected_runfiles`].
    #[freeze(identity)]
    pub is_file: bool,
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

    /// The files of output groups, as a build reports a target built them:
    /// its default outputs, for `default`, and every other group's files.
    pub fn output_group_files(&self, groups: &[String]) -> Vec<crate::Artifact> {
        let mut artifacts = Vec::new();
        for group in groups {
            match group.as_str() {
                DEFAULT_GROUP => artifacts.extend(depset_artifacts(self.default_info("files"))),
                _ => {
                    let instance = self.provider("OutputGroupInfo").and_then(ProviderInstance::from_value);
                    artifacts.extend(depset_artifacts(instance.and_then(|groups| groups.field(group))));
                }
            }
        }
        artifacts
    }
}

/// The output group of a target's default outputs.
pub const DEFAULT_GROUP: &str = "default";

/// The files of a depset of files.
pub fn depset_artifacts(value: Option<Value<'_>>) -> Vec<crate::Artifact> {
    let Some(depset) = value.and_then(bazel_starlark::depset::Depset::from_value) else {
        return Vec::new();
    };
    depset.to_list().into_iter().filter_map(crate::FileValue::artifact).collect()
}

/// The key a provider, given to index a target, finds its instance by.
pub(crate) fn provider_key(value: Value<'_>) -> starlark::Result<String> {
    if let Some(provider) = value.downcast_ref::<crate::cc::values::NativeProvider>() {
        return Ok(provider.name.to_string());
    }
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
            "actions" => crate::action_value::target_actions(self as *const Self as usize, heap),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        ["label", "files", "files_to_run", "data_runfiles", "default_runfiles", "actions"].iter().map(|name| name.to_string()).collect()
    }

    fn at(&self, index: Value<'v>, _heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let key = provider_key(index)?;
        match self.provider(&key) {
            Some(value) => Ok(value),
            None => Err(starlark::Error::new_other(anyhow::anyhow!(
                "<target {}> (rule '{}') doesn't contain declared provider '{}'",
                self.label,
                self.label.name,
                provider_key(index).unwrap_or_default()
            ))),
        }
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(self.providers.contains_key(&provider_key(other)?))
    }

    /// Targets are equal when they have the same label, so they can be
    /// keys of dicts, as `label_keyed_string_dict` attributes make them.
    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(TargetValue::from_value(other).is_some_and(|other| other.label == self.label))
    }

    fn write_hash(&self, hasher: &mut starlark::collections::StarlarkHasher) -> starlark::Result<()> {
        use std::hash::Hash;
        self.label.hash(hasher);
        Ok(())
    }
}

/// A target with the providers of the aspects applied to it: the target's,
/// then each aspect's, with their output groups merged, as Bazel merges
/// them. Any other provider given twice is an error.
pub fn merge<'v>(targets: &[Value<'v>], heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let Some((first, rest)) = targets.split_first() else {
        anyhow::bail!("no target to merge");
    };
    let Some(base) = TargetValue::from_value(*first) else {
        return Ok(*first);
    };
    if rest.is_empty() {
        return Ok(*first);
    }
    let mut providers: SmallMap<String, Value<'v>> = base.providers.iter().map(|(key, value)| (key.clone(), *value)).collect();
    let mut output_groups: Vec<Value<'v>> = providers.shift_remove(OUTPUT_GROUP_INFO).into_iter().collect();
    for aspect in rest {
        let Some(aspect) = TargetValue::from_value(*aspect) else {
            continue;
        };
        for (key, value) in aspect.providers.iter() {
            if key == OUTPUT_GROUP_INFO {
                output_groups.push(*value);
                continue;
            }
            if providers.insert(key.clone(), *value).is_some() {
                let name = key.rsplit('%').next().unwrap_or(key);
                return Err(crate::failure::ProvidersClash(name.to_string()).into());
            }
        }
    }
    if let Some(groups) = merged_output_groups(&output_groups, heap)? {
        providers.insert(OUTPUT_GROUP_INFO.to_string(), groups);
    }
    Ok(heap.alloc(TargetValue {
        label: base.label,
        providers,
        is_file: base.is_file,
    }))
}

/// The key `OutputGroupInfo` is stored under.
const OUTPUT_GROUP_INFO: &str = "OutputGroupInfo";

/// One `OutputGroupInfo` with every group of several: a group more than one
/// has holds the files of each.
fn merged_output_groups<'v>(infos: &[Value<'v>], heap: Heap<'v>) -> anyhow::Result<Option<Value<'v>>> {
    let Some((first, rest)) = infos.split_first() else {
        return Ok(None);
    };
    if rest.is_empty() {
        return Ok(Some(*first));
    }
    let mut groups: SmallMap<String, Vec<Value<'v>>> = SmallMap::new();
    for info in infos {
        let Some(instance) = ProviderInstance::from_value(*info) else {
            continue;
        };
        for (name, files) in instance.fields.iter() {
            groups.entry(name.to_string()).or_default().push(files.to_value());
        }
    }
    let fields: Vec<(String, Value<'v>)> = groups
        .into_iter()
        .map(|(name, files)| {
            let files = match files.as_slice() {
                [files] => *files,
                _ => heap.alloc(bazel_starlark::depset::Depset {
                    direct: Vec::new(),
                    transitive: files,
                    order: bazel_starlark::depset::Order::Default,
                }),
            };
            (name, files)
        })
        .collect();
    let fields = fields.iter().map(|(name, files)| (name.as_str(), *files)).collect();
    Ok(Some(crate::providers::instance(OUTPUT_GROUP_INFO, fields, heap)?))
}
