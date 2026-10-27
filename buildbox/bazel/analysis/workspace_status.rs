//! The build's status files, `ctx.info_file` and `ctx.version_file`, which
//! stamped actions read.
//!
//! bb doesn't run `--workspace_status_command` yet, so the files hold
//! fixed values, which keeps actions that read them cacheable.

use crate::inputs::InputSet;
use crate::action::{Action, ActionKind};
use crate::artifact::Artifact;
use bazel_label::Label;
use intern::{PathId, Symbol};
use std::sync::LazyLock;

/// The target that owns the status files. As in Bazel, there's one of each
/// file for the whole build, outside every configuration's directory, so
/// the target is analyzed in [`bazel_config::no_config`].
pub static OWNER: LazyLock<Label> = LazyLock::new(|| Label::parse_canonical("@@bazel_tools//tools:workspace_status").expect("a valid label"));

const STABLE: &str = "bazel-out/stable-status.txt";
const VOLATILE: &str = "bazel-out/volatile-status.txt";

/// The values of the stable status, which Bazel reruns actions for when
/// they change.
pub const STABLE_STATUS: [(&str, &str); 3] = [("BUILD_EMBED_LABEL", ""), ("BUILD_HOST", "bb"), ("BUILD_USER", "bb")];

/// The values of the volatile status, such as the time, which don't rerun
/// actions.
pub const VOLATILE_STATUS: [(&str, &str); 3] = [("BUILD_TIMESTAMP", "0"), ("BUILD_SCM_REVISION", "0"), ("BUILD_SCM_STATUS", "")];

/// The stable status file.
pub fn info_file() -> Artifact {
    artifact(STABLE)
}

/// The volatile status file.
pub fn version_file() -> Artifact {
    artifact(VOLATILE)
}

/// The actions that write the status files.
pub fn actions() -> Vec<Action> {
    vec![write(info_file(), &STABLE_STATUS), write(version_file(), &VOLATILE_STATUS[..1])]
}

fn artifact(path: &str) -> Artifact {
    Artifact {
        exec_path: PathId::parse(path).expect("a valid path"),
        owner: *OWNER,
        config: Some(bazel_config::no_config()),
        is_directory: false,
    }
}

fn write(output: Artifact, status: &[(&str, &str)]) -> Action {
    let content: String = status.iter().map(|(key, value)| format!("{key} {value}\n")).collect();
    Action {
        mnemonic: Symbol::intern("WorkspaceStatus"),
        progress_message: String::new(),
        inputs: InputSet::default(),
        runfiles: Vec::new(),
        outputs: vec![output],
        kind: ActionKind::Write {
            content,
            executable: false,
            from_args: false,
        },
    }
}
