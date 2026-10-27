//! The functions and values only `.bzl` files see.

use crate::context::EvalContext;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::none::NoneType;
use allocative::Allocative;
use starlark::starlark_simple_value;
use starlark::values::{NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, StringValue, Value, starlark_value};

#[starlark_module]
pub(crate) fn bzl_functions(builder: &mut GlobalsBuilder) {
    /// `visibility(packages)`: restricts which packages may load this file.
    /// Accepted and not yet enforced.
    fn visibility<'v>(value: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<NoneType> {
        let _ = (value, EvalContext::get(eval)?);
        Ok(NoneType)
    }

    /// `configuration_field(fragment, name)`: a late-bound default.
    fn configuration_field(fragment: &str, name: &str) -> anyhow::Result<LateBoundDefault> {
        Ok(LateBoundDefault {
            fragment: fragment.to_string(),
            name: name.to_string(),
        })
    }

    fn exec_group<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<crate::opaque::Opaque> {
        let _ = kwargs;
        Ok(crate::opaque::Opaque::new("ExecGroup"))
    }
}

#[starlark_module]
pub(crate) fn macro_function(builder: &mut GlobalsBuilder) {
    /// `macro(implementation, attrs = {}, inherit_attrs = None, ...)`: a
    /// symbolic macro. Called, it runs its implementation with `name`,
    /// `visibility` and the attributes it was given.
    fn r#macro<'v>(
        #[starlark(require = named)] implementation: Value<'v>,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<crate::symbolic_macro::SymbolicMacro<'v>> {
        let _ = kwargs;
        let definition = EvalContext::get(eval)?.caller_file(eval);
        Ok(crate::symbolic_macro::SymbolicMacro {
            implementation,
            exported: std::sync::OnceLock::new(),
            definition,
        })
    }
}

/// What `configuration_field()` returns: a default label the build's
/// configuration chooses, such as `--proto_compiler`'s value.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct LateBoundDefault {
    pub fragment: String,
    pub name: String,
}

starlark_simple_value!(LateBoundDefault);

impl std::fmt::Display for LateBoundDefault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<late-bound default {}.{}>", self.fragment, self.name)
    }
}

#[starlark_value(type = "LateBoundDefault")]
impl<'v> StarlarkValue<'v> for LateBoundDefault {}
