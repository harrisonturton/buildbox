//! `ctx.actions`: how rule implementations declare files and register the
//! actions that generate them.

use crate::inputs::InputSet;
use crate::action::{Action, ActionKind, ParamFile, RunfilesTree, Spawn};
use crate::args::{self, Args};
use crate::artifact::{self, Artifact};
use crate::file_value::FileValue;
use crate::runfiles::{FilesToRun, Runfiles};
use crate::state::{RuleState, runfiles_tree};
use allocative::Allocative;
use bazel_config::OptionValue;
use bazel_starlark::depset::Depset;
use intern::{PathId, Symbol};
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
    /// Depsets of tools, kept as sets, which many actions share.
    sets: Vec<InputSet>,
    runfiles: Vec<Arc<RunfilesTree>>,
}

impl Tools {
    fn add(&mut self, value: Value<'_>, state: &RuleState) -> anyhow::Result<()> {
        if let Some(files_to_run) = FilesToRun::from_value(value) {
            return self.add_files_to_run(files_to_run);
        }
        if Depset::from_value(value).is_some() {
            return self.add_set(value, state);
        }
        for artifact in files(Some(value), "tools")? {
            self.add_file(artifact, state);
        }
        Ok(())
    }

    /// A depset of tools: with the runfiles of those that are
    /// dependencies' executables.
    fn add_set(&mut self, value: Value<'_>, state: &RuleState) -> anyhow::Result<()> {
        let set = state.input_sets.borrow_mut().convert(value, "tools")?;
        let tool_runfiles = state.tool_runfiles.borrow();
        if !tool_runfiles.is_empty() {
            set.for_each(|artifact| self.runfiles.extend(tool_runfiles.get(&artifact).cloned()));
        }
        self.sets.push(set);
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

    /// The runfiles of the executables among `inputs` that
    /// `resolve_command` gave.
    fn add_resolved(&mut self, inputs: &InputSet, state: &RuleState) {
        let resolved = state.resolved_tools.borrow();
        if resolved.is_empty() {
            return;
        }
        let tool_runfiles = state.tool_runfiles.borrow();
        inputs.for_each(|artifact| {
            if resolved.contains(&artifact) {
                self.runfiles.extend(tool_runfiles.get(&artifact).cloned());
            }
        });
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

/// The arguments of an action after `command`: strings and `Args`, the
/// latter maybe in params files, which other actions write. As in Bazel's
/// `CommandLines.expand`, an `Args` whose params file is used only when
/// needed stays on the command line while the command line fits in
/// `--min_param_file_size`. The arguments that name params files are
/// recorded in `param_files`.
fn arguments(
    value: Option<Value<'_>>,
    state: &RuleState,
    outputs: &[Artifact],
    inputs: &mut Vec<Artifact>,
    command: &[String],
    param_files: &mut Vec<ParamFile>,
) -> anyhow::Result<Vec<String>> {
    let Some(value) = value.filter(|value| !value.is_none()) else {
        return Ok(Vec::new());
    };
    let items = bazel_starlark::convert::items(value).ok_or_else(|| anyhow::anyhow!("arguments must be a list"))?;
    let command_lines = 1 + items.iter().filter(|item| item.downcast_ref::<Args>().is_some()).count();
    let limit = max_command_line_length(state).saturating_sub(command_lines * PARAM_FILE_ARG_LENGTH_ESTIMATE);
    let mut length = total_length(command);
    let mut out = Vec::new();
    for item in items {
        let Some(args) = item.downcast_ref::<Args>() else {
            let argument = args::stringify_with(item, Some(&state.main_mapping));
            length += argument.len() + 1;
            out.push(argument);
            continue;
        };
        let args = args.state();
        let lines = args.lines();
        let Some(param_file_arg) = &args.param_file else {
            length += total_length(&lines);
            out.extend(lines);
            continue;
        };
        if args.param_file_when_needed && length + total_length(&lines) <= limit {
            length += total_length(&lines);
            out.extend(lines);
            continue;
        }
        let (in_file, kept): (Vec<String>, Vec<String>) = match args.flags_only() {
            true => lines.into_iter().partition(|line| line.starts_with("--")),
            false => (lines, Vec::new()),
        };
        let format = match args.flags_only() {
            true => "multiline",
            false => args.param_file_format.as_str(),
        };
        let params = write_param_file(state, outputs, &in_file, format)?;
        inputs.push(params);
        param_files.push(ParamFile {
            argument: (command.len() + out.len()) as u32,
            path: params.exec_path,
            format: format.to_string(),
        });
        let argument = param_file_arg.replacen("%s", &params.exec_path.to_string(), 1);
        length += argument.len() + 1 + total_length(&kept);
        out.push(argument);
        out.extend(kept);
    }
    Ok(out)
}

/// What Bazel allows for each params file's argument when deciding
/// whether a command line fits.
const PARAM_FILE_ARG_LENGTH_ESTIMATE: usize = 512;

/// The longest command line without params files, `--min_param_file_size`.
fn max_command_line_length(state: &RuleState) -> usize {
    match state.config.options.get("min_param_file_size") {
        Ok(OptionValue::Int(size)) => usize::try_from(*size).unwrap_or(0),
        _ => DEFAULT_MIN_PARAM_FILE_SIZE,
    }
}

const DEFAULT_MIN_PARAM_FILE_SIZE: usize = 32768;

/// The length of arguments on a command line, each with a separator.
fn total_length(arguments: &[String]) -> usize {
    arguments.iter().map(|argument| argument.len() + 1).sum()
}

/// Write a params file for an action, next to its first output.
fn write_param_file(state: &RuleState, outputs: &[Artifact], arguments: &[String], format: &str) -> anyhow::Result<Artifact> {
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
            content: args::param_file_contents(arguments, format),
            executable: false,
            from_args: true,
        },
    })?;
    Ok(params)
}

/// The entries of a dict of strings, in its order.
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
    Ok(entries)
}

/// The text of a progress message, with `%{label}`, and `%{input}` and
/// `%{output}` substituted by the paths, relative to their roots, of the
/// primary input and the first output.
fn progress_message(message: Option<&str>, state: &RuleState, primary_input: Option<Artifact>, outputs: &[Artifact]) -> String {
    let Some(message) = message else {
        return String::new();
    };
    let path = |artifact: Option<&Artifact>| artifact.map(|artifact| artifact.root_relative().to_string()).unwrap_or_default();
    let mut message = message.replace("%{label}", &state.label.to_string()).replace("%{output}", &path(outputs.first()));
    if message.contains("%{input}") {
        message = message.replace("%{input}", &path(primary_input.as_ref()));
    }
    message
}

/// An action's primary input, as Bazel's Starlark actions have it: the
/// first of the inputs it's given, in their order, or else of its tools.
fn primary_input(given: Option<Value<'_>>, tools: &Tools) -> Option<Artifact> {
    let given = given.filter(|value| !value.is_none());
    let first_given = given.and_then(|value| match Depset::from_value(value) {
        Some(depset) => depset.to_list().into_iter().find_map(FileValue::artifact),
        None => bazel_starlark::convert::items(value)?.into_iter().find_map(FileValue::artifact),
    });
    first_given.or_else(|| tools.inputs.first().copied()).or_else(|| tools.sets.iter().find_map(|set| set.to_vec().first().copied()))
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
        tools.add_resolved(&declared, state);
        let primary = primary_input(self.inputs, &tools);
        let mut arguments = command;
        let mut direct = Vec::new();
        let mut param_files = Vec::new();
        let added = self::arguments(self.arguments, state, &outputs, &mut direct, &arguments, &mut param_files)?;
        arguments.extend(added);
        direct.extend(tools.inputs);
        let inputs = InputSet::new(direct, std::iter::once(declared).chain(tools.sets).collect());
        let mut seen = std::collections::HashSet::new();
        tools.runfiles.retain(|tree| seen.insert(Arc::as_ptr(tree)));

        let fixed = string_dict(self.env, "env")?;
        let env = match self.use_default_shell_env {
            true => crate::action::environment(state.config.options.action_env.iter().cloned().chain(fixed)),
            false => crate::action::environment(fixed),
        };
        let action = Action {
            mnemonic: Symbol::intern(self.mnemonic.unwrap_or("Action")),
            progress_message: progress_message(self.progress_message, state, primary, &outputs),
            inputs,
            runfiles: tools.runfiles,
            outputs,
            kind: ActionKind::Spawn(Spawn {
                arguments,
                env,
                execution_info: crate::state::action_execution_info(string_dict(self.execution_requirements, "execution_requirements")?, &state.tag_execution_info),
                param_files,
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
    status: StatusFile,
    eval: &mut Evaluator<'v, '_, '_>,
) -> anyhow::Result<FileValue> {
    let heap = eval.heap();
    let (file, values, which): (_, &[(&str, &str)], _) = match status {
        StatusFile::Stable => (crate::workspace_status::info_file(), &crate::workspace_status::STABLE_STATUS, "stable"),
        StatusFile::Volatile => (crate::workspace_status::version_file(), &crate::workspace_status::VOLATILE_STATUS, "volatile"),
    };
    let status = heap.alloc(starlark::values::dict::AllocDict(values.iter().copied()));
    let substitutions = eval.eval_function(transform_func, &[status], &[]).map_err(|error| anyhow::anyhow!("{error}"))?;
    let substitutions = string_dict(Some(substitutions), "the substitutions transform_func returns")?;
    let state = RuleState::get(eval)?;
    let template = FileValue::artifact(template).ok_or_else(|| anyhow::anyhow!("template must be a File"))?;
    let output = state.declare(output_file_name, false)?;
    state.register(Action {
        mnemonic: Symbol::intern("TranslateBuildInfo"),
        progress_message: format!("Translating {which} BuildInfo file"),
        inputs: InputSet::of(vec![file]),
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

/// Which status file a translated build info file comes from.
enum StatusFile {
    Stable,
    Volatile,
}

/// The path, relative to the rule's package, of an output declared in a
/// sibling file's directory, or in the package. As in Bazel, the sibling
/// may be a source file, and the output must be in the rule's package.
fn sibling_name(state: &RuleState, filename: &str, sibling: Option<Value<'_>>) -> anyhow::Result<String> {
    let Some(sibling) = sibling.filter(|value| !value.is_none()) else {
        return Ok(filename.to_string());
    };
    let sibling = FileValue::artifact(sibling).ok_or_else(|| anyhow::anyhow!("sibling must be a File"))?;
    let package = artifact::package_dir(state.label.package);
    let dir = sibling.output_dir_relative().parent().unwrap_or(PathId::ROOT);
    let Some(relative) = dir.strip_prefix(package) else {
        anyhow::bail!("the output artifact '{dir}/{filename}' is not under package directory '{package}' for target '{}'", state.label);
    };
    match relative.is_root() {
        true => Ok(filename.to_string()),
        false => Ok(format!("{relative}/{filename}")),
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
        let from_args = content.downcast_ref::<Args>().is_some();
        let content = match content.downcast_ref::<Args>() {
            Some(args) => {
                let args = args.state();
                args::param_file_contents(&args.lines(), &args.param_file_format)
            }
            None => args::stringify_with(content, Some(&state.main_mapping)),
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
                from_args,
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
        let _ = use_exec_root_for_source;
        let state = RuleState::get(eval)?;
        let output = FileValue::artifact(output).ok_or_else(|| anyhow::anyhow!("symlink: output must be a File"))?;
        if target_path.is_some() {
            anyhow::bail!("symlink: target_path isn't supported by bb yet");
        }
        let target = target_file
            .and_then(FileValue::artifact)
            .ok_or_else(|| anyhow::anyhow!("symlink: target_file must be a File"))?;
        let mnemonic = match is_executable {
            true => "ExecutableSymlink",
            false => "Symlink",
        };
        let message = progress_message.unwrap_or("Creating symlink %{output}");
        state.register(Action {
            mnemonic: Symbol::intern(mnemonic),
            progress_message: self::progress_message(Some(message), state, Some(target), &[output]),
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
        let _ = (unused_inputs_list, input_manifests, shadowed_action, resource_set, toolchain);
        let state = RuleState::get(eval)?;
        state.check_exec_group(exec_group)?;
        let accepted = executable.unpack_str().is_some() || FileValue::artifact(executable).is_some() || FilesToRun::from_value(executable).is_some();
        if !accepted {
            anyhow::bail!(
                "in call to run(), parameter 'executable' got value of type '{}', want 'File, string, or FilesToRunProvider'",
                executable.get_type()
            );
        }
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
        let _ = (input_manifests, shadowed_action, resource_set, toolchain);
        let state = RuleState::get(eval)?;
        state.check_exec_group(exec_group)?;
        let has_arguments = arguments.and_then(bazel_starlark::convert::items).is_some_and(|items| !items.is_empty());
        let command = match command.unpack_str() {
            Some(script) => {
                let mut command = vec!["/bin/bash".to_string(), "-c".to_string(), script.to_string()];
                command.extend(has_arguments.then(String::new));
                command
            }
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
        transform_status_file(transform_func, template, output_file_name, StatusFile::Volatile, eval)
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
        transform_status_file(transform_func, template, output_file_name, StatusFile::Stable, eval)
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
