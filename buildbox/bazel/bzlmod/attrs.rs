//! Converting the attributes of repository rules and tags to the values
//! their schemas say they have.

use crate::star_value::StarValue;
use bazel_label::{Label, LabelContext, PackageId, RepoMapping, RepoName};
use bazel_packages::AttrType;
use bazel_starlark::attr_module::Attribute;
use bazel_starlark::repo_api::StarlarkAttr;
use intern::PathId;
use starlark::values::Value;

/// Resolves label strings in attribute values.
pub struct AttrContext<'a> {
    pub repo: RepoName,
    pub mapping: &'a dyn RepoMapping,
}

impl AttrContext<'_> {
    fn label(&self, text: &str) -> anyhow::Result<Label> {
        let context = LabelContext {
            package: PackageId::new(self.repo, PathId::ROOT),
            mapping: self.mapping,
            lenient: false,
        };
        Ok(Label::parse(text, &context)?)
    }

    fn convert_label(&self, value: &StarValue) -> anyhow::Result<StarValue> {
        match value {
            StarValue::String(text) => Ok(StarValue::Label(self.label(text)?)),
            StarValue::Label(_) | StarValue::None => Ok(value.clone()),
            other => anyhow::bail!("expected a label, got {other:?}"),
        }
    }

    /// Convert a value to an attribute of type `ty`: label strings become
    /// labels; everything else is kept as given.
    pub fn convert(&self, ty: AttrType, value: &StarValue) -> anyhow::Result<StarValue> {
        match (ty, value) {
            (AttrType::Label, value) => self.convert_label(value),
            (AttrType::LabelList, StarValue::List(items)) => {
                Ok(StarValue::List(items.iter().map(|item| self.convert_label(item)).collect::<anyhow::Result<_>>()?))
            }
            (AttrType::LabelKeyedStringDict, StarValue::Dict(entries)) => Ok(StarValue::Dict(
                entries
                    .iter()
                    .map(|(k, v)| Ok((self.convert_label(k)?, v.clone())))
                    .collect::<anyhow::Result<_>>()?,
            )),
            (AttrType::StringKeyedLabelDict, StarValue::Dict(entries)) => Ok(StarValue::Dict(
                entries
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), self.convert_label(v)?)))
                    .collect::<anyhow::Result<_>>()?,
            )),
            _ => Ok(value.clone()),
        }
    }
}

/// The value an attribute has when not given.
pub fn type_default(ty: AttrType) -> StarValue {
    match ty {
        AttrType::Bool => StarValue::Bool(false),
        AttrType::Int | AttrType::Tristate => StarValue::Int(0),
        AttrType::String => StarValue::String(String::new()),
        AttrType::Label | AttrType::Output | AttrType::License => StarValue::None,
        AttrType::StringList | AttrType::LabelList | AttrType::OutputList | AttrType::IntList => StarValue::List(Vec::new()),
        AttrType::StringDict | AttrType::StringListDict | AttrType::LabelKeyedStringDict | AttrType::StringKeyedLabelDict => {
            StarValue::Dict(Vec::new())
        }
    }
}

/// Apply a schema to given attribute values: convert each, fill in
/// defaults, and check mandatory ones are given.
pub fn apply_schema(
    what: &str,
    schema: &[(String, Value<'_>)],
    given: &[(String, StarValue)],
    given_context: &AttrContext<'_>,
    default_context: &AttrContext<'_>,
) -> anyhow::Result<Vec<(String, StarValue)>> {
    let mut attrs = Vec::with_capacity(schema.len());
    for (name, descriptor) in schema {
        let Some(attribute) = Attribute::from_value(*descriptor) else {
            anyhow::bail!("{what}: attribute '{name}' isn't an attr.*() value");
        };
        let value = match given.iter().find(|(given_name, _)| given_name == name) {
            Some((_, value)) => given_context.convert(attribute.ty, value)?,
            None if attribute.mandatory => anyhow::bail!("{what}: missing value for mandatory attribute '{name}'"),
            None => match attribute.default {
                Some(default) => default_context.convert(attribute.ty, &StarValue::from_value(default)?)?,
                None => type_default(attribute.ty),
            },
        };
        attrs.push((name.clone(), value));
    }
    for (name, value) in given {
        if !schema.iter().any(|(known, _)| known == name) {
            attrs.push((name.clone(), value.clone()));
        }
    }
    Ok(attrs)
}

/// Copy an attribute value a repository rule call recorded.
pub fn from_recorded(value: &StarlarkAttr) -> StarValue {
    match value {
        StarlarkAttr::None => StarValue::None,
        StarlarkAttr::Bool(b) => StarValue::Bool(*b),
        StarlarkAttr::Int(i) => StarValue::Int(*i),
        StarlarkAttr::String(s) => StarValue::String(s.clone()),
        StarlarkAttr::Label(label) => StarValue::Label(*label),
        StarlarkAttr::List(items) => StarValue::List(items.iter().map(from_recorded).collect()),
        StarlarkAttr::Dict(entries) => StarValue::Dict(entries.iter().map(|(k, v)| (from_recorded(k), from_recorded(v))).collect()),
    }
}
