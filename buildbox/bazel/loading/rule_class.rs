//! The schemas of rule classes, for queries over packages.

use crate::bzl::BzlLoadKey;
use bazel_label::Label;
use bazel_packages::{RuleClass, RuleClassRef};
use bazel_starlark::native_rules::NATIVE_RULES;
use bazel_starlark::rule_value::StarlarkRule;
use intern::Symbol;
use skycodec::{Decode, Encode};
use skyframe::{encoded_value, Compute, Ctx, Key, SkyError};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct RuleClassValue(pub RuleClass);

encoded_value!(RuleClassValue);

/// The schema of a rule class exported from a `.bzl` file.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub struct RuleClassKey {
    pub bzl: Label,
    pub name: Symbol,
}

impl Key for RuleClassKey {
    type Value = RuleClassValue;
    const KIND: &'static str = "RULE_CLASS";

    async fn compute(&self, ctx: &Ctx) -> Result<RuleClassValue, SkyError> {
        let module = ctx.compute(&BzlLoadKey(self.bzl)).await?;
        let missing = || SkyError::new(format!("{} doesn't export a rule named '{}'", self.bzl, self.name));
        let (value, _) = module.module.get_any_visibility(self.name.as_str()).map_err(|_| missing())?;
        let rule = StarlarkRule::from_value(value.value()).ok_or_else(missing)?;
        let class = rule.exported.get().ok_or_else(missing)?;
        Ok(RuleClassValue((**class).clone()))
    }
}

/// The schema of any rule class, native or not.
pub async fn rule_class(ctx: &impl Compute, class: &RuleClassRef) -> Result<Arc<RuleClass>, SkyError> {
    let Some((bzl, name)) = class.definition else {
        return NATIVE_RULES
            .get(&class.kind)
            .cloned()
            .ok_or_else(|| SkyError::new(format!("no native rule named '{}'", class.kind)));
    };
    let value = ctx.compute(&RuleClassKey { bzl, name }).await?;
    Ok(Arc::new(value.0.clone()))
}
