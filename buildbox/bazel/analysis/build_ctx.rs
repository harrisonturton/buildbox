//! Running a rule's implementation: its `ctx`, built from its attributes
//! and analyzed dependencies, and what it returns.

use crate::action::{Action, ActionKind};
use crate::inputs::InputSet;
use crate::actions::{self, Actions};
use crate::artifact::{self, Artifact};
use crate::action_value::DepTargets;
use crate::configured::{ConfiguredTarget, Implementation};
use crate::expand::LocationStyle;
use crate::exec_groups::ExecGroups;
use crate::failure::{AttributeErrors, ImplementationFailed};
use crate::file_value::{FileRoot, FileValue};
use crate::providers::{self, Defaults};
use crate::rule_ctx::RuleCtx;
use crate::runfiles::{FilesToRun, Runfiles};
use crate::state::RuleState;
use crate::target_value::TargetValue;
use crate::toolchains::{ResolvedToolchains, ToolchainAspectsProviders, ToolchainContext};
use crate::transition::AttrConfigs;
use bazel_config::{BuildConfiguration, ConfigId};
use bazel_label::{Label, PackageId, RepoMappingTable, RepoName};
use bazel_packages::{AttrSchema, AttrType, AttrValue, Cfg, RuleClass};
use bazel_starlark::attr_module::Attribute;
use bazel_starlark::label_value::LabelValue;
use bazel_starlark::opaque::{Analyzing, Opaque};
use bazel_starlark::{EvalContext, FileKind};
use intern::Symbol;
use skycodec::Fingerprinter;
use bazel_starlark::print::Printed;
use bazel_starlark::provider::ProviderInstance;
use starlark::collections::SmallMap;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::values::dict::{AllocDict, DictRef};
use starlark::values::list::AllocList;
use starlark::values::structs::AllocStruct;
use starlark::values::{Heap, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

/// An analyzed dependency.
pub enum Dep {
    /// A source file, or a file another rule generates.
    File(Label, Artifact),
    Target(Arc<ConfiguredTarget>),
    /// A target with aspects applied: the target, then each aspect's
    /// application to it, whose providers dependents see merged.
    Aspect(Vec<Arc<ConfiguredTarget>>),
}

impl Dep {
    pub fn fingerprint(&self, fingerprinter: &mut Fingerprinter) {
        match self {
            Dep::File(label, artifact) => fingerprinter.bytes(label.to_string().as_bytes()).bytes(artifact.exec_path.to_string().as_bytes()),
            Dep::Target(target) => fingerprinter.bytes(target.fingerprint.as_bytes()),
            Dep::Aspect(applied) => {
                for target in applied {
                    fingerprinter.bytes(target.fingerprint.as_bytes());
                }
                fingerprinter
            }
        };
    }
}

/// What an aspect's analysis has beside its own attributes: the target
/// it's applied to.
pub struct AspectView {
    /// The target's rule, with its attributes, as `ctx.rule` has them. Its
    /// dependencies are among the aspect's, with the aspect applied along
    /// the attributes it propagates along.
    pub rule: Box<RuleInput>,
    /// The target, then the aspects applied to it before this one, whose
    /// providers the implementation's `target` has.
    pub targets: Vec<Arc<ConfiguredTarget>>,
    /// The target's toolchains, as the aspect sees them.
    pub toolchains: Vec<AspectToolchain>,
}

/// A toolchain of the target an aspect is applied to: its type, as the
/// target's rule asks for it, and if one was found, its target's label
/// with the applications to it of the aspects that propagate to it.
pub struct AspectToolchain {
    pub toolchain_type: Label,
    pub target: Option<(Label, Vec<Arc<ConfiguredTarget>>)>,
}

/// Everything a rule's analysis needs, computed beforehand.
pub struct RuleInput {
    pub label: Label,
    pub config: Arc<BuildConfiguration>,
    pub class: Arc<RuleClass>,
    /// Every attribute's value, in the class's order.
    pub attrs: Vec<(Symbol, AttrValue)>,
    /// The configurations transitions give the dependencies through
    /// attributes.
    pub attr_configs: AttrConfigs,
    /// The configuration of the rule's dependencies: its own, less the
    /// execution platform a toolchain target is analyzed for.
    pub deps_config: ConfigId,
    /// The configuration of the rule's tools.
    pub exec_config: ConfigId,
    pub deps: BTreeMap<(Label, ConfigId), Dep>,
    pub build_file: Label,
    pub implementation: Implementation,
    pub mapping: Arc<RepoMappingTable>,
    /// How the main repository sees the others.
    pub main_mapping: Arc<RepoMappingTable>,
    pub mappings: HashMap<RepoName, Arc<RepoMappingTable>>,
    /// The target platform's constraint values, with their settings.
    pub target_constraints: Vec<(Label, Label)>,
    /// The target platform's CPU, as `$(TARGET_CPU)` names it.
    pub target_cpu: String,
    /// For an aspect: the target it's applied to.
    pub aspect: Option<AspectView>,
    pub toolchains: ResolvedToolchains,
    /// A rule set's Starlark function a native rule's implementation calls:
    /// for `java_plugins_flag_alias`, rules_java's merge of plugins.
    pub helper: Option<starlark::values::OwnedFrozenValue>,
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

    /// The configurations of the dependencies through an attribute, keyed
    /// as a split transition names them.
    pub fn dep_configs(&self, schema: &AttrSchema) -> Vec<(String, ConfigId)> {
        crate::transition::dep_configs(schema.cfg, schema.name, self.deps_config, self.exec_config, &self.attr_configs)
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
        let mut labels = Vec::new();
        value.labels(&mut labels);
        let mut targets = Vec::new();
        for (_, config) in input.dep_configs(schema) {
            targets.extend(labels.iter().filter_map(|label| self.deps.get(&(*label, config)).copied()));
        }
        targets
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

/// The default files of a target.
pub fn target_files(target: Value<'_>) -> anyhow::Result<Vec<Artifact>> {
    let Some(target) = TargetValue::from_value(target) else {
        return Ok(Vec::new());
    };
    actions::files(target.default_info("files"), "files")
}

/// The executable of an executable target, and its runfiles. A file's
/// target isn't one, though it runs as itself.
pub fn target_executable<'v>(target: Value<'v>) -> Option<(Artifact, &'v Runfiles<'v>)> {
    let target = TargetValue::from_value(target)?;
    target.default_info("executable").filter(|executable| !executable.is_none())?;
    let files_to_run = FilesToRun::from_value(target.default_info("files_to_run")?)?;
    let executable = FileValue::artifact(files_to_run.executable)?;
    Some((executable, Runfiles::from_value(files_to_run.runfiles)?))
}

/// Call the rule's implementation with `ctx`, and an aspect's with its
/// `target` too, returning what it returns.
fn implement<'v>(input: &RuleInput, prepared: &Prepared<'v>, ctx: Value<'v>, target: Option<Value<'v>>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
    match &input.implementation {
        Implementation::Starlark { implementation, .. } | Implementation::Builtin { implementation, .. } => {
            let implementation = eval.heap().access_owned_frozen_value(implementation);
            let arguments = match target {
                Some(target) => vec![target, ctx],
                None => vec![ctx],
            };
            eval.eval_function(implementation, &arguments, &[]).map_err(|error| ImplementationFailed::starlark(&error).into())
        }
        Implementation::Native(kind) => crate::native::analyze(*kind, input, prepared, eval),
    }
}

/// Where the rule's `ctx` is kept in its module while its implementation
/// runs, for native functions given only `ctx.actions` and for subrules: a
/// name Starlark can't refer to.
const RULE_CTX: &str = bazel_starlark::subrule::RULE_CTX;

/// The `ctx` of the rule `eval` is analyzing.
pub fn rule_ctx<'v>(eval: &Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
    eval.module().get(RULE_CTX).filter(|ctx| !ctx.is_none()).ok_or_else(|| anyhow::anyhow!("only rule implementations can do this"))
}

/// Run the rule's implementation, leaving its `Target` in `module` and
/// returning its actions. What it printed goes in `printed`, even if it
/// fails.
/// What running a rule's implementation gave besides its providers.
pub struct Ran {
    pub actions: Vec<Action>,
    /// An executable rule's `args`, expanded and split into words.
    pub args: Vec<String>,
}

pub fn run<'v>(module: &Module<'v>, input: &RuleInput, printed: &mut Vec<Printed>) -> anyhow::Result<Ran> {
    let heap = module.heap();
    let mut state = RuleState::new(input.label, input.class.kind, input.config.clone(), input.mapping.clone(), input.main_mapping.clone());
    state.target_constraints = input.target_constraints.clone();
    state.analysis_test = input.class.analysis_test && input.aspect.is_none();
    state.toolchain_types = input.class.toolchain_types.clone();
    if let Some(AttrValue::StringList(tags)) = input.attr("tags") {
        state.tag_execution_info = crate::state::tag_execution_info(tags);
    }
    let prepared = Prepared {
        deps: dep_values(module, input)?,
        outputs: predeclare(input, &state)?,
    };
    state.make_variables = crate::state::make_variables(&input.config, &input.target_cpu, template_variables(input, &prepared));
    state.locations = locations(input, &prepared)?;
    let fields = ctx_fields(input, &state, &prepared, heap)?;
    let ctx = heap.alloc(RuleCtx {
        label: input.label,
        fields,
    });

    let file = match &input.implementation {
        Implementation::Starlark { module, .. } => module.label,
        Implementation::Builtin { file, .. } => *file,
        Implementation::Native(_) => input.build_file,
    };
    let mut context = EvalContext::new(FileKind::Bzl, file);
    context.mappings = input.mappings.clone();
    context.extension = Some(Box::new(state));
    module.set(RULE_CTX, ctx);
    let aspect_target = match &input.aspect {
        Some(view) => Some(aspect_target(view, heap)?),
        None => None,
    };
    let returned = {
        let _analyzing = Analyzing::begin();
        let _targets = DepTargets::begin(dep_targets(input, &prepared, aspect_target));
        let mut eval = Evaluator::new(module);
        eval.extra = Some(&context);
        implement(input, &prepared, ctx, aspect_target, &mut eval)
    };
    module.set(RULE_CTX, Value::new_none());
    printed.extend(context.printed.take());
    let rule_state = context.extension.as_ref().and_then(|state| state.downcast_ref::<RuleState>());
    let args = rule_state.map(|state| executable_args(input, state)).unwrap_or_default();
    let attribute_errors = rule_state.map(|state| state.attribute_errors.take()).unwrap_or_default();
    if !attribute_errors.is_empty() {
        let implementation = returned.err().map(|error| error.downcast::<ImplementationFailed>().unwrap_or_else(|error| ImplementationFailed::reported(error.to_string())));
        return Err(AttributeErrors {
            errors: attribute_errors,
            implementation,
        }
        .into());
    }
    let returned = returned?;
    let state = context.extension.take().and_then(|state| state.downcast::<RuleState>().ok()).ok_or_else(|| anyhow::anyhow!("lost the rule's state"))?;
    let script = match state.analysis_test {
        true => Some(write_analysis_test_script(&state, returned)?),
        false => None,
    };
    let mut defaults = defaults(&state, &prepared.outputs);
    if script.is_some() {
        defaults.executable = script;
    }
    check_outputs(&state, &prepared.outputs)?;
    let target = match input.aspect {
        Some(_) => providers::collect_aspect(input.label, returned)?,
        None => providers::collect(input.label, returned, &defaults, heap)?,
    };
    module.set("target", heap.alloc(target));
    Ok(Ran {
        actions: state.actions.into_inner(),
        args,
    })
}

/// The arguments an executable rule's `args` gives its executable when
/// run or tested: each expanded, with `$(location)` giving runfiles paths,
/// then split into words, as Bazel's `RunfilesSupport` computes them.
/// Errors are the attribute's, recorded on `state`.
fn executable_args(input: &RuleInput, state: &RuleState) -> Vec<String> {
    if !input.class.executable && !input.class.test {
        return Vec::new();
    }
    let Some(AttrValue::StringList(values)) = input.attr("args") else {
        return Vec::new();
    };
    let mut args = Vec::new();
    for value in values {
        let words = state
            .expand(value, &[], LocationStyle::Root)
            .and_then(|expanded| crate::shell::tokenize(&expanded).map_err(|error| anyhow::anyhow!(error)));
        match words {
            Ok(words) => args.extend(words),
            Err(error) => {
                let error = format!("in args attribute of {} rule {}: {error}", input.class.kind, input.label);
                state.attribute_errors.borrow_mut().push(error);
            }
        }
    }
    args
}

/// Write an analysis test's executable, `<name>.sh`: a script that passes
/// or fails as the `AnalysisTestResultInfo` it returned says, as Bazel
/// writes it.
fn write_analysis_test_script(state: &RuleState, returned: Value<'_>) -> anyhow::Result<Artifact> {
    let result = analysis_test_result(returned).ok_or_else(|| ImplementationFailed::reported("rules with analysis_test=true must return an instance of AnalysisTestResultInfo".to_string()))?;
    let script = state.declare(&format!("{}.sh", state.label.name), false)?;
    let mut content = String::from("#!/bin/sh\n");
    for line in result.message.split('\n') {
        let escaped: String = line.chars().flat_map(|c| ['\\', c]).collect();
        content.push_str(&format!("echo {escaped}\n"));
    }
    content.push_str(if result.success { "exit 0" } else { "exit 1" });
    state.register_own(Action {
        mnemonic: Symbol::intern("FileWrite"),
        progress_message: String::new(),
        inputs: InputSet::default(),
        runfiles: Vec::new(),
        outputs: vec![script],
        kind: ActionKind::Write {
            content,
            executable: true,
            from_args: false,
        },
    })?;
    Ok(script)
}

/// What an analysis test's `AnalysisTestResultInfo` says.
struct AnalysisTestResult {
    success: bool,
    message: String,
}

/// The `AnalysisTestResultInfo` among what an implementation returned.
fn analysis_test_result(returned: Value<'_>) -> Option<AnalysisTestResult> {
    let items = match ProviderInstance::from_value(returned) {
        Some(_) => vec![returned],
        None => bazel_starlark::convert::items(returned)?,
    };
    let info = items.into_iter().filter_map(ProviderInstance::from_value).find(|info| info.provider_key().as_deref() == Some("AnalysisTestResultInfo"))?;
    Some(AnalysisTestResult {
        success: info.field("success").is_some_and(|success| success.to_bool()),
        message: info.field("message").and_then(|message| message.unpack_str()).unwrap_or_default().to_string(),
    })
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

/// Each `Target` value the implementation gets, by address, with the
/// configured targets it stands for: a target, or with aspects applied,
/// their applications and then the target, as Bazel lists their actions.
fn dep_targets(input: &RuleInput, prepared: &Prepared<'_>, aspect_target: Option<Value<'_>>) -> Vec<(usize, Vec<Arc<ConfiguredTarget>>)> {
    let address = |value: Value<'_>| TargetValue::from_value(value).map(|target| target as *const TargetValue as usize);
    let mut targets = Vec::new();
    for (key, dep) in &input.deps {
        let configured = match dep {
            Dep::File(..) => continue,
            Dep::Target(target) => vec![target.clone()],
            Dep::Aspect(applied) => aspects_then_target(applied),
        };
        if let Some(address) = prepared.deps.get(key).and_then(|value| address(*value)) {
            targets.push((address, configured));
        }
    }
    if let (Some(view), Some(address)) = (&input.aspect, aspect_target.and_then(address)) {
        targets.push((address, aspects_then_target(&view.targets)));
    }
    targets
}

/// A target and the applications of aspects to it, aspects first.
fn aspects_then_target(applied: &[Arc<ConfiguredTarget>]) -> Vec<Arc<ConfiguredTarget>> {
    let mut ordered: Vec<Arc<ConfiguredTarget>> = applied.iter().skip(1).cloned().collect();
    ordered.extend(applied.first().cloned());
    ordered
}

/// Each dependency as a `Target` value of `module`'s heap.
fn dep_values<'v>(module: &Module<'v>, input: &RuleInput) -> anyhow::Result<HashMap<(Label, ConfigId), Value<'v>>> {
    let heap = module.heap();
    let mut values = HashMap::with_capacity(input.deps.len());
    for (key, dep) in &input.deps {
        let value = match dep {
            Dep::File(label, artifact) => heap.alloc(providers::file_target(*label, *artifact, heap)?),
            Dep::Target(target) => heap.access_owned_frozen_value(target.providers()?.owned()),
            Dep::Aspect(applied) => merged(applied, heap)?,
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

/// The make variables a rule's toolchains define with
/// `TemplateVariableInfo`: those of the targets in its `toolchains`
/// attribute, then those of its resolved toolchains, the first definition
/// of each winning, as Bazel looks them up.
fn template_variables(input: &RuleInput, prepared: &Prepared<'_>) -> Vec<(String, String)> {
    let mut variables = Vec::new();
    for target in prepared.targets(input, "toolchains") {
        if let Some(info) = TargetValue::from_value(target).and_then(|target| target.provider(TEMPLATE_VARIABLE_INFO)) {
            variables.extend(template_variable_info(info));
        }
    }
    let resolved = input.toolchains.resolved.iter().filter_map(|(_, toolchain)| toolchain.as_ref()?.providers().ok());
    for providers in resolved {
        if let Some(info) = providers.get().and_then(|target| target.provider(TEMPLATE_VARIABLE_INFO)) {
            variables.extend(template_variable_info(info));
        }
    }
    let mut seen = HashSet::new();
    variables.retain(|(name, _)| seen.insert(name.clone()));
    variables
}

/// The provider toolchains define make variables with.
const TEMPLATE_VARIABLE_INFO: &str = "TemplateVariableInfo";

/// The variables of a `TemplateVariableInfo`.
fn template_variable_info(info: Value<'_>) -> Vec<(String, String)> {
    let Some(variables) = ProviderInstance::from_value(info).and_then(|info| info.field("variables")) else {
        return Vec::new();
    };
    let Some(dict) = DictRef::from_value(variables) else {
        return Vec::new();
    };
    dict.iter().filter_map(|(name, value)| Some((name.unpack_str()?.to_string(), value.unpack_str()?.to_string()))).collect()
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
    for (name, value) in subrule_attrs(input, prepared, heap) {
        set(name, value);
    }
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
    let options = config.options.clone();
    set("configuration", heap.alloc(crate::fragments::Configuration(options.clone())));
    set("features", heap.alloc(AllocList::EMPTY));
    set("disabled_features", heap.alloc(AllocList::EMPTY));
    set("info_file", heap.alloc(FileValue(crate::workspace_status::info_file())));
    set("version_file", heap.alloc(FileValue(crate::workspace_status::version_file())));
    set("toolchains", heap.alloc(ToolchainContext::new(&input.toolchains, definition_mapping(input), target_description(input), heap)));
    set("fragments", heap.alloc(crate::fragments::Fragments(options)));
    set("exec_groups", heap.alloc(exec_groups(input)));
    set("split_attr", split_attr_struct(input, prepared, heap));
    set("aspect_ids", heap.alloc(AllocList(input.config.aspects.iter().map(|aspect| aspect.description()))));
    set("build_setting_value", build_setting_value(input, heap));
    if let Some(view) = &input.aspect {
        set("rule", rule_struct(input, view, state, prepared, heap)?);
    }
    Ok(fields)
}

/// `ctx.rule` for an aspect: the attributes of the target's rule, its
/// files and executables, and its kind.
fn rule_struct<'v>(input: &RuleInput, view: &AspectView, state: &RuleState, prepared: &Prepared<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let rule = &view.rule;
    let fields = vec![
        ("attr", attr_struct(rule, prepared, heap)),
        ("file", file_struct(rule, prepared, heap)?),
        ("files", files_struct(rule, prepared, heap)?),
        ("executable", executable_struct(rule, state, prepared, heap)),
        ("kind", heap.alloc(rule.class.kind.as_str())),
        ("toolchains", aspect_toolchains(input, view, heap)?),
    ];
    Ok(heap.alloc(AllocStruct(fields)))
}

/// `ctx.rule.toolchains`: for each toolchain of the target, the providers
/// the aspects that propagate to it gave its target.
fn aspect_toolchains<'v>(input: &RuleInput, view: &AspectView, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let mut types = Vec::with_capacity(view.toolchains.len());
    let mut values = Vec::with_capacity(view.toolchains.len());
    for toolchain in &view.toolchains {
        types.push(toolchain.toolchain_type);
        let Some((label, applied)) = &toolchain.target else {
            values.push(Value::new_none());
            continue;
        };
        let providers = match applied.is_empty() {
            true => heap.alloc(TargetValue {
                label: *label,
                providers: SmallMap::new(),
                is_file: false,
            }),
            false => merged(applied, heap)?,
        };
        values.push(heap.alloc(ToolchainAspectsProviders { label: *label, providers }));
    }
    let mapping = definition_mapping(input);
    Ok(heap.alloc(ToolchainContext {
        types,
        values,
        mapping,
        target: target_description(input),
    }))
}

/// The `target` an aspect's implementation gets: the target, with the
/// providers of the aspects applied to it before.
fn aspect_target<'v>(view: &AspectView, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    merged(&view.targets, heap)
}

/// Configured targets' providers merged, as a target with aspects applied
/// has them.
fn merged<'v>(targets: &[Arc<ConfiguredTarget>], heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
    let mut values = Vec::with_capacity(targets.len());
    for target in targets {
        values.push(heap.access_owned_frozen_value(target.providers()?.owned()));
    }
    crate::target_value::merge(&values, heap)
}

/// What `ctx.exec_groups` holds: the exec groups the rule declares.
fn exec_groups(input: &RuleInput) -> ExecGroups {
    ExecGroups {
        target: target_description(input),
        names: input.class.exec_groups.iter().map(|name| name.to_string()).collect(),
    }
}

/// What a rule's or aspect's analysis is of, as Bazel's errors about its
/// toolchains and exec groups describe it: `<kind> rule //pkg:name`, or
/// `aspect //pkg:defs.bzl%name applied to <kind> rule //pkg:name`.
fn target_description(input: &RuleInput) -> String {
    let (Some(view), Some(aspect)) = (&input.aspect, input.config.aspects.last()) else {
        return format!("{} rule {}", input.class.kind, input.label);
    };
    format!("aspect {} applied to {} rule {}", aspect.description(), view.rule.class.kind, input.label)
}

/// The value of a build setting: from the command line, or its default.
fn build_setting_value<'v>(input: &RuleInput, heap: Heap<'v>) -> Value<'v> {
    let Some(default) = input.attr("build_setting_default") else {
        return Value::new_none();
    };
    if let Some(value) = input.config.options.flag(input.label) {
        return crate::settings::starlark_value(value, heap);
    }
    match input.class.build_setting.and_then(|spec| crate::settings::flag_value(default, spec)) {
        Some(default) => crate::settings::starlark_value(&default, heap),
        None => bazel_starlark::convert::to_starlark(default, heap),
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

/// The attributes the rule has for its subrules, which only they see, as
/// `ctx.attr` would have them.
fn subrule_attrs<'a, 'v>(input: &'a RuleInput, prepared: &Prepared<'v>, heap: Heap<'v>) -> Vec<(&'a str, Value<'v>)> {
    let mut attrs = Vec::new();
    for (schema, (name, value)) in input.class.attrs.iter().zip(&input.attrs) {
        let name = name.as_str();
        if name.starts_with('$') && name.contains('%') {
            attrs.push((name, attr_value(input, prepared, schema, value, heap)));
        }
    }
    attrs
}

/// The value a subrule's implementation gets for one of its attributes,
/// named `name` in the rule whose `ctx` is `ctx`: an executable's files to
/// run, a single file's file, or the targets, as Bazel passes them.
fn subrule_attr_value<'v>(ctx: Value<'v>, name: &str, attribute: &Attribute<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
    let Some(value) = ctx.get_attr(name, eval.heap()).ok().flatten().filter(|value| !value.is_none()) else {
        return Ok(Value::new_none());
    };
    if attribute.executable {
        let files_to_run = TargetValue::from_value(value).and_then(|target| target.default_info("files_to_run"));
        return Ok(files_to_run.unwrap_or_else(Value::new_none));
    }
    let single_file = attribute.allow_single_file.is_some_and(|allow| allow.to_bool());
    if attribute.ty != AttrType::Label || !single_file {
        return Ok(value);
    }
    match target_files(value)?.as_slice() {
        [file] => Ok(eval.heap().alloc(FileValue(*file))),
        _ => Ok(Value::new_none()),
    }
}

/// Let subrules find their attributes' values in the rules calling them.
pub fn register() {
    bazel_starlark::subrule::set_attr_value_fn(subrule_attr_value);
}

/// Late-bound attributes for Apple platforms, which rules read
/// unconditionally but only use when building for them. Lenient opaque
/// values stand in for the targets, which bb doesn't analyze yet.
const APPLE_LATE_BOUND_ATTRS: &[&str] = &["_xcode_config"];

fn attr_value<'v>(input: &RuleInput, prepared: &Prepared<'v>, schema: &AttrSchema, value: &AttrValue, heap: Heap<'v>) -> Value<'v> {
    if APPLE_LATE_BOUND_ATTRS.contains(&schema.name.as_str()) && matches!(value, AttrValue::None) {
        return heap.alloc(Opaque::lenient(schema.name.as_str()));
    }
    if schema.is_dependency && schema.cfg == Cfg::Transition && schema.ty.has_labels() && !schema.ty.is_output() {
        return transitioned_value(input, prepared, schema, heap);
    }
    let config = input.dep_configs(schema).first().map(|(_, config)| *config).unwrap_or(input.config.id);
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

/// The targets of an attribute with a Starlark transition, which may split
/// them into several configurations: a list, as in Bazel, even of one.
fn transitioned_value<'v>(input: &RuleInput, prepared: &Prepared<'v>, schema: &AttrSchema, heap: Heap<'v>) -> Value<'v> {
    let targets = prepared.targets(input, schema.name.as_str());
    heap.alloc(AllocList(targets))
}

/// `ctx.split_attr`: for each attribute with a transition, its targets in
/// each configuration, by the configuration's name, or `None` for a
/// transition to one.
fn split_attr_struct<'v>(input: &RuleInput, prepared: &Prepared<'v>, heap: Heap<'v>) -> Value<'v> {
    let mut fields = Vec::new();
    for (attr, configs) in &input.attr_configs {
        let Some(schema) = input.schema(*attr) else {
            continue;
        };
        let mut labels = Vec::new();
        if let Some(value) = input.attr(attr.as_str()) {
            value.labels(&mut labels);
        }
        let mut by_key = Vec::new();
        if !labels.is_empty() {
            for (key, config) in configs {
                let targets: Vec<Value<'v>> = labels.iter().filter_map(|label| prepared.deps.get(&(*label, *config)).copied()).collect();
                let targets = match schema.ty {
                    AttrType::Label => targets.first().copied().unwrap_or_else(Value::new_none),
                    _ => heap.alloc(AllocList(targets)),
                };
                let key = if key.is_empty() { Value::new_none() } else { heap.alloc(key.as_str()) };
                by_key.push((key, targets));
            }
        }
        fields.push((attr.as_str(), heap.alloc(AllocDict(by_key))));
    }
    heap.alloc(AllocStruct(fields))
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

/// The package of the `.bzl` file defining the implementation, and its
/// repository's mapping.
fn definition_mapping(input: &RuleInput) -> Option<(PackageId, Arc<RepoMappingTable>)> {
    let Implementation::Starlark { module, .. } = &input.implementation else {
        return None;
    };
    let package = module.label.package;
    Some((package, input.mappings.get(&package.repo)?.clone()))
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
        let executable = targets.first().and_then(|target| attr_executable(*target));
        if let (None, Some(AttrValue::Label(label))) = (executable, input.attr(schema.name.as_str())) {
            let error = format!(
                "in {} attribute of {} rule {}: {label} does not refer to a valid executable target",
                crate::dep_checks::display_name(schema),
                input.class.kind,
                input.label
            );
            state.attribute_errors.borrow_mut().push(error);
        }
        let value = executable.map(|artifact| heap.alloc(FileValue(artifact))).unwrap_or_else(Value::new_none);
        fields.push((schema.name.as_str(), value));
    }
    heap.alloc(AllocStruct(fields))
}

/// What an executable attribute runs: the target's executable, as Bazel's
/// `FilesToRunProvider` gives it, which for a file, or a filegroup of one,
/// is the file itself.
fn attr_executable(target: Value<'_>) -> Option<Artifact> {
    if let Some((executable, _)) = target_executable(target) {
        return Some(executable);
    }
    let target = TargetValue::from_value(target)?;
    let files_to_run = FilesToRun::from_value(target.default_info("files_to_run")?)?;
    FileValue::artifact(files_to_run.executable)
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
