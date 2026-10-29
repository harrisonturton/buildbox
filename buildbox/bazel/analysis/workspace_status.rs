//! The build's status files, `ctx.info_file` and `ctx.version_file`, which
//! stamped actions read.
//!
//! The files hold what `--workspace_status_command` prints, with
//! `--embed_label`, the host, the user and the time. As in Bazel, the
//! volatile file, with the time, doesn't make the actions that read it run
//! again: the local action cache leaves its contents out.

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

/// The keys of the stable status that every build has, which Bazel reruns
/// actions for when they change.
const STABLE_KEYS: [&str; 3] = ["BUILD_EMBED_LABEL", "BUILD_HOST", "BUILD_USER"];

/// The stable values `transform_info_file`'s function sees. bb runs it in
/// analysis, which mustn't depend on the build's status, so it sees fixed
/// values rather than the build's.
pub const TRANSLATED_STABLE: [(&str, &str); 3] = [("BUILD_EMBED_LABEL", ""), ("BUILD_HOST", "bb"), ("BUILD_USER", "bb")];

/// The volatile values `transform_version_file`'s function sees: the
/// epoch, as Bazel formats it.
pub const TRANSLATED_VOLATILE: [(&str, &str); 2] = [("BUILD_TIMESTAMP", "0"), ("FORMATTED_DATE", "1970 Jan 01 00 00 00 Thu")];

/// The path of the volatile status file, relative to the execution root,
/// whose contents don't make the actions that read it run again.
pub const VOLATILE_PATH: &str = VOLATILE;

/// The values of the status files, each sorted by key.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct StatusValues {
    pub stable: Vec<(String, String)>,
    pub volatile: Vec<(String, String)>,
}

encoded_value!(StatusValues);

impl Default for StatusValues {
    fn default() -> StatusValues {
        StatusValues::new(&BuildInfo::default(), "")
    }
}

/// What every build's status says: who built it where and when, and its
/// `--embed_label`.
#[derive(Clone, Debug, Default)]
pub struct BuildInfo {
    pub embed_label: String,
    /// The machine's short host name.
    pub host: String,
    pub user: String,
    /// When the build started, in seconds since the epoch.
    pub timestamp: u64,
}

impl StatusValues {
    /// The values for a build of `info`, whose `--workspace_status_command`
    /// printed `output`: lines of a key, a space and a value. Keys starting
    /// `STABLE_` are stable, as Bazel has them, and others volatile.
    pub fn new(info: &BuildInfo, output: &str) -> StatusValues {
        let mut stable = BTreeMap::new();
        stable.insert("BUILD_EMBED_LABEL".to_string(), info.embed_label.clone());
        stable.insert("BUILD_HOST".to_string(), info.host.clone());
        stable.insert("BUILD_USER".to_string(), info.user.clone());
        let mut volatile = BTreeMap::new();
        volatile.insert("BUILD_TIMESTAMP".to_string(), info.timestamp.to_string());
        volatile.insert("FORMATTED_DATE".to_string(), formatted_date(info.timestamp));
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
    key.starts_with("STABLE_") || STABLE_KEYS.contains(&key)
}

/// A time, in seconds since the epoch, as Bazel's `FORMATTED_DATE` writes
/// it in UTC: `yyyy MMM dd HH mm ss EEE`, such as `2026 Oct 29 14 05 09 Thu`.
fn formatted_date(timestamp: u64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    let days = timestamp / 86_400;
    let seconds = timestamp % 86_400;
    let (year, month, day) = civil_date(days as i64);
    let weekday = WEEKDAYS[(days % 7) as usize];
    format!("{year} {} {day:02} {:02} {:02} {:02} {weekday}", MONTHS[(month - 1) as usize], seconds / 3600, seconds / 60 % 60, seconds % 60)
}

/// The year, month and day of a day counted from 1970-01-01, by Howard
/// Hinnant's `civil_from_days`.
fn civil_date(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dates_as_bazel_does() {
        assert_eq!(formatted_date(0), "1970 Jan 01 00 00 00 Thu");
        assert_eq!(formatted_date(951_782_400), "2000 Feb 29 00 00 00 Tue");
        assert_eq!(formatted_date(1_793_282_709), "2026 Oct 29 14 05 09 Thu");
    }
}
