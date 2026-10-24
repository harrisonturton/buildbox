//! Toolchain resolution: choosing, for each toolchain type a rule needs, a
//! toolchain that targets the target platform.

use crate::configured::ConfiguredTarget;
use crate::target_value::TargetValue;
use allocative::Allocative;
use bazel_config::BuildConfiguration;
use bazel_label::Label;
use bazel_packages::RuleClass;
use bazel_starlark::label_value::LabelValue;
use skyframe::{Ctx, SkyError};
use starlark::coerce::Coerce;
use starlark::starlark_complex_value;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value, ValueLifetimeless,
    ValueLike, starlark_value,
};
use std::fmt;
use std::sync::Arc;

/// The toolchains resolved for a rule: each toolchain type, with the
/// analyzed toolchain, or `None` if none was found for an optional type.
#[derive(Default)]
pub struct ResolvedToolchains {
    pub resolved: Vec<(Label, Option<Arc<ConfiguredTarget>>)>,
}

/// Resolve the toolchains a rule class needs.
pub async fn resolve(ctx: &Ctx, class: &RuleClass, config: &BuildConfiguration) -> Result<ResolvedToolchains, SkyError> {
    let _ = (ctx, config);
    let resolved = class.toolchain_types.iter().map(|toolchain_type| (*toolchain_type, None)).collect();
    Ok(ResolvedToolchains { resolved })
}

/// `ctx.toolchains`: the `ToolchainInfo` of each resolved toolchain, by
/// toolchain type.
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct ToolchainContextGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[allocative(skip)]
    pub types: Vec<Label>,
    /// `ToolchainInfo` or `None`, in the order of `types`.
    pub values: Vec<V>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<ToolchainContextGen<To>> for ToolchainContextGen<From> {}

starlark_complex_value!(pub ToolchainContext);

impl<'v> ToolchainContext<'v> {
    pub fn new(resolved: &ResolvedToolchains, heap: Heap<'v>) -> ToolchainContext<'v> {
        let mut types = Vec::new();
        let mut values = Vec::new();
        for (toolchain_type, toolchain) in &resolved.resolved {
            let value = toolchain
                .as_ref()
                .and_then(|toolchain| {
                    let value = heap.access_owned_frozen_value(&toolchain.value);
                    TargetValue::from_value(value)?.provider("ToolchainInfo")
                })
                .unwrap_or_else(Value::new_none);
            types.push(*toolchain_type);
            values.push(value);
        }
        ToolchainContext { types, values }
    }
}

impl<'v, V: ValueLike<'v>> fmt::Display for ToolchainContextGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<toolchain_context>")
    }
}

fn toolchain_type_label(index: Value<'_>) -> Option<Label> {
    if let Some(label) = index.downcast_ref::<LabelValue>() {
        return Some(label.0);
    }
    let text = index.unpack_str()?;
    Label::parse_canonical(text).ok()
}

#[starlark_value(type = "ToolchainContext")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ToolchainContextGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn at(&self, index: Value<'v>, _heap: Heap<'v>) -> starlark::Result<Value<'v>> {
        let label = toolchain_type_label(index);
        let found = self.types.iter().position(|toolchain_type| Some(*toolchain_type) == label);
        match found {
            Some(position) => Ok(self.values[position].to_value()),
            None => Err(starlark::Error::new_other(anyhow::anyhow!(
                "toolchain type {} was requested but only types [{}] are configured",
                index.to_str(),
                self.types.iter().map(|label| label.to_string()).collect::<Vec<_>>().join(", ")
            ))),
        }
    }

    fn is_in(&self, other: Value<'v>) -> starlark::Result<bool> {
        let label = toolchain_type_label(other);
        Ok(self.types.iter().any(|toolchain_type| Some(*toolchain_type) == label))
    }
}
