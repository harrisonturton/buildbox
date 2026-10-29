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

    /// `tokenize(option)`: split a string into words as a shell would.
    fn tokenize<'v>(#[starlark(this)] _this: Value<'v>, option: &str) -> anyhow::Result<Vec<String>> {
        crate::shell::tokenize(option).map_err(|error| anyhow::anyhow!("{error} while tokenizing '{option}'"))
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
        expand_make_variables(state, attribute_name, command, &string_pairs(additional_substitutions))
    }

    /// `resolve_command(*, command = '', attribute = None,
    /// expand_locations = False, make_variables = None, tools = [],
    /// label_dict = {}, execution_requirements = {})`: the tools' files,
    /// and the command line that runs `command` with Bash, as Bazel's
    /// `CommandHelper` makes it.
    fn resolve_command<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = named, default = "")] command: &str,
        #[starlark(require = named)] attribute: Option<Value<'v>>,
        #[starlark(require = named, default = false)] expand_locations: bool,
        #[starlark(require = named)] make_variables: Option<Value<'v>>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
        #[starlark(require = named)] label_dict: Option<Value<'v>>,
        #[starlark(require = named)] execution_requirements: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = execution_requirements;
        let state = RuleState::get(eval)?;
        let heap = eval.heap();
        let attribute = attribute.and_then(|value| value.unpack_str()).unwrap_or_default();
        let (mut inputs, mut locations) = resolved_tools(state, tools)?;
        locations.extend(label_files(label_dict)?);
        let mut command = command.to_string();
        if expand_locations {
            command = state.expand(&escape_non_locations(&command), &locations, LocationStyle::Exec)?;
        }
        if let Some(variables) = make_variables.filter(|value| !value.is_none()) {
            command = expand_make_variables(state, attribute, &command, &string_pairs(variables))?;
        }
        let (argv, script) = state.bash_command(&command, &state.next_resolve_command_script())?;
        inputs.extend(script);
        let inputs = heap.alloc(AllocList(inputs.into_iter().map(|artifact| heap.alloc(FileValue(artifact)))));
        let argv = heap.alloc(AllocList(argv.iter().map(|argument| heap.alloc(argument.as_str()))));
        Ok(heap.alloc(AllocTuple([inputs, argv, heap.alloc(AllocList::EMPTY)])))
    }

    /// `coverage_instrumented(target = None)`: coverage isn't collected.
    fn coverage_instrumented<'v>(#[starlark(this)] _this: Value<'v>, target: Option<Value<'v>>) -> anyhow::Result<bool> {
        let _ = target;
        Ok(false)
    }

    /// `target_platform_has_constraint(constraintValue)`: whether the
    /// target platform has the value, or has no value for its setting and
    /// it's the setting's default.
    fn target_platform_has_constraint<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = pos)] constraint: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<bool> {
        let state = RuleState::get(eval)?;
        let Some(info) = bazel_starlark::provider::ProviderInstance::from_value(constraint) else {
            anyhow::bail!("expected a ConstraintValueInfo");
        };
        let label_of = |value: Option<Value<'v>>| value.and_then(|label| label.downcast_ref::<LabelValue>().map(|label| label.0));
        let Some(label) = label_of(info.field("label")) else {
            return Ok(false);
        };
        let setting = info.field("constraint").and_then(bazel_starlark::provider::ProviderInstance::from_value);
        let Some(setting) = setting else {
            return Ok(state.target_constraints.iter().any(|(_, value)| *value == label));
        };
        let setting_label = label_of(setting.field("label"));
        if let Some((_, value)) = state.target_constraints.iter().find(|(candidate, _)| Some(*candidate) == setting_label) {
            return Ok(*value == label);
        }
        let default = setting.field("default_constraint_value").and_then(bazel_starlark::provider::ProviderInstance::from_value);
        Ok(label_of(default.and_then(|default| default.field("label"))) == Some(label))
    }

    /// `resolve_tools(*, tools = [])`, which Bazel 9 disallows, as
    /// `--incompatible_disallow_ctx_resolve_tools` does by default.
    fn resolve_tools<'v>(#[starlark(this)] _this: Value<'v>, #[starlark(require = named)] tools: Option<Value<'v>>) -> anyhow::Result<Value<'v>> {
        let _ = tools;
        anyhow::bail!(
            "Pass an executable or tools argument to ctx.actions.run or ctx.actions.run_shell instead of calling ctx.resolve_tools.\nUse --noincompatible_disallow_ctx_resolve_tools to temporarily disable this check."
        )
    }
}

/// Escape every `$` in `text` that doesn't start a location function, so
/// only those are expanded.
/// The files of `tools`, with their executables, which actions taking
/// them as inputs run with their runfiles, and what `$(location)` expands
/// each tool's label to.
fn resolved_tools(state: &RuleState, tools: Option<Value<'_>>) -> anyhow::Result<(Vec<Artifact>, Vec<(Label, Vec<Artifact>)>)> {
    let mut inputs = Vec::new();
    let mut locations = Vec::new();
    for item in tools.and_then(bazel_starlark::convert::items).unwrap_or_default() {
        let Some(target) = TargetValue::from_value(item) else {
            continue;
        };
        let files = actions::files(target.default_info("files"), "files")?;
        inputs.extend(files.iter().copied());
        let executable = target.default_info("executable").and_then(FileValue::artifact);
        match executable {
            Some(executable) => {
                state.resolve_tool(executable);
                inputs.push(executable);
                locations.push((target.label, vec![executable]));
            }
            None => locations.push((target.label, files)),
        }
    }
    inputs.dedup();
    Ok((inputs, locations))
}

/// The files of a dict of labels to lists of files.
fn label_files(value: Option<Value<'_>>) -> anyhow::Result<Vec<(Label, Vec<Artifact>)>> {
    let Some(dict) = value.and_then(DictRef::from_value) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (key, files) in dict.iter() {
        let label = key.downcast_ref::<LabelValue>().map(|label| label.0).ok_or_else(|| anyhow::anyhow!("label_dict: keys must be labels"))?;
        out.push((label, actions::files(Some(files), "label_dict")?));
    }
    Ok(out)
}

/// The entries of a dict of strings.
fn string_pairs(value: Value<'_>) -> Vec<(String, String)> {
    let Some(dict) = DictRef::from_value(value) else {
        return Vec::new();
    };
    dict.iter().map(|(key, value)| (key.to_str(), crate::args::stringify(value))).collect()
}

/// Expand make variables in `command`, `substitutions` first.
fn expand_make_variables(state: &RuleState, attribute: &str, command: &str, substitutions: &[(String, String)]) -> anyhow::Result<String> {
    let mut lookup = |name: &str, argument: Option<&str>| -> anyhow::Result<String> {
        if argument.is_some() {
            anyhow::bail!("{attribute}: $({name} ...) can't be expanded here");
        }
        if let Some((_, value)) = substitutions.iter().find(|(key, _)| key == name) {
            return Ok(value.clone());
        }
        match state.make_variable(name) {
            Some(value) => Ok(value.to_string()),
            None => Err(anyhow::anyhow!("{attribute}: $({name}) not defined")),
        }
    };
    crate::expand::expand(command, &mut lookup)
}

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
