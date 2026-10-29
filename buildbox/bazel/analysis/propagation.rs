//! The functions of a target that decide where an aspect propagates, as
//! Bazel calls them: `propagation_predicate`, and `attr_aspects` and
//! `toolchains_aspects` given as functions.

use crate::aspect::LoadedAspect;
use bazel_config::AspectDescriptor;
use bazel_label::Label;
use bazel_packages::{AttrSchema, AttrType, AttrValue, Cfg, PackageError, RuleClassRef};
use bazel_starlark::attr_module::Attribute;
use bazel_starlark::aspect::{Aspect, AspectGen};
use bazel_starlark::label_value::LabelValue;
use bazel_starlark::{EvalContext, FileKind};
use intern::Symbol;
use skyframe::{Ctx, SkyError};
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::list::ListRef;
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, Value, ValueLike};

/// A function of a target an aspect may have that decides where it
/// propagates.
#[derive(Clone, Copy)]
pub(crate) enum PropagationFunction {
    /// `propagation_predicate`: whether it's applied to the target.
    Predicate,
    /// `attr_aspects`: the attributes it propagates along from the target.
    AttrAspects,
    /// `toolchains_aspects`: the toolchain types it propagates to.
    ToolchainsAspects,
}

/// What a propagation function returned.
pub(crate) enum Propagation {
    Allowed(bool),
    Attrs(Vec<String>),
    ToolchainTypes(Vec<Label>),
}

/// The target a propagation function is called for: its label, kind, and
/// the attributes it sees, each with its value and whether it's a tool.
pub(crate) struct PropagationTarget<'a> {
    pub label: Label,
    pub class_ref: &'a RuleClassRef,
    pub attrs: Vec<(Symbol, AttrValue, bool)>,
}

impl PropagationTarget<'_> {
    /// The attributes a rule's target gives the functions deciding which
    /// attributes and toolchains aspects propagate along: all but Bazel's
    /// internal ones, with whether each is built for the exec platform.
    pub fn attrs_of(schemas: &[AttrSchema], attrs: &[(Symbol, AttrValue)]) -> Vec<(Symbol, AttrValue, bool)> {
        let mut seen = Vec::with_capacity(attrs.len());
        for (schema, (name, value)) in schemas.iter().zip(attrs) {
            if name.as_str().starts_with('$') || name.as_str().starts_with(':') {
                continue;
            }
            seen.push((*name, value.clone(), matches!(schema.cfg, Cfg::Exec(_))));
        }
        seen
    }
}

/// Call an aspect's propagation function, `which`, for a target, if it has
/// one. What it prints is reported; if it fails, so is why, which it fails
/// with, as Bazel's do.
pub(crate) async fn call(
    ctx: &Ctx,
    aspect: &LoadedAspect,
    which: PropagationFunction,
    descriptor: &AspectDescriptor,
    target: &PropagationTarget<'_>,
) -> Result<Option<Propagation>, SkyError> {
    if function(aspect.get(), which).is_none() {
        return Ok(None);
    }
    let mut printed = Vec::new();
    let returned = Module::with_temp_heap(|module| -> Result<Propagation, PackageError> {
        let heap = module.heap();
        let value = heap.access_owned_frozen_value(aspect.owned());
        let aspect = Aspect::from_value(value).ok_or_else(|| PackageError::new("not an aspect"))?;
        let function = function(aspect, which).ok_or_else(|| PackageError::new("no propagation function"))?;
        let context = context(aspect, descriptor, target, heap);
        let eval_context = EvalContext::new(FileKind::Bzl, aspect.definition);
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&eval_context);
        let returned = eval.eval_function(function, &[context], &[]);
        printed = eval_context.printed.take();
        let returned = returned.map_err(|error| bazel_starlark::reported::package_error(&error, &[]))?;
        returned_propagation(returned, which).map_err(PackageError::new)
    });
    bazel_loading::report_printed(ctx, &printed).await;
    match returned {
        Ok(returned) => Ok(Some(returned)),
        Err(error) => Err(failed(ctx, &error, which, target.label).await),
    }
}

fn function<'v>(aspect: &AspectGen<Value<'v>>, which: PropagationFunction) -> Option<Value<'v>> {
    match which {
        PropagationFunction::Predicate => aspect.propagation_predicate,
        PropagationFunction::AttrAspects => aspect.attr_aspects_function,
        PropagationFunction::ToolchainsAspects => aspect.toolchains_aspects_function,
    }
}

/// What a propagation function returned, checked as Bazel checks it.
fn returned_propagation(returned: Value<'_>, which: PropagationFunction) -> Result<Propagation, String> {
    if let PropagationFunction::Predicate = which {
        return returned.unpack_bool().map(Propagation::Allowed).ok_or_else(|| "Expected a boolean".to_string());
    }
    let Some(list) = ListRef::from_value(returned) else {
        return Err("Expected a list".to_string());
    };
    match which {
        PropagationFunction::AttrAspects => attr_names(list.content()).map(Propagation::Attrs),
        _ => toolchain_types(list.content()).map(Propagation::ToolchainTypes),
    }
}

fn attr_names(values: &[Value<'_>]) -> Result<Vec<String>, String> {
    let mut names = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let Some(name) = value.unpack_str() else {
            return Err(format!("at index {index} of attr_aspects, got element of type {}, want string", value.get_type()));
        };
        if name == "*" {
            return Err("'*' is not allowed in 'attr_aspects' list".to_string());
        }
        names.push(name.to_string());
    }
    Ok(names)
}

fn toolchain_types(values: &[Value<'_>]) -> Result<Vec<Label>, String> {
    let mut types = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let Some(label) = value.downcast_ref::<LabelValue>() else {
            return Err(format!("at index {index} of toolchains_aspects, got element of type {}, want Label", value.get_type()));
        };
        types.push(label.0);
    }
    Ok(types)
}

/// A propagation function's error, as Bazel reports it: in full, with its
/// traceback, then its message alone, which the analysis fails with.
/// Bazel calls the functions deciding where an aspect propagates as it
/// analyzes the aspect on a target, so places their errors at the target,
/// whose analysis is what fails.
async fn failed(ctx: &Ctx, error: &PackageError, which: PropagationFunction, label: Label) -> SkyError {
    let paths = bazel_loading::error_paths(ctx, error).await;
    let reported = error.describe(&|file| paths.get(file).cloned().unwrap_or_else(|| file.to_string()));
    let message = match error.message.strip_prefix("Error") {
        Some(rest) => rest.split_once(": ").map(|(_, message)| message).unwrap_or(rest),
        None => &error.message,
    };
    if let PropagationFunction::Predicate = which {
        return SkyError::new(crate::configured::errors(&[&reported, message]));
    }
    let reported = match bazel_loading::target_location(ctx, label).await {
        Some(location) => format!("{location}: {reported}"),
        None => reported,
    };
    SkyError::new(crate::configured::errors(&[&reported, message])).with_origin(label.to_string())
}

/// The `ctx` a propagation function gets: `ctx.attr`, the aspect's
/// parameters, and `ctx.rule`, with the target's `label`, its
/// `qualified_kind`, and its attributes, each with its `value` and
/// whether it `is_tool`.
fn context<'v>(aspect: &AspectGen<Value<'v>>, descriptor: &AspectDescriptor, target: &PropagationTarget<'_>, heap: Heap<'v>) -> Value<'v> {
    let mut attrs = Vec::with_capacity(target.attrs.len());
    for (name, value, is_tool) in &target.attrs {
        let attr = AllocStruct([("value", bazel_starlark::convert::to_starlark(value, heap)), ("is_tool", Value::new_bool(*is_tool))]);
        attrs.push((name.as_str(), heap.alloc(attr)));
    }
    let file_label = match target.class_ref.definition {
        Some((file, _)) => heap.alloc(LabelValue(file)),
        None => Value::new_none(),
    };
    let kind = heap.alloc(AllocStruct([("file_label", file_label), ("rule_name", heap.alloc(target.class_ref.kind.as_str()))]));
    let rule = heap.alloc(AllocStruct([
        ("label", heap.alloc(LabelValue(target.label))),
        ("qualified_kind", kind),
        ("attr", heap.alloc(AllocStruct(attrs))),
    ]));
    heap.alloc(AllocStruct([("attr", parameters(aspect, descriptor, heap)), ("rule", rule)]))
}

/// The aspect's parameters, as the descriptor gives them.
fn parameters<'v>(aspect: &AspectGen<Value<'v>>, descriptor: &AspectDescriptor, heap: Heap<'v>) -> Value<'v> {
    let mut params = Vec::with_capacity(descriptor.params.len());
    for (name, text) in &descriptor.params {
        let attribute = aspect.attrs.iter().find(|(attr, _)| attr == name).and_then(|(_, value)| Attribute::from_value(*value));
        let value = match attribute.map(|attribute| attribute.ty) {
            Some(AttrType::String) => heap.alloc(bazel_config::unquoted(text).unwrap_or_default()),
            Some(AttrType::Int) => heap.alloc(text.parse::<i32>().unwrap_or_default()),
            Some(AttrType::Bool) => Value::new_bool(text == "True"),
            _ => Value::new_none(),
        };
        params.push((name.as_str(), value));
    }
    heap.alloc(AllocStruct(params))
}
