//! The build's status files, `ctx.info_file` and `ctx.version_file`, which
//! stamped actions read.
//!
//! bb doesn't run `--workspace_status_command` yet, so the files hold
//! fixed values, which keeps actions that read them cacheable.

use crate::action::{Action, ActionKind};
use crate::artifact::Artifact;
use bazel_config::BuildConfiguration;
use bazel_label::Label;
use intern::{PathId, Symbol};
use std::sync::LazyLock;

/// The target that owns the status files: one in each configuration.
pub static OWNER: LazyLock<Label> = LazyLock::new(|| Label::parse_canonical("@@bazel_tools//tools:workspace_status").expect("a valid label"));

const STABLE: &str = "stable-status.txt";
const VOLATILE: &str = "volatile-status.txt";

/// The stable status, which Bazel reruns actions for when it changes.
pub fn info_file(config: &BuildConfiguration) -> Artifact {
    artifact(config, STABLE)
}

/// The volatile status, such as the time, which doesn't rerun actions.
pub fn version_file(config: &BuildConfiguration) -> Artifact {
    artifact(config, VOLATILE)
}

/// The actions that write the status files.
pub fn actions(config: &BuildConfiguration) -> Vec<Action> {
    let stable = "BUILD_EMBED_LABEL \nBUILD_HOST bb\nBUILD_USER bb\n";
    let volatile = "BUILD_TIMESTAMP 0\n";
    vec![write(info_file(config), stable), write(version_file(config), volatile)]
}

fn artifact(config: &BuildConfiguration, name: &str) -> Artifact {
    let name = PathId::parse(name).expect("a valid file name");
    Artifact::derived(*OWNER, config, name, false)
}

fn write(output: Artifact, content: &str) -> Action {
    Action {
        mnemonic: Symbol::intern("WorkspaceStatus"),
        progress_message: String::new(),
        inputs: Vec::new(),
        runfiles: Vec::new(),
        outputs: vec![output],
        kind: ActionKind::Write {
            content: content.to_string(),
            executable: false,
        },
    }
}
