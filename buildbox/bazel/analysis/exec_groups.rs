//! `ctx.exec_groups`: the exec groups a rule declares.
//!
//! bb runs every action on the default execution platform and resolves no
//! toolchains per group yet, so a rule can ask which groups it has, but
//! not for one's context.

use allocative::Allocative;
use starlark::starlark_simple_value;
use starlark::values::{Heap, NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Value, starlark_value};
use std::fmt;

/// The exec groups of the rule being analyzed.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct ExecGroups {
    /// The target being analyzed, as Bazel describes it in errors.
    pub target: String,
    pub names: Vec<String>,
}

starlark_simple_value!(ExecGroups);

impl fmt::Display for ExecGroups {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<ctx.exec_groups>")
    }
}

impl ExecGroups {
    fn name<'v>(&self, key: Value<'v>) -> anyhow::Result<&'v str> {
        key.unpack_str().ok_or_else(|| {
            anyhow::anyhow!("exec groups only support indexing by exec group name, got {} of type {} instead", key.to_repr(), key.get_type())
        })
    }
}

#[starlark_value(type = "ExecGroupCollection")]
impl<'v> StarlarkValue<'v> for ExecGroups {
    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        let name = self.name(other).map_err(starlark::Error::new_other)?;
        Ok(self.names.iter().any(|candidate| candidate == name))
    }

    fn at(&self, index: Value<'v>, _heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let name = self.name(index).map_err(starlark::Error::new_other)?;
        if !self.names.iter().any(|candidate| candidate == name) {
            return Err(starlark::Error::new_other(anyhow::anyhow!(
                "In {}, unrecognized exec group '{name}' requested. Available exec groups: [{}]",
                self.target,
                self.names.join(", ")
            )));
        }
        Err(starlark::Error::new_other(anyhow::anyhow!("exec group '{name}': exec groups aren't supported by bb yet")))
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
            let groups = ExecGroups {
                target: "cc_test rule //a:b".to_string(),
                names: vec!["cpp_link".to_string(), "test".to_string()],
            };
            module.set("groups", module.heap().alloc(groups));
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
    }

    #[test]
    fn name_the_available_groups_when_one_is_missing() {
        let error = eval("groups['other']").unwrap_err();
        assert!(error.contains("In cc_test rule //a:b, unrecognized exec group 'other' requested. Available exec groups: [cpp_link, test]"), "{error}");
        assert!(eval("groups['test']").unwrap_err().contains("exec groups aren't supported by bb yet"));
    }
}
