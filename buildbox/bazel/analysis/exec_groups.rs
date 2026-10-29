//! `ctx.exec_groups`: the exec groups a rule declares, each with the
//! toolchains resolved for its execution platform; and the exec
//! properties of each group's actions.

use crate::platform::PlatformKey;
use crate::toolchains::ResolvedToolchains;
use allocative::Allocative;
use bazel_label::Label;
use bazel_packages::AttrValue;
use intern::Symbol;
use skyframe::{Ctx, SkyError};
use std::collections::BTreeMap;
use starlark::coerce::Coerce;
use starlark::starlark_complex_value;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value, ValueLifetimeless, ValueLike,
    starlark_value,
};
use std::fmt;

/// The exec groups of the rule being analyzed.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct ExecGroupsGen<V: ValueLifetimeless> {
    /// The target being analyzed, as Bazel describes it in errors.
    #[freeze(identity)]
    pub target: String,
    /// Each group's name, with its `ToolchainContext`.
    pub groups: Vec<(String, V)>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<ExecGroupsGen<To>> for ExecGroupsGen<From> {}

starlark_complex_value!(pub ExecGroups);

impl<'v, V: ValueLike<'v>> fmt::Display for ExecGroupsGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<ctx.exec_groups: {}>", self.sorted_names().join(", "))
    }
}

impl<'v, V: ValueLike<'v>> ExecGroupsGen<V> {
    fn name(key: Value<'v>) -> anyhow::Result<&'v str> {
        key.unpack_str().ok_or_else(|| {
            anyhow::anyhow!("exec groups only support indexing by exec group name, got {} of type {} instead", key.to_repr(), key.get_type())
        })
    }

    fn sorted_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.groups.iter().map(|(name, _)| name.as_str()).collect();
        names.sort();
        names
    }
}

#[starlark_value(type = "ExecGroupCollection")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ExecGroupsGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        let name = Self::name(other).map_err(starlark::Error::new_other)?;
        Ok(self.groups.iter().any(|(candidate, _)| candidate == name))
    }

    fn at(&self, index: Value<'v>, heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let name = Self::name(index).map_err(starlark::Error::new_other)?;
        match self.groups.iter().find(|(candidate, _)| candidate == name) {
            Some((_, toolchains)) => Ok(heap.alloc(ExecGroupContext { toolchains: toolchains.to_value() })),
            None => Err(starlark::Error::new_other(anyhow::anyhow!(
                "In {}, unrecognized exec group '{name}' requested. Available exec groups: [{}]",
                self.target,
                self.sorted_names().join(", ")
            ))),
        }
    }
}

/// `ctx.exec_groups[name]`: an exec group's toolchains.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct ExecGroupContextGen<V: ValueLifetimeless> {
    pub toolchains: V,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<ExecGroupContextGen<To>> for ExecGroupContextGen<From> {}

starlark_complex_value!(pub ExecGroupContext);

impl<'v, V: ValueLike<'v>> fmt::Display for ExecGroupContextGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<exec_group_context>")
    }
}

#[starlark_value(type = "ExecGroupContext")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ExecGroupContextGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "toolchains" => Some(self.toolchains.to_value()),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        vec!["toolchains".to_string()]
    }
}

/// The properties of each exec group's actions, `None` for the rule's
/// own, as Bazel combines them: the execution platform's, then the
/// target's `exec_properties`, then the platform's and the target's for the
/// group, the later winning. A key `group.key` is for that group.
pub(crate) async fn exec_properties(
    ctx: &Ctx,
    attrs: &[(Symbol, AttrValue)],
    toolchains: &ResolvedToolchains,
    host_platform: Label,
) -> Result<Vec<(Option<Symbol>, Vec<(String, String)>)>, SkyError> {
    let name = Symbol::intern("exec_properties");
    let target = match attrs.iter().find(|(attr, _)| *attr == name) {
        Some((_, AttrValue::StringDict(entries))) => entries.clone(),
        _ => Vec::new(),
    };
    let mut groups = vec![(None, toolchains.exec_platform)];
    groups.extend(toolchains.exec_groups.iter().map(|(group, resolved)| (Some(*group), resolved.exec_platform)));
    let mut properties = Vec::with_capacity(groups.len());
    for (group, platform) in groups {
        let platform = ctx.compute(&PlatformKey(platform.unwrap_or(host_platform))).await?;
        properties.push((group, combined(&platform.exec_properties, &target, group)));
    }
    Ok(properties)
}

/// The properties of `group`'s actions, from the platform's and the
/// target's.
fn combined(platform: &[(String, String)], target: &[(String, String)], group: Option<Symbol>) -> Vec<(String, String)> {
    let mut combined = BTreeMap::new();
    let mut add = |entries: &[(String, String)], wanted: Option<Symbol>| {
        for (key, value) in entries {
            let (entry_group, property) = match key.split_once('.') {
                Some((entry_group, property)) => (Some(entry_group), property),
                None => (None, key.as_str()),
            };
            if entry_group == wanted.map(|group| group.as_str()) {
                combined.insert(property.to_string(), value.clone());
            }
        }
    };
    add(platform, None);
    add(target, None);
    if group.is_some() {
        add(platform, group);
        add(target, None);
        add(target, group);
    }
    combined.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{Globals, Module};
    use starlark::eval::Evaluator;
    use starlark::syntax::{AstModule, Dialect};

    fn eval(code: &str) -> Result<String, String> {
        Module::with_temp_heap(|module| {
            let heap = module.heap();
            let groups = ExecGroups {
                target: "cc_test rule //a:b".to_string(),
                groups: vec![("test".to_string(), heap.alloc("test toolchains")), ("cpp_link".to_string(), heap.alloc("link toolchains"))],
            };
            module.set("groups", heap.alloc(groups));
            let ast = AstModule::parse("test.bzl", code.to_string(), &Dialect::Standard).map_err(|error| error.to_string())?;
            let mut eval = Evaluator::new(&module);
            let value = eval.eval_module(ast, &Globals::standard()).map_err(|error| error.to_string())?;
            Ok(value.to_str())
        })
    }

    #[test]
    fn contain_the_groups_the_rule_declares() {
        assert_eq!(eval("'cpp_link' in groups").unwrap(), "True");
        assert_eq!(eval("'other' in groups").unwrap(), "False");
        assert!(eval("1 in groups").unwrap_err().contains("only support indexing by exec group name"));
        assert_eq!(eval("str(groups)").unwrap(), "<ctx.exec_groups: cpp_link, test>");
    }

    #[test]
    fn give_each_group_its_toolchains() {
        assert_eq!(eval("groups['test'].toolchains").unwrap(), "test toolchains");
        assert_eq!(eval("groups['cpp_link'].toolchains").unwrap(), "link toolchains");
    }

    #[test]
    fn combine_exec_properties_as_bazel_does() {
        let platform = vec![("os".to_string(), "linux".to_string()), ("pool".to_string(), "small".to_string()), ("test.pool".to_string(), "tests".to_string())];
        let target = vec![("pool".to_string(), "big".to_string()), ("test.cpu".to_string(), "8".to_string())];
        let pairs = |entries: &[(&str, &str)]| entries.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect::<Vec<_>>();
        assert_eq!(combined(&platform, &target, None), pairs(&[("os", "linux"), ("pool", "big")]));
        let test = Some(Symbol::intern("test"));
        assert_eq!(combined(&platform, &target, test), pairs(&[("cpu", "8"), ("os", "linux"), ("pool", "big")]));
    }

    #[test]
    fn name_the_available_groups_when_one_is_missing() {
        let error = eval("groups['other']").unwrap_err();
        assert!(error.contains("In cc_test rule //a:b, unrecognized exec group 'other' requested. Available exec groups: [cpp_link, test]"), "{error}");
    }
}
