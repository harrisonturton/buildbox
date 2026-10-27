//! What builtins know about the rule being analyzed.

use intern::Symbol;
use crate::action::{Action, ActionKind, RunfilesTree};
use crate::artifact::{self, Artifact};
use crate::expand::LocationStyle;
use crate::runfiles::Runfiles;
use crate::inputs::InputSets;
use crate::shared::Sharing;
use bazel_config::{BuildConfiguration, OptionValue};
use bazel_label::{Label, LabelContext, RepoMappingTable};
use bazel_starlark::EvalContext;
use intern::PathId;
use starlark::eval::Evaluator;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

/// The state of analyzing one rule, which the builtins of rule
/// implementations, such as `ctx.actions`, share through the evaluation's
/// [`EvalContext::extension`].
pub struct RuleState {
    pub label: Label,
    /// The kind of rule, such as `cc_library`.
    pub kind: Symbol,
    pub config: Arc<BuildConfiguration>,
    /// How the rule's repository sees the others, for labels in
    /// `$(location)`.
    pub mapping: Arc<RepoMappingTable>,
    /// How the main repository sees the others, as command lines write
    /// labels.
    pub main_mapping: Arc<RepoMappingTable>,
    /// The actions registered so far.
    pub actions: RefCell<Vec<Action>>,
    /// The files declared so far, and whether an action generates each.
    pub declared: RefCell<Vec<(Artifact, bool)>>,
    /// The runfiles trees of the executables of dependencies, which actions
    /// that run them need.
    pub tool_runfiles: RefCell<HashMap<Artifact, Arc<RunfilesTree>>>,
    /// The input sets of the depsets actions read.
    pub input_sets: RefCell<InputSets>,
    /// Make variables, such as `$(BINDIR)`, by name.
    pub make_variables: Vec<(String, String)>,
    /// The files `$(location)` expands each label to: those of the
    /// dependencies, and the rule's outputs.
    pub locations: Vec<(Label, Vec<Artifact>)>,
    /// The constraint values of the target platform.
    pub target_constraints: Vec<Label>,
    /// Whether the rule is an analysis test, which may not register
    /// actions.
    pub analysis_test: bool,
    /// The execution requirements the rule's tags give its actions, as
    /// Bazel propagates them, sorted.
    pub tag_execution_info: Vec<(String, String)>,
    /// The toolchain types the rule resolves toolchains for.
    pub toolchain_types: Vec<Label>,
    /// Errors in the rule's attributes found as its `ctx` was made, as
    /// Bazel words them, which fail its analysis.
    pub attribute_errors: RefCell<Vec<String>>,
}

impl RuleState {
    pub fn new(label: Label, kind: Symbol, config: Arc<BuildConfiguration>, mapping: Arc<RepoMappingTable>, main_mapping: Arc<RepoMappingTable>) -> RuleState {
        let make_variables = default_make_variables(&config);
        RuleState {
            label,
            kind,
            config,
            mapping,
            main_mapping,
            actions: RefCell::new(Vec::new()),
            declared: RefCell::new(Vec::new()),
            tool_runfiles: RefCell::new(HashMap::new()),
            input_sets: RefCell::new(InputSets::default()),
            make_variables,
            locations: Vec::new(),
            target_constraints: Vec::new(),
            analysis_test: false,
            tag_execution_info: Vec::new(),
            toolchain_types: Vec::new(),
            attribute_errors: RefCell::new(Vec::new()),
        }
    }

    /// Set a make variable every rule has.
    pub fn set_make_variable(&mut self, name: &str, value: &str) {
        self.make_variables.retain(|(variable, _)| variable != name);
        self.make_variables.push((name.to_string(), value.to_string()));
    }

    /// The state of the rule `eval` is analyzing.
    pub fn get<'a>(eval: &Evaluator<'_, 'a, '_>) -> anyhow::Result<&'a RuleState> {
        let context = EvalContext::get(eval)?;
        let extension = context.extension.as_ref().ok_or_else(|| anyhow::anyhow!("only rule implementations can do this"))?;
        extension.downcast_ref::<RuleState>().ok_or_else(|| anyhow::anyhow!("only rule implementations can do this"))
    }

    /// Declare a file the rule generates, at `name` relative to its
    /// package's output directory.
    pub fn declare(&self, name: &str, is_directory: bool) -> anyhow::Result<Artifact> {
        let path = PathId::parse(name).map_err(|error| anyhow::anyhow!("invalid file name '{name}': {error}"))?;
        let artifact = Artifact::derived(self.label, &self.config, path, is_directory);
        let mut declared = self.declared.borrow_mut();
        if !declared.iter().any(|(existing, _)| *existing == artifact) {
            declared.push((artifact, false));
        }
        Ok(artifact)
    }

    /// Declare a file the rule generates at `path` relative to the output
    /// directory.
    pub fn declare_at_root(&self, path: &str) -> anyhow::Result<Artifact> {
        self.declare_at_root_as(path, false)
    }

    /// Declare a file, or a directory if `is_directory`, the rule generates
    /// at `path` relative to the output directory.
    pub fn declare_at_root_as(&self, path: &str, is_directory: bool) -> anyhow::Result<Artifact> {
        let path = PathId::parse(path).map_err(|error| anyhow::anyhow!("invalid file path '{path}': {error}"))?;
        let artifact = Artifact::derived_at_root(self.label, &self.config, path, is_directory);
        let mut declared = self.declared.borrow_mut();
        if !declared.iter().any(|(existing, _)| *existing == artifact) {
            declared.push((artifact, is_directory));
        }
        Ok(artifact)
    }

    /// Register an action. Its outputs must be files this rule declared, and
    /// no other action may generate them, unless it's the same action, which
    /// Bazel shares. An exact repeat is listed again, as Bazel lists it, but
    /// its outputs are generated by the first.
    pub fn register(&self, action: Action) -> anyhow::Result<()> {
        if self.analysis_test {
            anyhow::bail!(
                "implementation function of a rule with analysis_test=true may not register actions. Analysis test rules may only return success/failure information via AnalysisTestResultInfo."
            );
        }
        self.register_own(action)
    }

    /// Register an action of bb's own for the rule, which analysis tests
    /// may have too.
    pub fn register_own(&self, action: Action) -> anyhow::Result<()> {
        match self.share(&action) {
            Some(true) => {
                self.actions.borrow_mut().push(action);
                return Ok(());
            }
            Some(false) => return Ok(()),
            None => {}
        }
        let mut declared = self.declared.borrow_mut();
        for output in &action.outputs {
            let Some(entry) = declared.iter_mut().find(|(artifact, _)| artifact == output) else {
                anyhow::bail!("{}: output '{}' wasn't declared by this rule", self.label, output.short_path());
            };
            if entry.1 {
                anyhow::bail!("{}: '{}' is generated by more than one action", self.label, output.short_path());
            }
            entry.1 = true;
        }
        self.actions.borrow_mut().push(action);
        Ok(())
    }

    /// If `action` is the same as one registered already, which then stands
    /// for both, whether it's an exact repeat. Symlinks to the same file
    /// are the same action whether or not they say the file is executable,
    /// as in Bazel; the shared one is executable if either is.
    fn share(&self, action: &Action) -> Option<bool> {
        let mut actions = self.actions.borrow_mut();
        let existing = actions.iter_mut().find(|existing| same_action(existing, action))?;
        let repeat = *existing == *action;
        if let (ActionKind::Symlink { executable, .. }, ActionKind::Symlink { executable: new, .. }) = (&mut existing.kind, &action.kind) {
            *executable |= *new;
        }
        Some(repeat)
    }

    /// Parse a label relative to the rule's package.
    pub fn parse_label(&self, text: &str) -> anyhow::Result<Label> {
        let context = LabelContext::new(self.label.package, &*self.mapping);
        Ok(Label::parse(text, &context)?)
    }

    /// Record the runfiles of an executable, for the actions that run it.
    pub fn add_tool(&self, executable: Artifact, runfiles: &Runfiles<'_>) {
        let tree = runfiles_tree(executable, runfiles);
        self.tool_runfiles.borrow_mut().insert(executable, tree);
    }

    /// Expand `$(location)` functions and make variables in `text`.
    /// `extra` are more labels' files, which take precedence.
    pub fn expand(&self, text: &str, extra: &[(Label, Vec<Artifact>)], style: LocationStyle) -> anyhow::Result<String> {
        self.expand_with(text, extra, &[], style)
    }

    /// Expand `text`, with more variables, which take precedence over the
    /// make variables.
    pub fn expand_with(
        &self,
        text: &str,
        extra: &[(Label, Vec<Artifact>)],
        variables: &[(String, String)],
        style: LocationStyle,
    ) -> anyhow::Result<String> {
        let mut lookup = |name: &str, argument: Option<&str>| -> anyhow::Result<String> {
            let Some(argument) = argument else {
                if let Some((_, value)) = variables.iter().find(|(variable, _)| variable == name) {
                    return Ok(value.clone());
                }
                return match self.make_variable(name) {
                    Some(value) => Ok(value.to_string()),
                    None => Err(anyhow::anyhow!("$({name}) is not defined")),
                };
            };
            let label = self.parse_label(argument)?;
            let files = extra
                .iter()
                .chain(&self.locations)
                .find(|(candidate, _)| *candidate == label)
                .map(|(_, files)| files.as_slice());
            let Some(files) = files else {
                anyhow::bail!("label '{argument}' in $({name}) is not a declared prerequisite of this rule");
            };
            match crate::expand::location(name, files, style, argument) {
                Some(result) => result,
                None => Err(anyhow::anyhow!("unknown function $({name} ...)")),
            }
        };
        crate::expand::expand(text, &mut lookup)
    }

    pub fn make_variable(&self, name: &str) -> Option<&str> {
        self.make_variables.iter().find(|(variable, _)| variable == name).map(|(_, value)| value.as_str())
    }
}

fn same_action(a: &Action, b: &Action) -> bool {
    if let (ActionKind::Symlink { target: a_target, .. }, ActionKind::Symlink { target: b_target, .. }) = (&a.kind, &b.kind) {
        return a_target == b_target && a.outputs == b.outputs;
    }
    a == b
}

/// Runfiles trees alive in any target's actions.
static RUNFILES_TREES: LazyLock<Sharing<RunfilesTree>> = LazyLock::new(Sharing::default);

/// The runfiles tree of an executable, in the directory next to it, shared
/// with every action that runs the same tool.
pub fn runfiles_tree(executable: Artifact, runfiles: &Runfiles<'_>) -> Arc<RunfilesTree> {
    let dir = artifact::parse(&format!("{}.runfiles", executable.exec_path));
    let mut entries: Vec<(PathId, Artifact)> = runfiles
        .entries(artifact::runfiles_repo_name(bazel_label::RepoName::MAIN))
        .into_iter()
        .filter_map(|(path, artifact)| Some((PathId::parse(&path).ok()?, artifact)))
        .collect();
    entries.sort();
    entries.dedup_by(|a, b| a.0 == b.0);
    let empty = runfiles
        .empty_files(artifact::runfiles_repo_name(bazel_label::RepoName::MAIN))
        .iter()
        .filter_map(|path| PathId::parse(path).ok())
        .collect();
    RUNFILES_TREES.share(RunfilesTree { dir, entries, empty })
}

/// The execution requirements tags give, as Bazel's
/// `TargetUtils.getExecutionInfo` picks them: those that start with
/// `block-`, `requires-`, `no-`, `supports-`, `disable-`, `cpu:` or
/// `resources:`, and `local` and `worker-key-mnemonic`, sorted.
pub fn tag_execution_info(tags: &[String]) -> Vec<(String, String)> {
    let mut info: Vec<(String, String)> = tags.iter().filter(|tag| is_execution_info_key(tag)).map(|tag| (tag.clone(), String::new())).collect();
    info.sort();
    info.dedup();
    info
}

/// Whether Bazel takes `key` as an execution requirement.
fn is_execution_info_key(key: &str) -> bool {
    let prefixes = ["block-", "requires-", "no-", "supports-", "disable-", "cpu:", "resources:"];
    prefixes.iter().any(|prefix| key.starts_with(prefix)) || key == "local" || key == "worker-key-mnemonic"
}

/// The execution info of a Starlark action, as Bazel gives it: the
/// `execution_requirements` it takes, then those the rule's tags give that
/// they don't, sorted.
pub fn action_execution_info(requirements: Vec<(String, String)>, tags: &[(String, String)]) -> Vec<(String, String)> {
    let mut info: Vec<(String, String)> = requirements.into_iter().filter(|(key, _)| is_execution_info_key(key)).collect();
    for (tag, value) in tags {
        if !info.iter().any(|(key, _)| key == tag) {
            info.push((tag.clone(), value.clone()));
        }
    }
    info.sort();
    info
}

/// The make variables every rule has, from its configuration.
fn default_make_variables(config: &BuildConfiguration) -> Vec<(String, String)> {
    let bin_dir = config.bin_dir();
    let mut variables = vec![
        ("COMPILATION_MODE".to_string(), config.compilation_mode.clone()),
        ("BINDIR".to_string(), bin_dir.clone()),
        ("GENDIR".to_string(), bin_dir),
        ("TARGET_CPU".to_string(), config.cpu.clone()),
    ];
    if let Ok(OptionValue::Assignments(defines)) = config.options.get("define") {
        variables.extend(defines.iter().cloned());
    }
    variables
}
