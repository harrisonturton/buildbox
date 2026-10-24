//! The lines Bazel prints about a command once a phase of it is done.

use crate::bar::analysis_counts;
use crate::format_count;
use bazel_progress::Snapshot;

/// Runners reported before the others, in this order, as Bazel does.
const REPORTED_FIRST: [&str; 3] = ["disk cache hit", "remote cache hit", "internal"];

/// `Analyzed target //pkg:b (6 packages loaded, 9 targets configured).`,
/// where `what` is `target //pkg:b` or `3 targets`.
pub fn analyzed(what: &str, snapshot: &Snapshot) -> String {
    format!("Analyzed {what} ({}).", analysis_counts(snapshot))
}

/// How the actions that finished got their results:
/// `3 processes: 1 internal, 2 linux-sandbox.`
pub fn processes(snapshot: &Snapshot) -> String {
    let total = snapshot.actions_finished;
    if total == 0 {
        return "0 processes.".to_string();
    }
    let noun = match total {
        1 => "process",
        _ => "processes",
    };
    let first = REPORTED_FIRST.iter().filter_map(|name| Some((*name, *snapshot.processes.get(*name)?)));
    let rest = snapshot.processes.iter().filter(|(name, _)| !REPORTED_FIRST.contains(&name.as_str()));
    let parts: Vec<String> = first
        .chain(rest.map(|(name, count)| (name.as_str(), *count)))
        .map(|(name, count)| format!("{} {name}", format_count(count)))
        .collect();
    format!("{} {noun}: {}.", format_count(total), parts.join(", "))
}

/// `3 total actions`, counting the actions that finished.
pub fn total_actions(snapshot: &Snapshot) -> String {
    let total = snapshot.actions_finished;
    let noun = match total {
        1 => "action",
        _ => "actions",
    };
    format!("{} total {noun}", format_count(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished(processes: &[(&str, u64)]) -> Snapshot {
        Snapshot {
            actions_finished: processes.iter().map(|(_, count)| count).sum(),
            processes: processes.iter().map(|(name, count)| (name.to_string(), *count)).collect(),
            ..Snapshot::default()
        }
    }

    #[test]
    fn processes_are_summarised_as_bazel_does() {
        assert_eq!(processes(&finished(&[])), "0 processes.");
        assert_eq!(processes(&finished(&[("internal", 1)])), "1 process: 1 internal.");
        assert_eq!(
            processes(&finished(&[("linux-sandbox", 2), ("internal", 1), ("disk cache hit", 4), ("remote", 1)])),
            "8 processes: 4 disk cache hit, 1 internal, 2 linux-sandbox, 1 remote."
        );
    }

    #[test]
    fn totals_and_analysis_are_counted() {
        assert_eq!(total_actions(&finished(&[("internal", 1)])), "1 total action");
        assert_eq!(total_actions(&finished(&[("internal", 3)])), "3 total actions");
        let snapshot = Snapshot {
            packages_loaded: 6,
            targets_configured: 9,
            ..Snapshot::default()
        };
        assert_eq!(analyzed("target //pkg:b", &snapshot), "Analyzed target //pkg:b (6 packages loaded, 9 targets configured).");
    }
}
