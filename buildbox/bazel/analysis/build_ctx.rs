//! Running a rule's implementation: its `ctx`, built from its attributes
//! and analyzed dependencies, and what it returns.

use crate::action::Action;
use crate::actions::{self, Actions};
use crate::artifact::{self, Artifact};
use crate::configured::{ConfiguredTarget, Implementation};
use crate::file_value::{FileRoot, FileValue};
use crate::providers::{self, Defaults};
use crate::rule_ctx::RuleCtx;
use crate::runfiles::{FilesToRun, Runfiles};
use crate::state::RuleState;
use crate::target_value::TargetValue;
use crate::toolchains::{ResolvedToolchains, ToolchainContext};
use bazel_config::{BuildConfiguration, ConfigId};
use bazel_label::{Label, RepoMappingTable, RepoName};
use bazel_packages::{AttrSchema, AttrType, AttrValue, Cfg, RuleClass};
use bazel_starlark::label_value::LabelValue;
use bazel_starlark::opaque::Opaque;
use bazel_starlark::{EvalContext, FileKind};
use intern::Symbol;
use skycodec::Fingerprinter;
use starlark::PrintHandler;
use starlark::collections::SmallMap;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::dict::AllocDict;
use starlark::values::list::AllocList;
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// An analyzed dependency.
pub enum Dep {
    /// A source file, or a file another rule generates.
    File(Label, Artifact),
    Target(Arc<ConfiguredTarget>),
}

impl Dep {
    pub fn fingerprint(&self, fingerprinter: &mut Fingerprinter) {
        match self {
            Dep::File(label, artifact) => fingerprinter.bytes(label.to_string().as_bytes()).bytes(artifact.exec_path.to_string().as_bytes()),
            Dep::Target(target) => fingerprinter.bytes(target.fingerprint.as_bytes()),
        };
    }
}

/// Everything a rule's analysis needs, computed beforehand.
pub struct RuleInput {
    pub label: Label,
    pub config: Arc<BuildConfiguration>,
    pub class: Arc<RuleClass>,
    /// Every attribute's value, in the class's order.
    pub attrs: Vec<(Symbol, AttrValue)>,
    pub deps: BTreeMap<(Label, ConfigId), Dep>,
    pub build_file: Label,
    pub implementation: Implementation,
    pub mapping: Arc<RepoMappingTable>,
    pub mappings: HashMap<RepoName, Arc<RepoMappingTable>>,
    pub target_constraints: Vec<Label>,
    pub toolchains: ResolvedToolchains,
}

impl RuleInput {
    pub fn attr(&self, name: &str) -> Option<&AttrValue> {
        let name = Symbol::intern(name);
        self.attrs.iter().find(|(attr, _)| *attr == name).map(|(_, value)| value)
    }

    pub fn schema(&self, name: Symbol) -> Option<&AttrSchema> {
        self.class.attr(name)
    }

    pub fn string(&self, name: &str) -> &str {
        match self.attr(name) {
            Some(AttrValue::String(value)) => value,
            _ => "",
        }
    }

    pub fn bool(&self, name: &str) -> bool {
        matches!(self.attr(name), Some(AttrValue::Bool(true)))
    }
}

/// The files a rule declares before its implementation runs.
pub struct Predeclared {
    /// The outputs of each output attribute.
    pub by_attr: Vec<(Symbol, Vec<Artifact>)>,
    /// The outputs the rule class names with templates, by name.
    pub implicit: Vec<(String, Artifact)>,
    /// `ctx.outputs.executable`, for executable rules.
    pub executable: Option<Artifact>,
}

impl Predeclared {
    pub fn all(&self) -> Vec<Artifact> {
        let mut all: Vec<Artifact> = self.by_attr.iter().flat_map(|(_, outputs)| outputs.iter().copied()).collect();
        all.extend(self.implicit.iter().map(|(_, artifact)| *artifact));
        all
    }

    pub fn attr(&self, name: &str) -> Vec<Artifact> {
        let name = Symbol::intern(name);
        self.by_attr.iter().find(|(attr, _)| *attr == name).map(|(_, outputs)| outputs.clone()).unwrap_or_default()
    }
}

/// A rule's dependencies and predeclared outputs, as values of the heap
/// its implementation runs in.
pub struct Prepared<'v> {
    pub deps: HashMap<(Label, ConfigId), Value<'v>>,
    pub outputs: Predeclared,
}

impl<'v> Prepared<'v> {
    /// The targets an attribute names, in order.
    pub fn targets(&self, input: &RuleInput, attr: &str) -> Vec<Value<'v>> {
        let Some(schema) = input.schema(Symbol::intern(attr)) else {
            return Vec::new();
        };
        let Some(value) = input.attr(attr) else {
            return Vec::new();
        };
        let config = dep_config(schema.cfg, input.config.id);
        let mut labels = Vec::new();
        value.labels(&mut labels);
        labels.iter().filter_map(|label| self.deps.get(&(*label, config)).copied()).collect()
    }

    /// The files of the targets an attribute names.
    pub fn files(&self, input: &RuleInput, attr: &str) -> anyhow::Result<Vec<Artifact>> {
        let mut files = Vec::new();
        for target in self.targets(input, attr) {
            files.extend(target_files(target)?);
        }
        Ok(files)
    }
}

fn dep_config(cfg: Cfg, config: ConfigId) -> ConfigId {
    match cfg {
        Cfg::Exec => config.get().exec(),
        Cfg::Target | Cfg::Transition => config,
    }
}

/// The default files of a target.
pub fn target_files(target: Value<'_>) -> anyhow::Result<Vec<Artifact>> {
    let Some(target) = TargetValue::from_value(target) else {
        return Ok(Vec::new());
    };
    actions::files(target.default_info("files"), "files")
}

/// The executable of a target, and its runfiles, if it has one.
pub fn target_executable<'v>(target: Value<'v>) -> Option<(Artifact, &'v Runfiles<'v>)> {
    let target = TargetValue::from_value(target)?;
    let files_to_run = FilesToRun::from_value(target.default_info("files_to_run")?)?;
    let executable = FileValue::artifact(files_to_run.executable)?;
    Some((executable, Runfiles::from_value(files_to_run.runfiles)?))
}

struct Printer;

impl PrintHandler for Printer {
    fn println(&self, text: &str) -> starlark::Result<()> {
        tracing::info!("DEBUG: {text}");
        Ok(())
    }
}

/// Run the rule's implementation, leaving its `Target` in `module` and
/// returning its actions.
pub fn run<'v>(module: &Module<'v>, input: &RuleInput) -> anyhow::Result<Vec<Action>> {
    let heap = module.heap();
    let mut state = RuleState::new(input.label, input.config.clone(), input.mapping.clone());
    state.target_constraints = input.target_constraints.clone();
    let prepared = Prepared {
        deps: dep_values(module, input)?,
        outputs: predeclare(input, &state)?,
    };
    state.locations = locations(input, &prepared)?;
    let fields = ctx_fields(input, &state, &prepared, heap)?;
    let ctx = heap.alloc(RuleCtx {
        label: input.label,
        fields,
    });

    let file = match &input.implementation {
        Implementation::Starlark { module, .. } => module.label,
        Implementation::Native(_) => input.build_file,
    };
    let mut context = EvalContext::new(FileKind::Bzl, file);
    context.mappings = input.mappings.clone();
    context.extension = Some(Box::new(state));
    let returned = {
        let mut eval = Evaluator::new(module);
        eval.extra = Some(&context);
        eval.set_print_handler(&Printer);
        match &input.implementation {
            Implementation::Starlark { implementation, .. } => {
                let implementation = heap.access_owned_frozen_value(implementation);
                eval.eval_function(implementation, &[ctx], &[]).map_err(|error| anyhow::anyhow!("{error}"))?
            }
            Implementation::Native(kind) => crate::native::analyze(*kind, input, &prepared, &mut eval)?,
        }
    };
    let state = context.extension.take().and_then(|state| state.downcast::<RuleState>().ok()).ok_or_else(|| anyhow::anyhow!("lost the rule's state"))?;
    let defaults = defaults(&state, &prepared.outputs);
    check_outputs(&state, &prepared.outputs)?;
    let target = providers::collect(input.label, returned, &defaults, heap)?;
    module.set("target", heap.alloc(target));
    Ok(state.actions.into_inner())
}

/// What `DefaultInfo` defaults to: the predeclared outputs actions
/// generate.
fn defaults(state: &RuleState, outputs: &Predeclared) -> Defaults {
    let declared = state.declared.borrow();
    let generated = |artifact: &Artifact| declared.iter().any(|(declared, generated)| declared == artifact && *generated);
    Defaults {
        files: outputs.all().into_iter().filter(|artifact| generated(artifact)).collect(),
        executable: outputs.executable.filter(|artifact| generated(artifact)),
    }
}

/// Every declared file must have an action that generates it, except the
/// predeclared executable, which a rule may leave unused.
fn check_outputs(state: &RuleState, outputs: &Predeclared) -> anyhow::Result<()> {
    let declared = state.declared.borrow();
    let missing: Vec<String> = declared
        .iter()
        .filter(|(artifact, generated)| !generated && Some(*artifact) != outputs.executable)
        .map(|(artifact, _)| artifact.short_path())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    anyhow::bail!("the following files have no generating action:\n{}", missing.join("\n"))
}

/// Each dependency as a `Target` value of `module`'s heap.
fn dep_values<'v>(module: &Module<'v>, input: &RuleInput) -> anyhow::Result<HashMap<(Label, ConfigId), Value<'v>>> {
    let heap = module.heap();
    let mut values = HashMap::with_capacity(input.deps.len());
    for (key, dep) in &input.deps {
        let value = match dep {
            Dep::File(label, artifact) => heap.alloc(providers::file_target(*label, *artifact, heap)?),
            Dep::Target(target) => heap.access_owned_frozen_value(&target.value),
        };
        values.insert(*key, value);
    }
    Ok(values)
}

/// Declare the outputs the rule's attributes and class name.
fn predeclare(input: &RuleInput, state: &RuleState) -> anyhow::Result<Predeclared> {
    let mut by_attr = Vec::new();
    for (schema, (_, value)) in input.class.attrs.iter().zip(&input.attrs) {
        if !schema.ty.is_output() {
            continue;
        }
        let mut labels = Vec::new();
        value.labels(&mut labels);
        let outputs: anyhow::Result<Vec<Artifact>> = labels.iter().map(|label| state.declare(label.name.as_str(), false)).collect();
        by_attr.push((schema.name, outputs?));
    }
    let mut implicit = Vec::new();
    for (name, template) in &input.class.outputs {
        let file = substitute_template(template, input);
        implicit.push((name.to_string(), state.declare(&file, false)?));
    }
    let executable = match input.class.executable || input.class.test {
        true => Some(state.declare(input.label.name.as_str(), false)?),
        false => None,
    };
    Ok(Predeclared {
        by_attr,
        implicit,
        executable,
    })
}

/// An implicit output's name: its template with `%{name}` and other
/// attributes substituted.
fn substitute_template(template: &str, input: &RuleInput) -> String {
    let mut out = template.replace("%{name}", input.label.name.as_str());
    for (name, value) in &input.attrs {
        let pattern = format!("%{{{name}}}");
        if !out.contains(&pattern) {
            continue;
        }
        let value = match value {
            AttrValue::String(value) => value.clone(),
            AttrValue::Label(label) => label.name.to_string(),
            _ => String::new(),
        };
        out = out.replace(&pattern, &value);
    }
    out
}

/// What `$(location)` expands each label to: an executable target's
/// executable, or a target's files.
fn locations(input: &RuleInput, prepared: &Prepared<'_>) -> anyhow::Result<Vec<(Label, Vec<Artifact>)>> {
    let mut locations = Vec::new();
    for ((label, _), value) in &prepared.deps {
        let files = match target_executable(*value) {
            Some((executable, _)) => vec![executable],
            None => target_files(*value)?,
        };
        locations.push((*label, files));
    }
    for (schema, (_, value)) in input.class.attrs.iter().zip(&input.attrs) {
        if !schema.ty.is_output() {
            continue;
        }
        let mut labels = Vec::new();
        value.labels(&mut labels);
        let outputs = prepared.outputs.attr(schema.name.as_str());
        locations.extend(labels.into_iter().zip(outputs).map(|(label, artifact)| (label, vec![artifact])));
    }
    Ok(locations)
}

/// The fields of `ctx`.
fn ctx_fields<'v>(input: &RuleInput, state: &RuleState, prepared: &Prepared<'v>, heap: Heap<'v>) -> anyhow::Result<SmallMap<String, Value<'v>>> {
    let config = &input.config;
    let bin_dir = heap.alloc(FileRoot(config.bin_dir()));
    let mut fields = SmallMap::new();
    let mut set = |name: &str, value: Value<'v>| {
        fields.insert(name.to_string(), value);
    };
    set("attr", attr_struct(input, prepared, heap));
    set("file", file_struct(input, prepared, heap)?);
    set("files", files_struct(input, prepared, heap)?);
    set("executable", executable_struct(input, state, prepared, heap));
    set("outputs", outputs_struct(&prepared.outputs, input, heap));
    set("actions", heap.alloc(Actions));
    set("label", heap.alloc(LabelValue(input.label)));
    set("bin_dir", bin_dir);
    set("genfiles_dir", bin_dir);
    set("workspace_name", heap.alloc("_main"));
    let build_file_path = artifact::package_dir(input.label.package).join(input.build_file.name);
    set("build_file_path", heap.alloc(build_file_path.to_string()));
    set("var", heap.alloc(AllocDict(state.make_variables.iter().map(|(name, value)| (name.as_str(), value.as_str())))));
    let options = crate::fragments::Options {
        compilation_mode: config.options.compilation_mode.clone(),
        action_env: config.options.action_env.clone(),
        values: config.options.values.clone(),
        is_exec: config.options.is_exec,
    };
    set("configuration", heap.alloc(crate::fragments::Configuration(options.clone())));
    set("features", heap.alloc(AllocList::EMPTY));
    set("disabled_features", heap.alloc(AllocList::EMPTY));
    set("toolchains", heap.alloc(ToolchainContext::new(&input.toolchains, heap)));
    set("fragments", heap.alloc(crate::fragments::Fragments(options)));
    set("exec_groups", heap.alloc(Opaque::new("ctx.exec_groups")));
    set("split_attr", heap.alloc(AllocStruct(Vec::<(&str, Value<'v>)>::new())));
    set("aspect_ids", heap.alloc(AllocList::EMPTY));
    set("build_setting_value", build_setting_value(input, heap));
    Ok(fields)
}

/// The value of a build setting: from the command line, or its default.
fn build_setting_value<'v>(input: &RuleInput, heap: Heap<'v>) -> Value<'v> {
    let Some(default) = input.attr("build_setting_default") else {
        return Value::new_none();
    };
    let given = input.config.options.starlark_flags.iter().find(|(label, _)| *label == input.label).map(|(_, value)| value);
    let Some(given) = given else {
        return bazel_starlark::convert::to_starlark(default, heap);
    };
    match default {
        AttrValue::Bool(_) => Value::new_bool(matches!(given.as_str(), "true" | "True" | "1" | "yes")),
        AttrValue::Int(_) => heap.alloc(given.parse::<i32>().unwrap_or(0)),
        AttrValue::StringList(_) => heap.alloc(AllocList(given.split(',').filter(|value| !value.is_empty()))),
        _ => heap.alloc(given.as_str()),
    }
}

/// The attributes as `ctx.attr` has them: labels as targets, outputs as
/// files.
fn attr_struct<'v>(input: &RuleInput, prepared: &Prepared<'v>, heap: Heap<'v>) -> Value<'v> {
    let mut fields: Vec<(&str, Value<'v>)> = Vec::new();
    for (schema, (name, value)) in input.class.attrs.iter().zip(&input.attrs) {
        let name = name.as_str();
        if name.starts_with('$') || name.starts_with(':') {
            continue;
        }
        let value = attr_value(input, prepared, schema, value, heap);
        fields.push((name, value));
    }
    heap.alloc(AllocStruct(fields))
}

/// Late-bound attributes for Apple platforms, which rules read
/// unconditionally but only use when building for them. They stand in for
/// targets bb doesn't analyze.
const APPLE_LATE_BOUND_ATTRS: &[&str] = &["_xcode_config"];

fn attr_value<'v>(input: &RuleInput, prepared: &Prepared<'v>, schema: &AttrSchema, value: &AttrValue, heap: Heap<'v>) -> Value<'v> {
    if APPLE_LATE_BOUND_ATTRS.contains(&schema.name.as_str()) && matches!(value, AttrValue::None) {
        return heap.alloc(Opaque::new(schema.name.as_str()));
    }
    let config = dep_config(schema.cfg, input.config.id);
    let target = |label: &Label| prepared.deps.get(&(*label, config)).copied().unwrap_or_else(Value::new_none);
    match (schema.ty, value) {
        (AttrType::Output | AttrType::OutputList, _) => output_value(schema, &prepared.outputs, heap),
        (_, AttrValue::Label(label)) if schema.is_dependency => target(label),
        (_, AttrValue::LabelList(labels)) if schema.is_dependency => heap.alloc(AllocList(labels.iter().map(target))),
        (_, AttrValue::LabelKeyedStringDict(entries)) if schema.is_dependency => {
            heap.alloc(AllocDict(entries.iter().map(|(label, value)| (target(label), value.as_str()))))
        }
        (_, AttrValue::StringKeyedLabelDict(entries)) if schema.is_dependency => {
            heap.alloc(AllocDict(entries.iter().map(|(key, label)| (key.as_str(), target(label)))))
        }
        _ => bazel_starlark::convert::to_starlark(value, heap),
    }
}

fn output_value<'v>(schema: &AttrSchema, outputs: &Predeclared, heap: Heap<'v>) -> Value<'v> {
    let outputs = outputs.attr(schema.name.as_str());
    match schema.ty {
        AttrType::Output => outputs.first().map(|artifact| heap.alloc(FileValue(*artifact))).unwrap_or_else(Value::new_none),
        _ => heap.alloc(AllocList(outputs.iter().map(|artifact| FileValue(*artifact)))),
    }
}

/// The label attributes, with each one's schema and targets.
fn label_attrs<'a, 'v>(input: &'a RuleInput, prepared: &'a Prepared<'v>) -> impl Iterator<Item = (&'a AttrSchema, Vec<Value<'v>>)> + 'a {
    input.class.attrs.iter().filter(|schema| schema.is_dependency && schema.ty.has_labels() && !schema.ty.is_output()).filter_map(|schema| {
        let name = schema.name.as_str();
        if name.starts_with('$') || name.starts_with(':') {
            return None;
        }
        Some((schema, prepared.targets(input, name)))
    })
}

/// `ctx.file`: for label attributes whose target has exactly one file.
fn file_struct<'v>(input: &RuleInput, prepared: &Prepared<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let mut fields: Vec<(&str, Value<'v>)> = Vec::new();
    for (schema, targets) in label_attrs(input, prepared) {
        if schema.ty != AttrType::Label {
            continue;
        }
        let files = match targets.first() {
            Some(target) => target_files(*target)?,
            None => Vec::new(),
        };
        let value = match files.as_slice() {
            [file] => heap.alloc(FileValue(*file)),
            _ => Value::new_none(),
        };
        fields.push((schema.name.as_str(), value));
    }
    Ok(heap.alloc(AllocStruct(fields)))
}

/// `ctx.files`: every file of each label attribute's targets.
fn files_struct<'v>(input: &RuleInput, prepared: &Prepared<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let mut fields: Vec<(&str, Value<'v>)> = Vec::new();
    for (schema, targets) in label_attrs(input, prepared) {
        let mut files = Vec::new();
        for target in targets {
            files.extend(target_files(target)?);
        }
        fields.push((schema.name.as_str(), heap.alloc(AllocList(files.into_iter().map(FileValue)))));
    }
    Ok(heap.alloc(AllocStruct(fields)))
}

/// `ctx.executable`: the executables of executable attributes' targets,
/// whose runfiles actions that run them get.
fn executable_struct<'v>(input: &RuleInput, state: &RuleState, prepared: &Prepared<'v>, heap: Heap<'v>) -> Value<'v> {
    let mut fields: Vec<(&str, Value<'v>)> = Vec::new();
    for (schema, targets) in label_attrs(input, prepared) {
        for target in &targets {
            if let Some((executable, runfiles)) = target_executable(*target) {
                state.add_tool(executable, runfiles);
            }
        }
        if !schema.executable || schema.ty != AttrType::Label {
            continue;
        }
        let executable = targets.first().and_then(|target| target_executable(*target));
        let value = executable.map(|(artifact, _)| heap.alloc(FileValue(artifact))).unwrap_or_else(Value::new_none);
        fields.push((schema.name.as_str(), value));
    }
    heap.alloc(AllocStruct(fields))
}

/// `ctx.outputs`: the predeclared outputs.
fn outputs_struct<'v>(outputs: &Predeclared, input: &RuleInput, heap: Heap<'v>) -> Value<'v> {
    let mut fields: Vec<(String, Value<'v>)> = Vec::new();
    for (schema, _) in input.class.attrs.iter().zip(&input.attrs) {
        if schema.ty.is_output() {
            fields.push((schema.name.to_string(), output_value(schema, outputs, heap)));
        }
    }
    for (name, artifact) in &outputs.implicit {
        fields.push((name.clone(), heap.alloc(FileValue(*artifact))));
    }
    if let Some(executable) = outputs.executable {
        fields.push(("executable".to_string(), heap.alloc(FileValue(executable))));
    }
    heap.alloc(AllocStruct(fields))
}
