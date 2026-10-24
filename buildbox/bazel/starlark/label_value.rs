//! `Label`, the Starlark type of labels.

use crate::context::EvalContext;
use allocative::Allocative;
use bazel_label::Label;
use starlark::environment::{GlobalsBuilder, Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::{
    NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, ValueLike, starlark_value,
};
use std::cmp::Ordering;
use std::fmt;
use std::hash::Hash;

/// A label, as a Starlark value. Converts to its canonical form,
/// `@@repo//package:name`, with `str()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct LabelValue(#[allocative(skip)] pub Label);

starlark_simple_value!(LabelValue);

impl fmt::Display for LabelValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.canonical_form())
    }
}

#[starlark_value(type = "Label")]
impl<'v> StarlarkValue<'v> for LabelValue {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("Label", label_methods);
        Some(METHODS.methods())
    }

    fn collect_repr(&self, collector: &mut String) {
        collector.push_str(&format!("Label({:?})", self.0.canonical_form()));
    }

    fn collect_str(&self, collector: &mut String) {
        collector.push_str(&self.0.canonical_form());
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other.downcast_ref::<LabelValue>().is_some_and(|other| other.0 == self.0))
    }

    fn compare(&self, other: Value<'v>) -> starlark::Result<Ordering> {
        match other.downcast_ref::<LabelValue>() {
            Some(other) => Ok(self.0.to_string().cmp(&other.0.to_string())),
            None => starlark::values::ValueError::unsupported_with(self, "compare", other),
        }
    }

    fn write_hash(&self, hasher: &mut starlark::collections::StarlarkHasher) -> starlark::Result<()> {
        self.0.hash(hasher);
        Ok(())
    }
}

#[starlark_module]
fn label_methods(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn name(this: &LabelValue) -> anyhow::Result<String> {
        Ok(this.0.name.as_str().to_string())
    }

    #[starlark(attribute)]
    fn package(this: &LabelValue) -> anyhow::Result<String> {
        Ok(this.0.package.path.to_string())
    }

    #[starlark(attribute)]
    fn repo_name(this: &LabelValue) -> anyhow::Result<String> {
        Ok(this.0.repo().as_str().to_string())
    }

    #[starlark(attribute)]
    fn workspace_name(this: &LabelValue) -> anyhow::Result<String> {
        Ok(this.0.repo().as_str().to_string())
    }

    #[starlark(attribute)]
    fn workspace_root(this: &LabelValue) -> anyhow::Result<String> {
        if this.0.repo().is_main() {
            return Ok(String::new());
        }
        Ok(format!("external/{}", this.0.repo()))
    }

    fn same_package_label(this: &LabelValue, target_name: &str) -> anyhow::Result<LabelValue> {
        let name = crate::convert::target_name(target_name)?;
        Ok(LabelValue(this.0.sibling(name)))
    }

    fn relative<'v>(this: &LabelValue, relative_name: &str, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<LabelValue> {
        let context = EvalContext::get(eval)?;
        let mapping = context.mapping_for(this.0.repo());
        let label = Label::parse(
            relative_name,
            &bazel_label::LabelContext {
                package: this.0.package,
                mapping: &*mapping,
            },
        )?;
        Ok(LabelValue(label))
    }
}

#[starlark_module]
pub(crate) fn label_constructor(builder: &mut GlobalsBuilder) {
    /// `Label(text)`: the label `text` names, relative to the `.bzl` file
    /// (or BUILD file) the call is in.
    #[starlark(as_type = LabelValue)]
    fn Label<'v>(#[starlark(require = pos)] input: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<LabelValue> {
        if let Some(label) = input.downcast_ref::<LabelValue>() {
            return Ok(*label);
        }
        let Some(text) = input.unpack_str() else {
            anyhow::bail!("Label() takes a string, not {}", input.get_type());
        };
        let context = EvalContext::get(eval)?;
        Ok(LabelValue(context.parse_label_in_caller(text, eval)?))
    }
}
