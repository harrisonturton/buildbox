//! `py_builtins`: the native API `@_builtins`' `py_internal` is written
//! against, which rules_python's rules call through it.

use crate::action::{Action, ActionKind};
use crate::artifact::{Artifact, runfiles_repo_name};
use crate::expand::LocationStyle;
use crate::file_value::FileValue;
use crate::inputs::InputSet;
use crate::runfiles::Runfiles;
use crate::state::RuleState;
use crate::target_value::TargetValue;
use bazel_label::Label;
use bazel_starlark::depset::Depset;
use bazel_starlark::label_value::LabelValue;
use intern::Symbol;
use starlark::environment::GlobalsBuilder;
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::values::none::NoneType;
use starlark::values::Value;

#[starlark_module]
fn py_builtins_functions(builder: &mut GlobalsBuilder) {
    /// Whether Bzlmod is enabled, which it always is.
    fn is_bzlmod_enabled<'v>(ctx: Value<'v>) -> anyhow::Result<bool> {
        let _ = ctx;
        Ok(true)
    }

    /// Whether a depset holds exactly one file.
    fn is_singleton_depset<'v>(#[starlark(require = pos)] value: Value<'v>) -> anyhow::Result<bool> {
        let depset = Depset::from_value(value).ok_or_else(|| anyhow::anyhow!("is_singleton_depset: expected a depset, not {}", value.get_type()))?;
        Ok(depset.to_list().len() == 1)
    }

    /// Whether `subject` matches `pattern` entirely, as Java's
    /// `String.matches` does.
    fn regex_match(#[starlark(require = pos)] subject: &str, #[starlark(require = pos)] pattern: &str) -> anyhow::Result<bool> {
        let regex = regex::Regex::new(&format!("^(?:{pattern})$")).map_err(|error| anyhow::anyhow!("regex_match: invalid pattern '{pattern}': {error}"))?;
        Ok(regex.is_match(subject))
    }

    /// The kind of the rule being analyzed.
    fn get_rule_name<'v>(ctx: Value<'v>, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<String> {
        let _ = ctx;
        Ok(RuleState::get(eval)?.kind.to_string())
    }

    /// The name of the OS bb runs on, as Bazel names it.
    fn get_current_os_name() -> anyhow::Result<&'static str> {
        Ok(match std::env::consts::OS {
            "macos" => "osx",
            other => other,
        })
    }

    /// The path of a label's package relative to the main repository's
    /// directory in a runfiles tree.
    fn get_label_repo_runfiles_path<'v>(label: Value<'v>) -> anyhow::Result<String> {
        let label = LabelValue::from_value(label).map(|label| label.0).ok_or_else(|| anyhow::anyhow!("expected a Label, not {}", label.get_type()))?;
        Ok(label_repo_runfiles_path(label))
    }

    /// Expand `$(location)`, as runfiles paths, and make variables in an
    /// attribute's value, with `targets` naming more files.
    fn expand_location_and_make_variables<'v>(
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] attribute_name: &str,
        #[starlark(require = named)] expression: &str,
        #[starlark(require = named)] targets: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<String> {
        let _ = ctx;
        let state = RuleState::get(eval)?;
        let extra = location_files(targets)?;
        state.expand(expression, &extra, LocationStyle::Root).map_err(|error| anyhow::anyhow!("{attribute_name}: {error}"))
    }

    /// The runfiles, with the empty `__init__.py` files Python needs added
    /// to their tree.
    fn merge_runfiles_with_generated_inits_empty_files_supplier<'v>(
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] runfiles: Value<'v>,
    ) -> anyhow::Result<Runfiles<'v>> {
        let _ = ctx;
        let given = Runfiles::from_value(runfiles).ok_or_else(|| anyhow::anyhow!("expected runfiles, not {}", runfiles.get_type()))?;
        let has_files = Depset::from_value(given.files).is_some_and(|files| !files.to_list().is_empty());
        if !has_files && given.symlinks.is_empty() && given.root_symlinks.is_empty() {
            anyhow::bail!("input runfiles cannot be empty");
        }
        Ok(Runfiles {
            files: given.files,
            symlinks: given.symlinks.clone(),
            root_symlinks: given.root_symlinks.clone(),
            init_py_files: true,
        })
    }

    /// Write the repository mapping manifest of a runfiles tree to `output`.
    fn create_repo_mapping_manifest<'v>(
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] runfiles: Value<'v>,
        #[starlark(require = named)] output: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = ctx;
        let state = RuleState::get(eval)?;
        let files = runfiles_entries(runfiles)?.into_iter().map(|(_, artifact)| artifact).collect();
        let mut action = single_output_action("RepoMappingManifest", output, ActionKind::RepoMappingManifest { files })?;
        action.progress_message = format!("Writing repo mapping manifest for {}", state.label);
        state.register(action)?;
        Ok(NoneType)
    }

    /// Write the runfiles paths of a runfiles tree's files to `output`, one
    /// a line, sorted.
    fn create_sources_only_manifest<'v>(
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] runfiles: Value<'v>,
        #[starlark(require = named)] output: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = ctx;
        let state = RuleState::get(eval)?;
        let mut paths: Vec<String> = runfiles_entries(runfiles)?.into_iter().map(|(path, _)| path).collect();
        if let Some(runfiles) = Runfiles::from_value(runfiles) {
            paths.extend(runfiles.empty_files(runfiles_repo_name(bazel_label::RepoName::MAIN)));
        }
        paths.sort();
        paths.dedup();
        let content: String = paths.iter().map(|path| format!("{path}\n")).collect();
        let kind = ActionKind::Write {
            content,
            executable: false,
            from_args: false,
        };
        state.register(single_output_action("PackagingSourcesManifest", output, kind)?)?;
        Ok(NoneType)
    }

    /// Copy `read_from` to `write_to`.
    fn copy_without_caching<'v>(
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] read_from: Value<'v>,
        #[starlark(require = named)] write_to: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = ctx;
        let state = RuleState::get(eval)?;
        let target = FileValue::artifact(read_from).ok_or_else(|| anyhow::anyhow!("copy_without_caching: read_from must be a File"))?;
        let kind = ActionKind::Symlink { target, executable: false };
        let mut action = single_output_action("PyCopyWithoutCaching", write_to, kind)?;
        action.inputs = InputSet::of(vec![target]);
        state.register(action)?;
        Ok(NoneType)
    }

    /// Declare a file of the rule, in its package's directory of `root`.
    fn declare_constant_metadata_file<'v>(
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] name: &str,
        #[starlark(require = named)] root: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (ctx, root);
        let artifact = RuleState::get(eval)?.declare(name, false)?;
        Ok(eval.heap().alloc(FileValue(artifact)))
    }

    /// Whether `--experimental_action_listener` registered any listeners,
    /// which bb doesn't support, so none are.
    fn are_action_listeners_enabled<'v>(#[starlark(require = pos)] ctx: Value<'v>) -> anyhow::Result<bool> {
        let _ = ctx;
        Ok(false)
    }

    /// The pseudo-action action listeners see, which bb doesn't support.
    fn add_py_extra_pseudo_action<'v>(
        #[starlark(require = named)] ctx: Value<'v>,
        #[starlark(require = named)] dependency_transitive_python_sources: Value<'v>,
    ) -> anyhow::Result<NoneType> {
        let _ = (ctx, dependency_transitive_python_sources);
        anyhow::bail!("action listeners aren't supported by bb yet")
    }
}

/// A label's package as a path relative to the main repository's
/// directory in a runfiles tree: `pkg`, or `../repo/pkg`.
fn label_repo_runfiles_path(label: Label) -> String {
    let package = label.package;
    if package.repo.is_main() {
        return package.path.to_string();
    }
    let repo = runfiles_repo_name(package.repo);
    match package.path.to_string().as_str() {
        "" => format!("../{repo}"),
        path => format!("../{repo}/{path}"),
    }
}

/// The files `$(location)` expands each target's label to: its executable,
/// if it has one, or its default files.
fn location_files(targets: Value<'_>) -> anyhow::Result<Vec<(Label, Vec<Artifact>)>> {
    let items = bazel_starlark::convert::items(targets).ok_or_else(|| anyhow::anyhow!("targets must be a list of targets"))?;
    let mut files = Vec::with_capacity(items.len());
    for item in items {
        let target = TargetValue::from_value(item).ok_or_else(|| anyhow::anyhow!("targets must be a list of targets, not {}", item.get_type()))?;
        let executable = target.default_info("executable").and_then(FileValue::artifact);
        let artifacts = match executable {
            Some(executable) => vec![executable],
            None => crate::actions::files(target.default_info("files"), "files")?,
        };
        files.push((target.label, artifacts));
    }
    Ok(files)
}

/// Each file of a runfiles value, at its path in the tree.
fn runfiles_entries(runfiles: Value<'_>) -> anyhow::Result<Vec<(String, Artifact)>> {
    let runfiles = Runfiles::from_value(runfiles).ok_or_else(|| anyhow::anyhow!("expected runfiles, not {}", runfiles.get_type()))?;
    Ok(runfiles.entries(runfiles_repo_name(bazel_label::RepoName::MAIN)))
}

/// An action of bb's that makes one file, `output`.
fn single_output_action(mnemonic: &str, output: Value<'_>, kind: ActionKind) -> anyhow::Result<Action> {
    let output = FileValue::artifact(output).ok_or_else(|| anyhow::anyhow!("output must be a File, not {}", output.get_type()))?;
    Ok(Action {
        mnemonic: Symbol::intern(mnemonic),
        progress_message: String::new(),
        inputs: InputSet::default(),
        runfiles: Vec::new(),
        outputs: vec![output],
        kind,
    })
}

/// Add `py_builtins` to `_builtins.internal`.
pub fn register_builtins_internal(builder: &mut GlobalsBuilder) {
    builder.namespace("py_builtins", py_builtins_functions);
}
