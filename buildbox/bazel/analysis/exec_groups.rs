//! `ctx.exec_groups`: the exec groups a rule declares, each with the
//! toolchains resolved for its execution platform.

use allocative::Allocative;
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
    fn name_the_available_groups_when_one_is_missing() {
        let error = eval("groups['other']").unwrap_err();
        assert!(error.contains("In cc_test rule //a:b, unrecognized exec group 'other' requested. Available exec groups: [cpp_link, test]"), "{error}");
    }
}
