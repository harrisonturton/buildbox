//! The build's status files, `ctx.info_file` and `ctx.version_file`, which
//! stamped actions read.
//!
//! The files hold what `--workspace_status_command` prints, with
//! `--embed_label`, and fixed values for the host, user and time, which
//! keep actions that read them cacheable.

use crate::inputs::InputSet;
use crate::action::{Action, ActionKind};
use crate::artifact::Artifact;
use bazel_label::Label;
use intern::{PathId, Symbol};
use skycodec::{Decode, Encode};
use skyframe::{Ctx, Key, SkyError, encoded_value};
use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex};

/// The target that owns the status files. As in Bazel, there's one of each
/// file for the whole build, outside every configuration's directory, so
/// the target is analyzed in [`bazel_config::no_config`].
pub static OWNER: LazyLock<Label> = LazyLock::new(|| Label::parse_canonical("@@bazel_tools//tools:workspace_status").expect("a valid label"));

const STABLE: &str = "bazel-out/stable-status.txt";
const VOLATILE: &str = "bazel-out/volatile-status.txt";

/// The values of the stable status, which Bazel reruns actions for when
/// they change.
pub const STABLE_STATUS: [(&str, &str); 3] = [("BUILD_EMBED_LABEL", ""), ("BUILD_HOST", "bb"), ("BUILD_USER", "bb")];

/// The values of the volatile status, the time, which don't rerun actions:
/// fixed at the epoch, as Bazel formats it.
pub const VOLATILE_STATUS: [(&str, &str); 2] = [("BUILD_TIMESTAMP", "0"), ("FORMATTED_DATE", "1970 Jan 01 00 00 00 Thu")];

/// The values of the status files, each sorted by key.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct StatusValues {
    pub stable: Vec<(String, String)>,
    pub volatile: Vec<(String, String)>,
}

encoded_value!(StatusValues);

impl Default for StatusValues {
    fn default() -> StatusValues {
        StatusValues::new("", "")
    }
}

impl StatusValues {
    /// The values for a build with `--embed_label=embed_label`, whose
    /// `--workspace_status_command` printed `output`: lines of a key, a
    /// space and a value. Keys starting `STABLE_` are stable, as Bazel
    /// has them, and others volatile.
    pub fn new(embed_label: &str, output: &str) -> StatusValues {
        let mut stable: BTreeMap<String, String> = STABLE_STATUS.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect();
        stable.insert("BUILD_EMBED_LABEL".to_string(), embed_label.to_string());
        let mut volatile: BTreeMap<String, String> = VOLATILE_STATUS.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect();
        for line in output.trim().lines() {
            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            let entry = (key.to_string(), value.trim().to_string());
            match is_stable(key) {
                true => stable.insert(entry.0, entry.1),
                false => volatile.insert(entry.0, entry.1),
            };
        }
        StatusValues {
            stable: stable.into_iter().collect(),
            volatile: volatile.into_iter().collect(),
        }
    }
}

fn is_stable(key: &str) -> bool {
    key.starts_with("STABLE_") || STABLE_STATUS.iter().any(|(stable, _)| *stable == key)
}

/// The status values of a workspace's latest build, which its graph's
/// [`WorkspaceStatusKey`] reads.
#[derive(Debug, Default)]
pub struct BuildStatus(Mutex<StatusValues>);

impl BuildStatus {
    pub fn new() -> Arc<BuildStatus> {
        Arc::default()
    }

    /// Set the values, returning whether they changed, in which case
    /// [`WorkspaceStatusKey`] must be invalidated.
    pub fn set(&self, values: StatusValues) -> bool {
        let mut current = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = *current != values;
        *current = values;
        changed
    }

    fn current(&self) -> StatusValues {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

/// The values the status files hold, as the latest build set them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Encode, Decode)]
pub struct WorkspaceStatusKey;

impl Key for WorkspaceStatusKey {
    type Value = StatusValues;
    const KIND: &'static str = "WORKSPACE_STATUS";
    const EXTERNAL: bool = true;

    async fn compute(&self, ctx: &Ctx) -> Result<StatusValues, SkyError> {
        Ok(ctx.global::<BuildStatus>()?.current())
    }
}

/// The stable status file.
pub fn info_file() -> Artifact {
    artifact(STABLE)
}

/// The volatile status file.
pub fn version_file() -> Artifact {
    artifact(VOLATILE)
}

/// The actions that write the status files.
pub fn actions(values: &StatusValues) -> Vec<Action> {
    vec![write(info_file(), &values.stable), write(version_file(), &values.volatile)]
}

fn artifact(path: &str) -> Artifact {
    Artifact {
        exec_path: PathId::parse(path).expect("a valid path"),
        owner: *OWNER,
        config: Some(bazel_config::no_config()),
        is_directory: false,
    }
}

fn write(output: Artifact, status: &[(String, String)]) -> Action {
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
            args: None,
        },
    }
}
