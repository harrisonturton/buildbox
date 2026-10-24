//! `config_common.toolchain_type()`: a rule's requirement for a toolchain.

use crate::context::EvalContext;
use allocative::Allocative;
use bazel_label::Label;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::{NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, ValueLike, starlark_value};
use std::fmt;

/// A toolchain type a rule needs, and whether it must find a toolchain of
/// that type.
#[derive(Debug, Clone, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct ToolchainType {
    #[allocative(skip)]
    pub label: Label,
    pub mandatory: bool,
}

starlark_simple_value!(ToolchainType);

impl fmt::Display for ToolchainType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "config_common.toolchain_type({}, mandatory = {})", self.label, self.mandatory)
    }
}

#[starlark_value(type = "toolchain_type")]
impl<'v> StarlarkValue<'v> for ToolchainType {}

/// `toolchain_type(name, mandatory = True)`, resolving `name` relative to
/// the calling file.
pub fn toolchain_type<'v>(name: Value<'v>, mandatory: bool, eval: &Evaluator<'v, '_, '_>) -> anyhow::Result<ToolchainType> {
    let context = EvalContext::get(eval)?;
    let label = match name.downcast_ref::<crate::label_value::LabelValue>() {
        Some(label) => label.0,
        None => {
            let Some(text) = name.unpack_str() else {
                anyhow::bail!("toolchain_type() takes a label");
            };
            context.parse_label_in_caller(text, eval)?
        }
    };
    Ok(ToolchainType { label, mandatory })
}

#[starlark_module]
fn config_common_functions(builder: &mut GlobalsBuilder) {
    fn toolchain_type<'v>(
        #[starlark(require = pos)] name: Value<'v>,
        #[starlark(require = named, default = true)] mandatory: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<ToolchainType> {
        self::toolchain_type(name, mandatory, eval)
    }

    const FeatureFlagInfo: crate::opaque::Opaque = crate::opaque::Opaque {
        type_name: String::new(),
    };
}

/// Add the `config_common` namespace.
pub(crate) fn register(builder: &mut GlobalsBuilder) {
    builder.namespace("config_common", config_common_functions);
}
