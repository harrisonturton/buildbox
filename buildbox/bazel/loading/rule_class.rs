//! The schemas of rule classes, for queries over packages.

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
        if self.bzl.repo().as_str() == crate::BUILTINS_REPO {
            let builtins = ctx.compute(&crate::BuiltinsKey).await?;
            let class = builtins.rule_classes.get(&self.name).ok_or_else(|| SkyError::new(format!("no builtin rule named '{}'", self.name)))?;
            return Ok(RuleClassValue((**class).clone()));
        }
        if is_build_file(self.bzl) {
            return defined_in_build_file(ctx, self.bzl, self.name).await;
        }
        let module = ctx.compute(&crate::bzl_key_for_build(ctx, self.bzl).await?).await?;
        let missing = || SkyError::new(format!("{} doesn't export a rule named '{}'", self.bzl, self.name));
        let (value, _) = module.module.get_any_visibility(self.name.as_str()).map_err(|_| missing())?;
        let rule = StarlarkRule::from_value(value.value()).ok_or_else(missing)?;
        let class = rule.exported.get().ok_or_else(missing)?;
        Ok(RuleClassValue((**class).clone()))
    }
}

/// Whether a rule class's definition is a BUILD file rather than a `.bzl`
/// file, as for those `testing.analysis_test` defines.
pub fn is_build_file(definition: Label) -> bool {
    crate::package::BUILD_FILE_NAMES.contains(&definition.name.as_str())
}

/// The schema of a rule class a BUILD file defines, which its package
/// keeps.
async fn defined_in_build_file(ctx: &Ctx, build_file: Label, kind: Symbol) -> Result<RuleClassValue, SkyError> {
    let package = ctx.compute(&crate::PackageKey(build_file.package)).await?;
    let class = package.0.defined_classes.iter().find(|class| class.kind == kind);
    let class = class.ok_or_else(|| SkyError::new(format!("{build_file} doesn't define a rule named '{kind}'")))?;
    Ok(RuleClassValue(class.clone()))
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
