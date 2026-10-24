//! C++ compile actions: running the toolchain's compiler on one source file
//! with the command line its features give.

use super::features::{FeatureConfiguration, Variables};
use crate::action::{Action, ActionKind, Spawn};
use crate::artifact::Artifact;
use crate::inputs::InputSet;
use crate::state::RuleState;
use intern::Symbol;
use std::collections::BTreeMap;

/// The compile action of each kind of source, by file extension, as Bazel's
/// `CppFileTypes` tell them apart.
const SOURCE_ACTIONS: &[(&[&str], &str)] = &[
    (&["cppmap"], "c++-module-compile"),
    (&["h", "hh", "hpp", "ipp", "hxx", "h++", "inc", "inl", "tlh", "tli", "H", "tcc"], "c++-header-parsing"),
    (&["c"], "c-compile"),
    (&["cc", "cpp", "cxx", "c++", "C", "cu", "cl"], "c++-compile"),
    (&["m"], "objc-compile"),
    (&["mm"], "objc++-compile"),
    (&["s", "asm"], "assemble"),
    (&["S"], "preprocess-assemble"),
    (&["ipb"], "clif-match"),
    (&["pcm"], "c++-module-codegen"),
];

/// One source file's compilation.
pub struct Compile {
    pub source: Artifact,
    /// The action to configure it as, if not the one its extension implies.
    pub action_name: Option<String>,
    /// Said before "Compiling" in its progress message, if given.
    pub progress_message_prefix: Option<String>,
    /// Every input but the source: headers, toolchain files and so on.
    pub inputs: InputSet,
    pub outputs: Vec<Artifact>,
}

/// Register the action that compiles `compile` as `configuration` and
/// `variables` give its command line.
pub fn register(state: &RuleState, configuration: &FeatureConfiguration, variables: &Variables, compile: Compile) -> anyhow::Result<()> {
    let action = match compile.action_name {
        Some(action) => action,
        None => action_name(&compile.source)?.to_string(),
    };
    let tool = configuration.tool(&action)?;
    let mut arguments = vec![tool.path.clone()];
    arguments.extend(configuration.command_line(&action, variables)?);
    let execution_info = tool.execution_requirements.iter().map(|requirement| (requirement.clone(), String::new())).collect();
    let progress_message = progress_message(compile.progress_message_prefix.as_deref(), &compile.source);
    state.register(Action {
        mnemonic: Symbol::intern("CppCompile"),
        progress_message,
        inputs: InputSet::new(vec![compile.source], vec![compile.inputs]),
        runfiles: Vec::new(),
        outputs: compile.outputs,
        kind: ActionKind::Spawn(Spawn {
            arguments,
            env: action_env(state, configuration, &action, variables)?,
            execution_info,
        }),
    })
}

/// The compile action a source file's extension implies.
pub fn action_name(source: &Artifact) -> anyhow::Result<&'static str> {
    let name = source.exec_path.name().as_str();
    let extension = name.rsplit_once('.').map(|(_, extension)| extension).unwrap_or("");
    match SOURCE_ACTIONS.iter().find(|(extensions, _)| extensions.contains(&extension)) {
        Some((_, action)) => Ok(action),
        None => anyhow::bail!("'{}' isn't a kind of file C++ rules compile", source.exec_path),
    }
}

/// "Compiling <source>", after the prefix if there is one.
fn progress_message(prefix: Option<&str>, source: &Artifact) -> String {
    let path = source.exec_path.strip_prefix(source.root()).unwrap_or(source.exec_path);
    match prefix.filter(|prefix| !prefix.is_empty()) {
        Some(prefix) => format!("{prefix}: Compiling {path}"),
        None => format!("Compiling {path}"),
    }
}

/// The environment of C++ actions: the default shell environment, and the
/// toolchain's for `action`.
pub fn action_env(state: &RuleState, configuration: &FeatureConfiguration, action: &str, variables: &Variables) -> anyhow::Result<Vec<(String, String)>> {
    let mut env: BTreeMap<String, String> = state.config.options.action_env.iter().cloned().collect();
    env.insert("PWD".to_string(), "/proc/self/cwd".to_string());
    env.extend(configuration.environment(action, variables)?);
    Ok(env.into_iter().collect())
}
