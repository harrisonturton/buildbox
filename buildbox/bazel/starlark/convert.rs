//! Converting Starlark values to attribute values, and back.

use crate::label_value::LabelValue;
use crate::select::{Select, selector_parts};
use bazel_label::{Label, LabelContext, PackageId, RepoMapping};
use bazel_packages::{AttrType, AttrValue, DEFAULT_CONDITION, Select as SelectValue, SelectorPart};
use intern::Symbol;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::{AllocList, ListRef};
use starlark::values::tuple::TupleRef;
use starlark::values::{Heap, Value, ValueLike};

/// Parses labels relative to a package, as seen from its repository.
#[derive(Clone, Copy)]
pub struct LabelParser<'a> {
    pub package: PackageId,
    pub mapping: &'a dyn RepoMapping,
}

impl LabelParser<'_> {
    pub fn parse(&self, text: &str) -> anyhow::Result<Label> {
        let context = LabelContext {
            package: self.package,
            mapping: self.mapping,
            lenient: true,
        };
        Ok(Label::parse(text, &context)?)
    }

    /// A label given as a string or a `Label`.
    pub fn label(&self, value: Value<'_>) -> anyhow::Result<Label> {
        if let Some(label) = value.downcast_ref::<LabelValue>() {
            return Ok(label.0);
        }
        match value.unpack_str() {
            Some(text) => self.parse(text),
            None => anyhow::bail!("expected a label, got {} of type '{}'", value.to_repr(), value.get_type()),
        }
    }
}

/// Check a target name, as given to `name =` or `same_package_label`.
pub fn target_name(name: &str) -> anyhow::Result<Symbol> {
    let label = format!("//:{name}");
    let parsed = Label::parse_canonical(&label).map_err(|error| anyhow::anyhow!("invalid target name '{name}': {}", error.reason))?;
    Ok(parsed.name)
}

/// Convert the value given for attribute `attr` of type `ty`, which may be
/// a `select()`.
pub fn attr_value(value: Value<'_>, ty: AttrType, parser: &LabelParser<'_>, attr: &str) -> anyhow::Result<AttrValue> {
    let Some(parts) = selector_parts(value) else {
        return plain_value(value, ty, parser, attr);
    };
    let mut converted = Vec::with_capacity(parts.len());
    for part in parts {
        let Some(select) = Select::from_value(part) else {
            converted.push(SelectorPart::Value(plain_value(part, ty, parser, attr)?));
            continue;
        };
        let mut branches = Vec::with_capacity(select.branches.len());
        for (condition, value) in &select.branches {
            let condition = match condition.unpack_str() {
                Some(DEFAULT_CONDITION) => Label::parse_canonical(DEFAULT_CONDITION)?,
                _ => parser.label(*condition)?,
            };
            let value = if value.is_none() {
                AttrValue::None
            } else {
                plain_value(*value, ty, parser, attr)?
            };
            branches.push((condition, value));
        }
        converted.push(SelectorPart::Select(SelectValue {
            branches,
            no_match_error: select.no_match_error.clone(),
        }));
    }
    Ok(AttrValue::Configurable(converted))
}

/// Convert a value that isn't a `select()`.
pub fn plain_value(value: Value<'_>, ty: AttrType, parser: &LabelParser<'_>, attr: &str) -> anyhow::Result<AttrValue> {
    let wrong_type = || {
        anyhow::anyhow!(
            "expected value of type '{}' for attribute '{attr}', but got {} ({})",
            ty.name(),
            value.to_repr(),
            value.get_type()
        )
    };
    let converted = match ty {
        AttrType::Bool => AttrValue::Bool(unpack_bool(value).ok_or_else(wrong_type)?),
        AttrType::Int | AttrType::Tristate => AttrValue::Int(unpack_int(value).ok_or_else(wrong_type)?),
        AttrType::String => AttrValue::String(value.unpack_str().ok_or_else(wrong_type)?.to_string()),
        AttrType::Label => AttrValue::Label(parser.label(value).map_err(|_| wrong_type())?),
        AttrType::Output => AttrValue::Label(output_label(value, parser).map_err(|_| wrong_type())?),
        AttrType::StringList | AttrType::License => AttrValue::StringList(strings(value).ok_or_else(wrong_type)?),
        AttrType::LabelList => {
            let items = items(value).ok_or_else(wrong_type)?;
            AttrValue::LabelList(items.into_iter().map(|item| parser.label(item)).collect::<anyhow::Result<_>>()?)
        }
        AttrType::OutputList => {
            let items = items(value).ok_or_else(wrong_type)?;
            AttrValue::LabelList(items.into_iter().map(|item| output_label(item, parser)).collect::<anyhow::Result<_>>()?)
        }
        AttrType::IntList => {
            let items = items(value).ok_or_else(wrong_type)?;
            AttrValue::IntList(items.into_iter().map(|item| unpack_int(item).ok_or_else(wrong_type)).collect::<anyhow::Result<_>>()?)
        }
        AttrType::StringDict => AttrValue::StringDict(dict_entries(value, wrong_type, |k| str_of(k), |v| str_of(v))?),
        AttrType::StringListDict => AttrValue::StringListDict(dict_entries(value, wrong_type, |k| str_of(k), strings)?),
        AttrType::LabelKeyedStringDict => {
            AttrValue::LabelKeyedStringDict(dict_entries(value, wrong_type, |k| parser.label(k).ok(), |v| str_of(v))?)
        }
        AttrType::StringKeyedLabelDict => {
            AttrValue::StringKeyedLabelDict(dict_entries(value, wrong_type, |k| str_of(k), |v| parser.label(v).ok())?)
        }
    };
    Ok(converted)
}

/// An output must be a file name in the rule's own package.
fn output_label(value: Value<'_>, parser: &LabelParser<'_>) -> anyhow::Result<Label> {
    let label = parser.label(value)?;
    if label.package != parser.package {
        anyhow::bail!("output {label} is not in the rule's package");
    }
    Ok(label)
}

fn unpack_bool(value: Value<'_>) -> Option<bool> {
    if let Some(b) = value.unpack_bool() {
        return Some(b);
    }
    match value.unpack_i32()? {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn unpack_int(value: Value<'_>) -> Option<i64> {
    if let Some(b) = value.unpack_bool() {
        return Some(b as i64);
    }
    value.unpack_i32().map(i64::from)
}

fn str_of(value: Value<'_>) -> Option<String> {
    value.unpack_str().map(str::to_string)
}

/// The elements of a list or tuple.
pub fn items(value: Value<'_>) -> Option<Vec<Value<'_>>> {
    if let Some(list) = ListRef::from_value(value) {
        return Some(list.iter().collect());
    }
    TupleRef::from_value(value).map(|tuple| tuple.iter().collect())
}

/// A list or tuple of strings.
pub fn strings(value: Value<'_>) -> Option<Vec<String>> {
    items(value)?.into_iter().map(str_of).collect()
}

fn dict_entries<'v, K, V>(
    value: Value<'v>,
    wrong_type: impl Fn() -> anyhow::Error,
    key: impl Fn(Value<'v>) -> Option<K>,
    entry: impl Fn(Value<'v>) -> Option<V>,
) -> anyhow::Result<Vec<(K, V)>> {
    let dict = DictRef::from_value(value).ok_or_else(&wrong_type)?;
    let mut entries = Vec::with_capacity(dict.len());
    for (k, v) in dict.iter() {
        entries.push((key(k).ok_or_else(&wrong_type)?, entry(v).ok_or_else(&wrong_type)?));
    }
    Ok(entries)
}

/// An attribute value as Starlark sees it, as `native.existing_rule` and
/// `ctx.attr` return them.
pub fn to_starlark<'v>(value: &AttrValue, heap: Heap<'v>) -> Value<'v> {
    match value {
        AttrValue::None => Value::new_none(),
        AttrValue::Bool(b) => Value::new_bool(*b),
        AttrValue::Int(i) => heap.alloc(*i as i32),
        AttrValue::String(s) => heap.alloc(s.as_str()),
        AttrValue::Label(label) => heap.alloc(LabelValue(*label)),
        AttrValue::StringList(items) => heap.alloc(AllocList(items.iter().map(|s| s.as_str()))),
        AttrValue::LabelList(labels) => heap.alloc(AllocList(labels.iter().map(|l| LabelValue(*l)))),
        AttrValue::IntList(items) => heap.alloc(AllocList(items.iter().map(|i| *i as i32))),
        AttrValue::StringDict(entries) => heap.alloc(AllocDict(entries.iter().map(|(k, v)| (k.as_str(), v.as_str())))),
        AttrValue::StringListDict(entries) => heap.alloc(AllocDict(
            entries.iter().map(|(k, v)| (k.as_str(), heap.alloc(AllocList(v.iter().map(|s| s.as_str()))))),
        )),
        AttrValue::LabelKeyedStringDict(entries) => {
            heap.alloc(AllocDict(entries.iter().map(|(k, v)| (LabelValue(*k), v.as_str()))))
        }
        AttrValue::StringKeyedLabelDict(entries) => {
            heap.alloc(AllocDict(entries.iter().map(|(k, v)| (k.as_str(), LabelValue(*v)))))
        }
        AttrValue::Configurable(parts) => {
            let parts: Vec<Value<'v>> = parts.iter().map(|part| selector_part_to_starlark(part, heap)).collect();
            match parts.len() {
                1 => parts[0],
                _ => heap.alloc(crate::select::SelectorList { parts }),
            }
        }
    }
}

fn selector_part_to_starlark<'v>(part: &SelectorPart, heap: Heap<'v>) -> Value<'v> {
    match part {
        SelectorPart::Value(value) => to_starlark(value, heap),
        SelectorPart::Select(select) => heap.alloc(Select {
            branches: select
                .branches
                .iter()
                .map(|(condition, value)| (heap.alloc(LabelValue(*condition)), to_starlark(value, heap)))
                .collect(),
            no_match_error: select.no_match_error.clone(),
        }),
    }
}
