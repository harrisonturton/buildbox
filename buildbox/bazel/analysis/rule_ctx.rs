//! `ctx`: what a rule implementation is given.

use crate::actions;
use crate::artifact::Artifact;
use crate::expand::LocationStyle;
use crate::file_value::FileValue;
use crate::providers;
use crate::runfiles::Runfiles;
use crate::state::RuleState;
use crate::target_value::TargetValue;
use allocative::Allocative;
use bazel_label::Label;
use bazel_starlark::depset::{Depset, Order};
use bazel_starlark::label_value::LabelValue;
use starlark::coerce::Coerce;
use starlark::collections::SmallMap;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::Evaluator;
use starlark::starlark_complex_value;
use starlark::starlark_module;
use starlark::values::dict::DictRef;
use starlark::values::tuple::AllocTuple;
use starlark::values::list::AllocList;
use starlark::values::{
    Freeze, Heap, NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Trace, Value, ValueLifetimeless,
    ValueLike, starlark_value,
};
use std::fmt;

/// The attributes of a rule's dependencies that `ctx.runfiles` collects the
/// runfiles of.
const RUNFILES_ATTRS: &[&str] = &["srcs", "deps", "data"];

/// Which runfiles of its dependencies `ctx.runfiles` collects.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Collect {
    /// `collect_default`: the default runfiles of `srcs` and `deps`, and
    /// the data runfiles of `data`, as Bazel collects them.
    Default,
    /// `collect_data`: the data runfiles of all three.
    Data,
}

/// A rule implementation's `ctx`: its fields, such as `attr` and `actions`,
/// and methods that reach the rule's [`RuleState`].
#[derive(Debug, Trace, Freeze, NoSerialize, ProvidesStaticType, Allocative, StarlarkPagablePanic)]
#[repr(C)]
pub struct RuleCtxGen<V: ValueLifetimeless> {
    #[freeze(identity)]
    #[allocative(skip)]
    pub label: Label,
    pub fields: SmallMap<String, V>,
}

unsafe impl<From: Coerce<To> + ValueLifetimeless, To: ValueLifetimeless> Coerce<RuleCtxGen<To>> for RuleCtxGen<From> {}

starlark_complex_value!(pub RuleCtx);

impl<'v, V: ValueLike<'v>> fmt::Display for RuleCtxGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<rule context for {}>", self.label)
    }
}

#[starlark_value(type = "ctx")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for RuleCtxGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("ctx", ctx_methods);
        Some(METHODS.methods())
    }

    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.fields.get(attribute).map(|value| value.to_value())
    }

    fn dir_attr(&self) -> Vec<String> {
        self.fields.keys().filter(|name| !name.starts_with('$')).cloned().collect()
    }
}

/// The runfiles of the targets of `ctx.attr.srcs`, `deps` and `data`.
fn dep_runfiles<'v>(ctx: Value<'v>, collect: Collect, heap: Heap<'v>) -> Vec<Value<'v>> {
    let Some(attr) = ctx.get_attr("attr", heap).ok().flatten() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in RUNFILES_ATTRS {
        let field = match (collect, *name) {
            (Collect::Default, "srcs" | "deps") => "default_runfiles",
            _ => "data_runfiles",
        };
        let Some(value) = attr.get_attr(name, heap).ok().flatten() else {
            continue;
        };
        let targets = bazel_starlark::convert::items(value).unwrap_or_else(|| vec![value]);
        for target in targets {
            out.extend(providers::collected_runfiles(target, field, heap));
        }
    }
    out
}

/// The files of each target in a list, by label, for `$(location)`.
fn target_files(targets: Option<Value<'_>>) -> anyhow::Result<Vec<(Label, Vec<Artifact>)>> {
    let Some(targets) = targets.filter(|value| !value.is_none()) else {
        return Ok(Vec::new());
    };
    let items = bazel_starlark::convert::items(targets).ok_or_else(|| anyhow::anyhow!("targets must be a list"))?;
    let mut out = Vec::new();
    for item in items {
        let target = TargetValue::from_value(item).ok_or_else(|| anyhow::anyhow!("targets must be a list of targets"))?;
        let files = actions::files(target.default_info("files"), "files")?;
        out.push((target.label, files));
    }
    Ok(out)
}

/// `symlinks` and `root_symlinks` of `ctx.runfiles`: dicts of paths to
/// files.
fn symlinks<'v>(value: Option<Value<'v>>, what: &str) -> anyhow::Result<Vec<(String, Value<'v>)>> {
    let Some(value) = value.filter(|value| !value.is_none()) else {
        return Ok(Vec::new());
    };
    if Depset::from_value(value).is_some() {
        return Ok(Vec::new());
    }
    let dict = DictRef::from_value(value).ok_or_else(|| anyhow::anyhow!("{what} must be a dict"))?;
    let mut out = Vec::new();
    for (path, file) in dict.iter() {
        let path = path.unpack_str().ok_or_else(|| anyhow::anyhow!("{what} keys must be strings"))?;
        if FileValue::artifact(file).is_none() {
            anyhow::bail!("{what} values must be files");
        }
        out.push((path.to_string(), file));
    }
    Ok(out)
}

#[starlark_module]
fn ctx_methods(builder: &mut MethodsBuilder) {
    /// `runfiles(files = [], transitive_files = None, collect_data = False,
    /// collect_default = False, symlinks = {}, root_symlinks = {})`.
    fn runfiles<'v>(
        #[starlark(this)] this: Value<'v>,
        files: Option<Value<'v>>,
        transitive_files: Option<Value<'v>>,
        #[starlark(default = false)] collect_data: bool,
        #[starlark(default = false)] collect_default: bool,
        symlinks: Option<Value<'v>>,
        root_symlinks: Option<Value<'v>>,
        #[starlark(require = named, default = false)] skip_conflict_checking: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Runfiles<'v>> {
        // Conflicting runfiles are never an error here, so there is nothing
        // to skip.
        let _ = skip_conflict_checking;
        let heap = eval.heap();
        let direct: Vec<Value<'v>> = actions::files(files, "files")?.into_iter().map(|artifact| heap.alloc(FileValue(artifact))).collect();
        let mut transitive = Vec::new();
        if let Some(depset) = transitive_files.filter(|value| !value.is_none()) {
            if Depset::from_value(depset).is_none() {
                anyhow::bail!("transitive_files must be a depset");
            }
            transitive.push(depset);
        }
        let mut merged = Vec::new();
        if collect_default {
            merged.extend(dep_runfiles(this, Collect::Default, heap));
        }
        if collect_data {
            merged.extend(dep_runfiles(this, Collect::Data, heap));
        }
        let files = heap.alloc(Depset {
            direct,
            transitive,
            order: Order::Default,
        });
        let mut own = Runfiles::new(files);
        own.symlinks = self::symlinks(symlinks, "symlinks")?;
        own.root_symlinks = self::symlinks(root_symlinks, "root_symlinks")?;
        if merged.is_empty() {
            return Ok(own);
        }
        let own = heap.alloc(own);
        let mut all = vec![Runfiles::from_value(own).ok_or_else(|| anyhow::anyhow!("runfiles"))?];
        for runfiles in merged {
            if let Some(runfiles) = Runfiles::from_value(runfiles) {
                all.push(runfiles);
            }
        }
        Ok(Runfiles::merge_all(&all, heap))
    }

    /// `expand_location(input, targets = [], short_paths = False)`.
    fn expand_location<'v>(
        #[starlark(this)] _this: Value<'v>,
        input: &str,
        targets: Option<Value<'v>>,
        #[starlark(default = false)] short_paths: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<String> {
        let state = RuleState::get(eval)?;
        let extra = target_files(targets)?;
        let style = if short_paths { LocationStyle::Root } else { LocationStyle::Exec };
        // Only $(location) functions are expanded; other `$`s are left.
        let escaped = escape_non_locations(input);
        state.expand(&escaped, &extra, style)
    }

    /// `expand_make_variables(attribute_name, command,
    /// additional_substitutions)`.
    fn expand_make_variables<'v>(
        #[starlark(this)] _this: Value<'v>,
        attribute_name: &str,
        command: &str,
        additional_substitutions: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<String> {
        let state = RuleState::get(eval)?;
        let mut substitutions = Vec::new();
        if let Some(dict) = DictRef::from_value(additional_substitutions) {
            for (key, value) in dict.iter() {
                substitutions.push((key.to_str(), crate::args::stringify(value)));
            }
        }
        let mut lookup = |name: &str, argument: Option<&str>| -> anyhow::Result<String> {
            if argument.is_some() {
                anyhow::bail!("{attribute_name}: $({name} ...) can't be expanded here");
            }
            if let Some((_, value)) = substitutions.iter().find(|(key, _)| key == name) {
                return Ok(value.clone());
            }
            match state.make_variable(name) {
                Some(value) => Ok(value.to_string()),
                None => Err(anyhow::anyhow!("{attribute_name}: $({name}) is not defined")),
            }
        };
        crate::expand::expand(command, &mut lookup)
    }

    /// `coverage_instrumented(target = None)`: coverage isn't collected.
    fn coverage_instrumented<'v>(#[starlark(this)] _this: Value<'v>, target: Option<Value<'v>>) -> anyhow::Result<bool> {
        let _ = target;
        Ok(false)
    }

    /// `target_platform_has_constraint(constraintValue)`.
    fn target_platform_has_constraint<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = pos)] constraint: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<bool> {
        let state = RuleState::get(eval)?;
        let Some(info) = bazel_starlark::provider::ProviderInstance::from_value(constraint) else {
            anyhow::bail!("expected a ConstraintValueInfo");
        };
        let Some(label) = info.field("label").and_then(|label| label.downcast_ref::<LabelValue>().map(|label| label.0)) else {
            return Ok(false);
        };
        Ok(state.target_constraints.contains(&label))
    }

    /// `resolve_tools(*, tools = [])`: the tools' files and runfiles as a
    /// depset, and their input manifests (none).
    fn resolve_tools<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let heap = eval.heap();
        let mut direct = Vec::new();
        for item in tools.and_then(bazel_starlark::convert::items).unwrap_or_default() {
            let Some(target) = TargetValue::from_value(item) else {
                continue;
            };
            direct.extend(actions::files(target.default_info("files"), "files")?.into_iter().map(|artifact| heap.alloc(FileValue(artifact))));
        }
        let inputs = heap.alloc(Depset {
            direct,
            transitive: Vec::new(),
            order: Order::Default,
        });
        let manifests = heap.alloc(AllocList::EMPTY);
        Ok(heap.alloc(AllocTuple([inputs, manifests])))
    }
}

/// Escape every `$` in `text` that doesn't start a location function, so
/// only those are expanded.
fn escape_non_locations(text: &str) -> String {
    const FUNCTIONS: &[&str] = &["location", "locations", "execpath", "execpaths", "rootpath", "rootpaths", "rlocationpath", "rlocationpaths"];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('$') {
        out.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        let is_location = after
            .strip_prefix('(')
            .is_some_and(|inner| FUNCTIONS.iter().any(|function| inner.starts_with(&format!("{function} "))));
        out.push_str(if is_location { "$" } else { "$$" });
        rest = after;
    }
    out.push_str(rest);
    out
}
