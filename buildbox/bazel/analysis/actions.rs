//! `ctx.actions`: how rule implementations declare files and register the
//! actions that generate them.

use crate::inputs::InputSet;
use crate::action::{Action, ActionKind, RunfilesTree, Spawn};
use crate::args::{self, Args};
use crate::artifact::{self, Artifact};
use crate::file_value::FileValue;
use crate::runfiles::{FilesToRun, Runfiles};
use crate::state::{RuleState, runfiles_tree};
use allocative::Allocative;
use bazel_starlark::depset::Depset;
use intern::Symbol;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::dict::DictRef;
use starlark::values::none::NoneType;
use starlark::values::{NoSerialize, ProvidesStaticType, StarlarkPagablePanic, StarlarkValue, Value, ValueLike, starlark_value};
use std::fmt;
use std::sync::Arc;

/// `ctx.actions`.
#[derive(Debug, ProvidesStaticType, NoSerialize, Allocative, StarlarkPagablePanic)]
pub struct Actions;

starlark_simple_value!(Actions);

impl fmt::Display for Actions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<actions>")
    }
}

#[starlark_value(type = "actions")]
impl<'v> StarlarkValue<'v> for Actions {
    fn get_methods() -> Option<&'static Methods> {
        static METHODS: MethodsStatic = MethodsStatic::new("actions", actions_methods);
        Some(METHODS.methods())
    }
}

/// The files of a `File`, a list of them, or a depset.
pub fn files(value: Option<Value<'_>>, what: &str) -> anyhow::Result<Vec<Artifact>> {
    let Some(value) = value.filter(|value| !value.is_none()) else {
        return Ok(Vec::new());
    };
    if let Some(artifact) = FileValue::artifact(value) {
        return Ok(vec![artifact]);
    }
    let items = match Depset::from_value(value) {
        Some(depset) => depset.to_list(),
        None => bazel_starlark::convert::items(value).ok_or_else(|| anyhow::anyhow!("{what} must be a list or depset of files, not {}", value.get_type()))?,
    };
    items
        .into_iter()
        .map(|item| FileValue::artifact(item).ok_or_else(|| anyhow::anyhow!("{what} must contain files, not {}", item.get_type())))
        .collect()
}

/// The inputs and runfiles trees of an action's tools: files, and the
/// `FilesToRunProvider`s of executables, whose runfiles come with them.
#[derive(Default)]
struct Tools {
    inputs: Vec<Artifact>,
    runfiles: Vec<Arc<RunfilesTree>>,
}

impl Tools {
    fn add(&mut self, value: Value<'_>, state: &RuleState) -> anyhow::Result<()> {
        if let Some(files_to_run) = FilesToRun::from_value(value) {
            return self.add_files_to_run(files_to_run);
        }
        for artifact in files(Some(value), "tools")? {
            self.add_file(artifact, state);
        }
        Ok(())
    }

    fn add_list(&mut self, value: Option<Value<'_>>, state: &RuleState) -> anyhow::Result<()> {
        let Some(value) = value.filter(|value| !value.is_none()) else {
            return Ok(());
        };
        if Depset::from_value(value).is_some() || FileValue::artifact(value).is_some() {
            return self.add(value, state);
        }
        let items = bazel_starlark::convert::items(value).ok_or_else(|| anyhow::anyhow!("tools must be a list or depset"))?;
        for item in items {
            self.add(item, state)?;
        }
        Ok(())
    }

    /// An executable file: with its runfiles, if it's a dependency's.
    fn add_file(&mut self, artifact: Artifact, state: &RuleState) {
        self.inputs.push(artifact);
        if let Some(tree) = state.tool_runfiles.borrow().get(&artifact) {
            self.runfiles.push(tree.clone());
        }
    }

    fn add_files_to_run(&mut self, files_to_run: &FilesToRun<'_>) -> anyhow::Result<()> {
        let Some(executable) = FileValue::artifact(files_to_run.executable) else {
            return Ok(());
        };
        self.inputs.push(executable);
        // A source file runs as itself, without a runfiles tree.
        if executable.is_source() {
            return Ok(());
        }
        if let Some(runfiles) = Runfiles::from_value(files_to_run.runfiles) {
            self.runfiles.push(runfiles_tree(executable, runfiles));
        }
        Ok(())
    }
}

/// The arguments of an action: strings and `Args`, the latter maybe in a
/// params file, which another action writes.
fn arguments(value: Option<Value<'_>>, state: &RuleState, outputs: &[Artifact], inputs: &mut Vec<Artifact>) -> anyhow::Result<Vec<String>> {
    let Some(value) = value.filter(|value| !value.is_none()) else {
        return Ok(Vec::new());
    };
    let items = bazel_starlark::convert::items(value).ok_or_else(|| anyhow::anyhow!("arguments must be a list"))?;
    let mut out = Vec::new();
    for item in items {
        let Some(args) = item.downcast_ref::<Args>() else {
            out.push(args::stringify(item));
            continue;
        };
        let args = args.state();
        let Some(param_file_arg) = &args.param_file else {
            out.extend(args.arguments);
            continue;
        };
        let params = write_param_file(state, outputs, &args)?;
        inputs.push(params);
        out.push(param_file_arg.replacen("%s", &params.exec_path.to_string(), 1));
    }
    Ok(out)
}

/// Write a params file for an action, next to its first output.
fn write_param_file(state: &RuleState, outputs: &[Artifact], args: &args::ArgsState) -> anyhow::Result<Artifact> {
    let first = outputs.first().ok_or_else(|| anyhow::anyhow!("an action needs at least one output"))?;
    let package = artifact::package_dir(state.label.package);
    let base = artifact::parse(&state.config.bin_dir()).join_path(package);
    let relative = first.exec_path.strip_prefix(base).map(|path| path.to_string()).unwrap_or_else(|| first.exec_path.name().to_string());
    let count = state.actions.borrow().len();
    let params = state.declare(&format!("{relative}-{count}.params"), false)?;
    state.register(Action {
        mnemonic: Symbol::intern("ParamFile"),
        progress_message: String::new(),
        inputs: InputSet::default(),
        runfiles: Vec::new(),
        outputs: vec![params],
        kind: ActionKind::Write {
            content: args::param_file_contents(args),
            executable: false,
        },
    })?;
    Ok(params)
}

fn string_dict(value: Option<Value<'_>>, what: &str) -> anyhow::Result<Vec<(String, String)>> {
    let Some(value) = value.filter(|value| !value.is_none()) else {
        return Ok(Vec::new());
    };
    let dict = DictRef::from_value(value).ok_or_else(|| anyhow::anyhow!("{what} must be a dict"))?;
    let mut entries = Vec::new();
    for (key, value) in dict.iter() {
        let key = key.unpack_str().ok_or_else(|| anyhow::anyhow!("{what} keys must be strings"))?;
        entries.push((key.to_string(), args::stringify(value)));
    }
    entries.sort();
    Ok(entries)
}

/// The text of a progress message, with `%{label}`, `%{input}` and
/// `%{output}` substituted.
fn progress_message(message: Option<&str>, state: &RuleState, inputs: &InputSet, outputs: &[Artifact]) -> String {
    let Some(message) = message else {
        return String::new();
    };
    let first_path = |artifacts: &[Artifact]| artifacts.first().map(|artifact| artifact.exec_path.to_string()).unwrap_or_default();
    let mut message = message.replace("%{label}", &state.label.to_string()).replace("%{output}", &first_path(outputs));
    if message.contains("%{input}") {
        message = message.replace("%{input}", &first_path(&inputs.to_vec()));
    }
    message
}

/// What `run` and `run_shell` share.
struct SpawnArgs<'v> {
    outputs: Value<'v>,
    inputs: Option<Value<'v>>,
    tools: Option<Value<'v>>,
    arguments: Option<Value<'v>>,
    mnemonic: Option<&'v str>,
    progress_message: Option<&'v str>,
    use_default_shell_env: bool,
    env: Option<Value<'v>>,
    execution_requirements: Option<Value<'v>>,
}

impl<'v> SpawnArgs<'v> {
    /// Register the action, running `command` followed by the arguments.
    fn register(self, state: &RuleState, command: Vec<String>, mut tools: Tools) -> anyhow::Result<NoneType> {
        let outputs = files(Some(self.outputs), "outputs")?;
        if outputs.is_empty() {
            anyhow::bail!("an action must have at least one output");
        }
        let declared = match self.inputs {
            Some(inputs) => state.input_sets.borrow_mut().convert(inputs, "inputs")?,
            None => InputSet::default(),
        };
        tools.add_list(self.tools, state)?;
        let mut arguments = command;
        let mut direct = Vec::new();
        arguments.extend(self::arguments(self.arguments, state, &outputs, &mut direct)?);
        direct.extend(tools.inputs);
        let inputs = InputSet::new(direct, vec![declared]);

        let mut env = string_dict(self.env, "env")?;
        if self.use_default_shell_env {
            env.extend(state.config.options.action_env.iter().cloned());
            env.sort();
            env.dedup_by(|a, b| a.0 == b.0);
        }
        let action = Action {
            mnemonic: Symbol::intern(self.mnemonic.unwrap_or("Action")),
            progress_message: progress_message(self.progress_message, state, &inputs, &outputs),
            inputs,
            runfiles: tools.runfiles,
            outputs,
            kind: ActionKind::Spawn(Spawn {
                arguments,
                env,
                execution_info: string_dict(self.execution_requirements, "execution_requirements")?,
            }),
        };
        state.register(action)?;
        Ok(NoneType)
    }
}

/// Expand a template with the substitutions `transform_func` makes from
/// build status values.
fn transform_status_file<'v>(
    transform_func: Value<'v>,
    template: Value<'v>,
    output_file_name: &str,
    status: &[(&str, &str)],
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<FileValue> {
    let heap = eval.heap();
    let status = heap.alloc(starlark::values::dict::AllocDict(status.iter().copied()));
    let substitutions = eval.eval_function(transform_func, &[status], &[]).map_err(|error| anyhow::anyhow!("{error}"))?;
    let substitutions = string_dict(Some(substitutions), "the substitutions transform_func returns")?;
    let state = RuleState::get(eval)?;
    let template = FileValue::artifact(template).ok_or_else(|| anyhow::anyhow!("template must be a File"))?;
    let output = state.declare(output_file_name, false)?;
    state.register(Action {
        mnemonic: Symbol::intern("TemplateExpand"),
        progress_message: String::new(),
        inputs: InputSet::default(),
        runfiles: Vec::new(),
        outputs: vec![output],
        kind: ActionKind::Template {
            template,
            substitutions,
            executable: false,
        },
    })?;
    Ok(FileValue(output))
}

/// The path of an output declared relative to a sibling file's directory,
/// or the rule's package.
fn sibling_name(state: &RuleState, filename: &str, sibling: Option<Value<'_>>) -> anyhow::Result<String> {
    let Some(sibling) = sibling.filter(|value| !value.is_none()) else {
        return Ok(filename.to_string());
    };
    let sibling = FileValue::artifact(sibling).ok_or_else(|| anyhow::anyhow!("sibling must be a File"))?;
    let package = artifact::parse(&state.config.bin_dir()).join_path(artifact::package_dir(state.label.package));
    let dir = sibling.exec_path.parent().and_then(|parent| parent.strip_prefix(package));
    match dir {
        Some(dir) if !dir.is_root() => Ok(format!("{dir}/{filename}")),
        _ => Ok(filename.to_string()),
    }
}

#[starlark_module]
fn actions_methods(builder: &mut MethodsBuilder) {
    /// `declare_file(filename, *, sibling = None)`.
    fn declare_file<'v>(
        #[starlark(this)] _this: Value<'v>,
        filename: &str,
        #[starlark(require = named)] sibling: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<FileValue> {
        let state = RuleState::get(eval)?;
        let name = sibling_name(state, filename, sibling)?;
        Ok(FileValue(state.declare(&name, false)?))
    }

    /// `declare_shareable_artifact(path, artifact_root = unbound)`: a file at
    /// `path` in the output directory, which actions of several targets may
    /// generate alike, such as a module map.
    fn declare_shareable_artifact<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = pos)] path: &str,
        #[starlark(require = pos)] artifact_root: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<FileValue> {
        let _ = artifact_root;
        Ok(FileValue(RuleState::get(eval)?.declare_at_root(path)?))
    }

    /// `declare_shareable_directory(path)`: a directory at `path` in the
    /// output directory, as link actions declare them.
    fn declare_shareable_directory<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = pos)] path: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<FileValue> {
        Ok(FileValue(RuleState::get(eval)?.declare_at_root_as(path, true)?))
    }

    /// `declare_directory(filename, *, sibling = None)`.
    fn declare_directory<'v>(
        #[starlark(this)] _this: Value<'v>,
        filename: &str,
        #[starlark(require = named)] sibling: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<FileValue> {
        let state = RuleState::get(eval)?;
        let name = sibling_name(state, filename, sibling)?;
        Ok(FileValue(state.declare(&name, true)?))
    }

    /// `declare_symlink(filename, *, sibling = None)`.
    fn declare_symlink<'v>(
        #[starlark(this)] _this: Value<'v>,
        filename: &str,
        #[starlark(require = named)] sibling: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<FileValue> {
        let state = RuleState::get(eval)?;
        let name = sibling_name(state, filename, sibling)?;
        Ok(FileValue(state.declare(&name, false)?))
    }

    /// `args()`.
    fn args(#[starlark(this)] _this: Value<'_>) -> anyhow::Result<Args> {
        Ok(Args::default())
    }

    /// `template_dict()`.
    fn template_dict(#[starlark(this)] _this: Value<'_>) -> anyhow::Result<args::TemplateDict> {
        Ok(args::TemplateDict::default())
    }

    /// `write(output, content, is_executable = False, *, mnemonic = None)`.
    fn write<'v>(
        #[starlark(this)] _this: Value<'v>,
        output: Value<'v>,
        content: Value<'v>,
        #[starlark(default = false)] is_executable: bool,
        #[starlark(require = named)] mnemonic: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let state = RuleState::get(eval)?;
        let output = FileValue::artifact(output).ok_or_else(|| anyhow::anyhow!("write: output must be a File"))?;
        let content = match content.downcast_ref::<Args>() {
            Some(args) => args::param_file_contents(&args.state()),
            None => args::stringify(content),
        };
        state.register(Action {
            mnemonic: Symbol::intern(mnemonic.unwrap_or("FileWrite")),
            progress_message: String::new(),
            inputs: InputSet::default(),
            runfiles: Vec::new(),
            outputs: vec![output],
            kind: ActionKind::Write {
                content,
                executable: is_executable,
            },
        })?;
        Ok(NoneType)
    }

    /// `expand_template(*, template, output, substitutions = {},
    /// is_executable = False, computed_substitutions = unbound)`.
    fn expand_template<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = named)] template: Value<'v>,
        #[starlark(require = named)] output: Value<'v>,
        #[starlark(require = named)] substitutions: Option<Value<'v>>,
        #[starlark(require = named, default = false)] is_executable: bool,
        #[starlark(require = named)] computed_substitutions: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let state = RuleState::get(eval)?;
        let template = FileValue::artifact(template).ok_or_else(|| anyhow::anyhow!("expand_template: template must be a File"))?;
        let output = FileValue::artifact(output).ok_or_else(|| anyhow::anyhow!("expand_template: output must be a File"))?;
        let mut substitutions = string_dict(substitutions, "substitutions")?;
        if let Some(computed) = computed_substitutions.and_then(|value| value.downcast_ref::<args::TemplateDict>()) {
            substitutions.extend(computed.substitutions());
        }
        state.register(Action {
            mnemonic: Symbol::intern("TemplateExpand"),
            progress_message: String::new(),
            inputs: InputSet::default(),
            runfiles: Vec::new(),
            outputs: vec![output],
            kind: ActionKind::Template {
                template,
                substitutions,
                executable: is_executable,
            },
        })?;
        Ok(NoneType)
    }

    /// `symlink(*, output, target_file = None, target_path = None,
    /// is_executable = False, progress_message = None)`. The output is a
    /// copy of the target file.
    fn symlink<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = named)] output: Value<'v>,
        #[starlark(require = named)] target_file: Option<Value<'v>>,
        #[starlark(require = named)] target_path: Option<&str>,
        #[starlark(require = named, default = false)] is_executable: bool,
        #[starlark(require = named)] progress_message: Option<&str>,
        #[starlark(require = named)] use_exec_root_for_source: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        // bb copies the target's contents, so where its path is taken from
        // makes no difference.
        let _ = (progress_message, use_exec_root_for_source);
        let state = RuleState::get(eval)?;
        let output = FileValue::artifact(output).ok_or_else(|| anyhow::anyhow!("symlink: output must be a File"))?;
        if target_path.is_some() {
            anyhow::bail!("symlink: target_path isn't supported by bb yet");
        }
        let target = target_file
            .and_then(FileValue::artifact)
            .ok_or_else(|| anyhow::anyhow!("symlink: target_file must be a File"))?;
        state.register(Action {
            mnemonic: Symbol::intern("Symlink"),
            progress_message: String::new(),
            inputs: InputSet::default(),
            runfiles: Vec::new(),
            outputs: vec![output],
            kind: ActionKind::Symlink {
                target,
                executable: is_executable,
            },
        })?;
        Ok(NoneType)
    }

    /// `run(*, outputs, inputs = [], executable, tools = None, arguments = [],
    /// mnemonic = None, progress_message = None, use_default_shell_env =
    /// False, env = None, execution_requirements = None, ...)`.
    fn run<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = named)] outputs: Value<'v>,
        #[starlark(require = named)] inputs: Option<Value<'v>>,
        #[starlark(require = named)] unused_inputs_list: Option<Value<'v>>,
        #[starlark(require = named)] executable: Value<'v>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
        #[starlark(require = named)] arguments: Option<Value<'v>>,
        #[starlark(require = named)] mnemonic: Option<&'v str>,
        #[starlark(require = named)] progress_message: Option<&'v str>,
        #[starlark(require = named, default = false)] use_default_shell_env: bool,
        #[starlark(require = named)] env: Option<Value<'v>>,
        #[starlark(require = named)] execution_requirements: Option<Value<'v>>,
        #[starlark(require = named)] input_manifests: Option<Value<'v>>,
        #[starlark(require = named)] exec_group: Option<Value<'v>>,
        #[starlark(require = named)] shadowed_action: Option<Value<'v>>,
        #[starlark(require = named)] resource_set: Option<Value<'v>>,
        #[starlark(require = named)] toolchain: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (unused_inputs_list, input_manifests, exec_group, shadowed_action, resource_set, toolchain);
        let state = RuleState::get(eval)?;
        let mut program = Tools::default();
        let command = match executable.unpack_str() {
            Some(path) => vec![path.to_string()],
            None => {
                program.add(executable, state)?;
                let file = program.inputs.first().ok_or_else(|| anyhow::anyhow!("run: executable has no file"))?;
                vec![file.exec_path.to_string()]
            }
        };
        let spawn = SpawnArgs {
            outputs,
            inputs,
            tools,
            arguments,
            mnemonic,
            progress_message,
            use_default_shell_env,
            env,
            execution_requirements,
        };
        spawn.register(state, command, program)
    }

    /// `run_shell(*, outputs, inputs = [], tools = None, arguments = [],
    /// mnemonic = None, command, progress_message = None,
    /// use_default_shell_env = False, env = None, execution_requirements =
    /// None, ...)`.
    fn run_shell<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = named)] outputs: Value<'v>,
        #[starlark(require = named)] inputs: Option<Value<'v>>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
        #[starlark(require = named)] arguments: Option<Value<'v>>,
        #[starlark(require = named)] mnemonic: Option<&'v str>,
        #[starlark(require = named)] command: Value<'v>,
        #[starlark(require = named)] progress_message: Option<&'v str>,
        #[starlark(require = named, default = false)] use_default_shell_env: bool,
        #[starlark(require = named)] env: Option<Value<'v>>,
        #[starlark(require = named)] execution_requirements: Option<Value<'v>>,
        #[starlark(require = named)] input_manifests: Option<Value<'v>>,
        #[starlark(require = named)] exec_group: Option<Value<'v>>,
        #[starlark(require = named)] shadowed_action: Option<Value<'v>>,
        #[starlark(require = named)] resource_set: Option<Value<'v>>,
        #[starlark(require = named)] toolchain: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (input_manifests, exec_group, shadowed_action, resource_set, toolchain);
        let state = RuleState::get(eval)?;
        let command = match command.unpack_str() {
            Some(script) => vec!["/bin/bash".to_string(), "-c".to_string(), script.to_string(), String::new()],
            None => bazel_starlark::convert::strings(command).ok_or_else(|| anyhow::anyhow!("run_shell: command must be a string"))?,
        };
        let spawn = SpawnArgs {
            outputs,
            inputs,
            tools,
            arguments,
            mnemonic,
            progress_message,
            use_default_shell_env,
            env,
            execution_requirements,
        };
        spawn.register(state, command, Tools::default())
    }

    /// `transform_version_file(*, transform_func, template,
    /// output_file_name)`: a file from the volatile build status. bb
    /// doesn't stamp, so the status has fixed values.
    fn transform_version_file<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = named)] transform_func: Value<'v>,
        #[starlark(require = named)] template: Value<'v>,
        #[starlark(require = named)] output_file_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<FileValue> {
        let status = [("BUILD_TIMESTAMP", "0"), ("BUILD_SCM_REVISION", "0"), ("BUILD_SCM_STATUS", "")];
        transform_status_file(transform_func, template, output_file_name, &status, eval)
    }

    /// `transform_info_file(*, transform_func, template, output_file_name)`:
    /// a file from the stable build status.
    fn transform_info_file<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = named)] transform_func: Value<'v>,
        #[starlark(require = named)] template: Value<'v>,
        #[starlark(require = named)] output_file_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<FileValue> {
        let status = [("BUILD_EMBED_LABEL", ""), ("BUILD_HOST", "bb"), ("BUILD_USER", "bb")];
        transform_status_file(transform_func, template, output_file_name, &status, eval)
    }

    /// `do_nothing(*, mnemonic, inputs = [])`.
    fn do_nothing<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = named)] mnemonic: &str,
        #[starlark(require = named)] inputs: Option<Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = (mnemonic, inputs);
        Ok(NoneType)
    }
}
