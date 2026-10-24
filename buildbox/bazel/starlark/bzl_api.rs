//! The functions and values only `.bzl` files see.

use crate::context::EvalContext;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::none::NoneType;
use starlark::values::{StringValue, Value};

#[starlark_module]
pub(crate) fn bzl_functions(builder: &mut GlobalsBuilder) {
    /// `visibility(packages)`: restricts which packages may load this file.
    /// Accepted and not yet enforced.
    fn visibility<'v>(#[starlark(require = pos)] value: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<NoneType> {
        let _ = (value, EvalContext::get(eval)?);
        Ok(NoneType)
    }

    /// `aspect(implementation, ...)`.
    fn aspect<'v>(
        implementation: Value<'v>,
        #[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>,
    ) -> anyhow::Result<crate::opaque::Opaque> {
        let _ = (implementation, kwargs);
        Ok(crate::opaque::Opaque::new("Aspect"))
    }

    /// `transition(implementation, inputs, outputs)`.
    fn transition<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<crate::opaque::Opaque> {
        let _ = kwargs;
        Ok(crate::opaque::Opaque::new("transition"))
    }

    fn analysis_test_transition<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<crate::opaque::Opaque> {
        let _ = kwargs;
        Ok(crate::opaque::Opaque::new("transition"))
    }

    /// `configuration_field(fragment, name)`: a late-bound default.
    fn configuration_field(#[starlark(require = named)] fragment: &str, #[starlark(require = named)] name: &str) -> anyhow::Result<crate::opaque::Opaque> {
        let _ = (fragment, name);
        Ok(crate::opaque::Opaque::new("LateBoundDefault"))
    }

    fn exec_group<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<crate::opaque::Opaque> {
        let _ = kwargs;
        Ok(crate::opaque::Opaque::new("ExecGroup"))
    }

    fn subrule<'v>(#[starlark(kwargs)] kwargs: SmallMap<StringValue<'v>, Value<'v>>) -> anyhow::Result<crate::opaque::Opaque> {
        let _ = kwargs;
        Ok(crate::opaque::Opaque::new("Subrule"))
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
    ) -> anyhow::Result<crate::symbolic_macro::SymbolicMacro<'v>> {
        let _ = kwargs;
        Ok(crate::symbolic_macro::SymbolicMacro { implementation })
    }
}
