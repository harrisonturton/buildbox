//! `Label`, the Starlark type of labels.

use crate::context::EvalContext;
use allocative::Allocative;
use bazel_label::Label;
use starlark::environment::{GlobalsBuilder, Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::{
    NoSerialize, ProvidesStaticType, StarlarkPagable, StarlarkValue, Value, ValueLike, starlark_value,
};
use std::cmp::Ordering;
use std::fmt;
use std::hash::Hash;

/// A label, as a Starlark value. As in Bazel, `str()` gives its
/// unambiguous canonical form, `@@repo//package:name` or `@@//package:name`,
/// and `repr()` its canonical form in a call, `Label("//package:name")`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagable)]
pub struct LabelValue(#[starlark_pagable(serialize_with = "crate::paging::serialize_encoded", deserialize_with = "crate::paging::deserialize_encoded")] #[allocative(skip)] pub Label);

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

    /// `Label("//package:name")`, or `Label("@@repo//package:name")` for
    /// another repository's.
    fn collect_repr(&self, collector: &mut String) {
        collector.push_str("Label(");
        collector.push_str(&starlark::values::string::StarlarkStr::repr(&self.0.to_string()));
        collector.push(')');
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
                lenient: false,
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

#[cfg(test)]
mod tests {
    use super::*;
    use starlark::environment::{Globals, Module};
    use starlark::syntax::{AstModule, Dialect};

    /// What `expr` gives with `main` and `other` labels of the main
    /// repository and another.
    fn formatted(expr: &str) -> String {
        Module::with_temp_heap(|module| {
            module.set("main", module.heap().alloc(LabelValue(Label::parse_canonical("@@//pkg:x").unwrap())));
            module.set("other", module.heap().alloc(LabelValue(Label::parse_canonical("@@rules_rust+//pkg:x").unwrap())));
            let ast = AstModule::parse("test.bzl", expr.to_string(), &Dialect::Extended).unwrap();
            let mut eval = Evaluator::new(&module);
            eval.eval_module(ast, &Globals::standard()).unwrap().unpack_str().unwrap().to_string()
        })
    }

    #[test]
    fn labels_print_as_bazel_prints_them() {
        assert_eq!(formatted("repr(main)"), r#"Label("//pkg:x")"#);
        assert_eq!(formatted("repr(other)"), r#"Label("@@rules_rust+//pkg:x")"#);
        assert_eq!(formatted("str(main)"), "@@//pkg:x");
        assert_eq!(formatted("'%s' % main"), "@@//pkg:x");
        assert_eq!(formatted("'%s %s' % (main, other)"), "@@//pkg:x @@rules_rust+//pkg:x");
        assert_eq!(formatted("'<{}>'.format(main)"), "<@@//pkg:x>");
        assert_eq!(formatted("'%r' % main"), r#"Label("//pkg:x")"#);
        assert_eq!(formatted("str([main])"), r#"[Label("//pkg:x")]"#);
    }
}
