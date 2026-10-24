//! `repository_rule()`, `module_extension()` and `tag_class()`: what `.bzl`
//! files use to define external repositories.

use crate::context::EvalContext;
use allocative::Allocative;
use bazel_label::Label;
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
use std::fmt;
use std::sync::OnceLock;

/// A repository a module extension defined by calling a repository rule.
#[derive(Clone, Debug)]
pub struct RepoDefinition {
    pub name: String,
    /// The `.bzl` file exporting the rule, and its exported name.
    pub rule: (Label, String),
    /// The attributes, as given; their values are copied out by the caller.
    pub attrs: Vec<(String, StarlarkAttr)>,
}

/// An attribute value copied out of a Starlark heap: its JSON form, which
/// can hold every value repository rule attributes take, and the labels
/// within it, which JSON can't.
#[derive(Clone, Debug)]
pub enum StarlarkAttr {
    None,
    Bool(bool),
    Int(i64),
    String(String),
    Label(Label),
    List(Vec<StarlarkAttr>),
    Dict(Vec<(StarlarkAttr, StarlarkAttr)>),
}

impl StarlarkAttr {
    pub fn from_value(value: Value<'_>) -> anyhow::Result<StarlarkAttr> {
        if value.is_none() {
            return Ok(StarlarkAttr::None);
        }
        if let Some(b) = value.unpack_bool() {
            return Ok(StarlarkAttr::Bool(b));
        }
        if let Some(i) = value.unpack_i32() {
            return Ok(StarlarkAttr::Int(i as i64));
        }
        if let Some(s) = value.unpack_str() {
            return Ok(StarlarkAttr::String(s.to_string()));
        }
        if let Some(label) = value.downcast_ref::<crate::label_value::LabelValue>() {
            return Ok(StarlarkAttr::Label(label.0));
        }
        if let Some(items) = crate::convert::items(value) {
            return Ok(StarlarkAttr::List(items.into_iter().map(StarlarkAttr::from_value).collect::<anyhow::Result<_>>()?));
        }
        if let Some(dict) = DictRef::from_value(value) {
            let mut entries = Vec::with_capacity(dict.len());
            for (k, v) in dict.iter() {
                entries.push((StarlarkAttr::from_value(k)?, StarlarkAttr::from_value(v)?));
            }
            return Ok(StarlarkAttr::Dict(entries));
        }
        anyhow::bail!("repository rule attributes can't be of type '{}'", value.get_type())
    }
}

/// A repository rule defined with `repository_rule()`.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagable)]
#[starlark_pagable(bound = "V: starlark::pagable::StarlarkPagable")]
#[repr(C)]
pub struct RepositoryRuleGen<V: ValueLifetimeless> {
    pub implementation: V,
    pub attrs: Vec<(String, V)>,
    /// The environment variables the rule reads.
    pub environ: Vec<String>,
    #[freeze(identity)]
    #[allocative(skip)]
    #[starlark_pagable(serialize_with = "crate::paging::serialize_encoded", deserialize_with = "crate::paging::deserialize_encoded")]
    pub definition: Label,
    #[freeze(identity)]
    #[allocative(skip)]
    #[starlark_pagable(serialize_with = "crate::paging::serialize_once_lock", deserialize_with = "crate::paging::deserialize_once_lock")]
    pub name: OnceLock<String>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<RepositoryRuleGen<To>> for RepositoryRuleGen<From> {}

starlark_complex_value!(pub RepositoryRule);

impl<'v, V: ValueLike<'v>> fmt::Display for RepositoryRuleGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<repository rule {}>", self.name.get().map(String::as_str).unwrap_or("unexported"))
    }
}

#[starlark_value(type = "repository_rule")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for RepositoryRuleGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn export_as(&self, variable_name: &str, _eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<()> {
        let _ = self.name.set(variable_name.to_string());
        Ok(())
    }

    fn invoke(&self, _me: Value<'v>, args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> starlark::Result<Value<'v>> {
        define_repo(self, args, eval).map_err(starlark::Error::new_other)?;
        Ok(Value::new_none())
    }
}

/// Record a call to a repository rule in the module extension being
/// evaluated.
fn define_repo<'v, V: ValueLike<'v>>(rule: &RepositoryRuleGen<V>, args: &Arguments<'v, '_>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<()> {
    let context = EvalContext::get(eval)?;
    let Some(repos) = &context.repos else {
        anyhow::bail!("repository rules can only be called from a module extension's implementation");
    };
    let Some(rule_name) = rule.name.get() else {
        anyhow::bail!("this repository rule hasn't been exported: assign it to a global in a .bzl file");
    };
    args.no_positional_args(eval.heap()).map_err(|error| anyhow::anyhow!("{error}"))?;
    let names = args.names_map().map_err(|error| anyhow::anyhow!("{error}"))?;
    let mut name = None;
    let mut attrs = Vec::with_capacity(names.len());
    for (attr, value) in names.iter() {
        if attr.as_str() == "name" {
            name = value.unpack_str().map(str::to_string);
            continue;
        }
        if value.is_none() {
            continue;
        }
        let known = rule.attrs.iter().any(|(known, _)| known == attr.as_str()) || is_common_repo_attr(attr.as_str());
        if !known {
            anyhow::bail!("{rule_name}(): no such attribute '{}'", attr.as_str());
        }
        attrs.push((attr.as_str().to_string(), StarlarkAttr::from_value(*value)?));
    }
    let Some(name) = name else {
        anyhow::bail!("{rule_name}(): missing mandatory attribute 'name'");
    };
    let mut repos = repos.borrow_mut();
    if repos.iter().any(|repo: &RepoDefinition| repo.name == name) {
        anyhow::bail!("a repository named '{name}' was already defined by this module extension");
    }
    repos.push(RepoDefinition {
        name,
        rule: (rule.definition, rule_name.clone()),
        attrs,
    });
    Ok(())
}

/// Attributes every repository rule accepts.
fn is_common_repo_attr(name: &str) -> bool {
    matches!(name, "repo_mapping")
}

/// A module extension defined with `module_extension()`.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagable)]
#[starlark_pagable(bound = "V: starlark::pagable::StarlarkPagable")]
#[repr(C)]
pub struct ModuleExtensionGen<V: ValueLifetimeless> {
    pub implementation: V,
    pub tag_classes: Vec<(String, V)>,
    pub environ: Vec<String>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<ModuleExtensionGen<To>> for ModuleExtensionGen<From> {}

starlark_complex_value!(pub ModuleExtension);

impl<'v, V: ValueLike<'v>> fmt::Display for ModuleExtensionGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<module extension>")
    }
}

#[starlark_value(type = "module_extension")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ModuleExtensionGen<V> where Self: ProvidesStaticType<'v> {}

/// A tag class defined with `tag_class()`: the attributes its tags take.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagable)]
#[starlark_pagable(bound = "V: starlark::pagable::StarlarkPagable")]
#[repr(C)]
pub struct TagClassGen<V: ValueLifetimeless> {
    pub attrs: Vec<(String, V)>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<TagClassGen<To>> for TagClassGen<From> {}

starlark_complex_value!(pub TagClass);

impl<'v, V: ValueLike<'v>> fmt::Display for TagClassGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<tag class>")
    }
}

#[starlark_value(type = "tag_class")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for TagClassGen<V> where Self: ProvidesStaticType<'v> {}

fn attr_entries<'v>(value: Option<Value<'v>>, what: &str) -> anyhow::Result<Vec<(String, Value<'v>)>> {
    let Some(value) = value.filter(|value| !value.is_none()) else {
        return Ok(Vec::new());
    };
    let Some(dict) = DictRef::from_value(value) else {
        anyhow::bail!("{what} must be a dict");
    };
    let mut entries = Vec::with_capacity(dict.len());
    for (name, attr) in dict.iter() {
        let Some(name) = name.unpack_str() else {
            anyhow::bail!("{what} names must be strings");
        };
        entries.push((name.to_string(), attr));
    }
    Ok(entries)
}

#[starlark_module]
pub(crate) fn repo_functions(builder: &mut GlobalsBuilder) {
    fn repository_rule<'v>(
        implementation: Value<'v>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named)] environ: Option<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<RepositoryRule<'v>> {
        let _ = kwargs;
        let context = EvalContext::get(eval)?;
        Ok(RepositoryRule {
            implementation,
            attrs: attr_entries(attrs, "repository_rule(): attrs")?,
            environ: environ.and_then(crate::convert::strings).unwrap_or_default(),
            definition: context.caller_file(eval),
            name: OnceLock::new(),
        })
    }

    fn module_extension<'v>(
        implementation: Value<'v>,
        #[starlark(require = named)] tag_classes: Option<Value<'v>>,
        #[starlark(require = named)] environ: Option<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
    ) -> anyhow::Result<ModuleExtension<'v>> {
        let _ = kwargs;
        Ok(ModuleExtension {
            implementation,
            tag_classes: attr_entries(tag_classes, "module_extension(): tag_classes")?,
            environ: environ.and_then(crate::convert::strings).unwrap_or_default(),
        })
    }

    fn tag_class<'v>(
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
    ) -> anyhow::Result<TagClass<'v>> {
        let _ = kwargs;
        Ok(TagClass {
            attrs: attr_entries(attrs, "tag_class(): attrs")?,
        })
    }
}
