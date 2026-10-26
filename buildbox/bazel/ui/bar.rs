//! The progress bar: what a command is doing, from the phase it's in and
//! what its tracker has seen, in Bazel's words.

use crate::{GREEN, UiOptions, colored, format_count};
use bazel_progress::{RunningAction, Snapshot};
use std::time::Instant;

/// The phase a command is in, which decides what its progress bar shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// Nothing to show.
    #[default]
    Idle,
    RepoMapping,
    Loading,
    /// Analyzing what's named: `target //pkg:name`, or `3 targets`.
    Analyzing(String),
    Executing,
}

/// How long an action runs before the progress bar shows its time, unless
/// it shows its runner, and so its time, anyway.
const SHOW_TIME_AFTER_SECONDS: u64 = 3;

/// Appended to the last action listed when there are more.
const AND_MORE: &str = " ...";

/// The progress bar's lines, without line ends: one line unless the bar is
/// redrawn in place. With `now` as `None`, times are left out, which tells
/// whether anything but the time has changed.
pub fn progress_bar(phase: &Phase, snapshot: &Snapshot, now: Option<Instant>, options: &UiOptions) -> Vec<String> {
    let width = options.columns;
    match phase {
        Phase::Idle => Vec::new(),
        Phase::RepoMapping => vec![status_line(options, "Computing main repo mapping:", "", width)],
        Phase::Loading => {
            let loaded = format!("{} packages loaded", format_count(snapshot.packages_loaded));
            vec![status_line(options, "Loading:", &loaded, width)]
        }
        Phase::Analyzing(what) => {
            let progress = format!("{what} ({})", analysis_counts(snapshot));
            vec![status_line(options, "Analyzing:", &progress, width)]
        }
        Phase::Executing => execution_lines(snapshot, now, options),
    }
}

/// `packages loaded, targets configured`, and aspect applications if
/// there were any, as the analysis phase shows them.
pub(crate) fn analysis_counts(snapshot: &Snapshot) -> String {
    let targets = snapshot.targets_configured;
    let noun = match targets {
        1 => "target",
        _ => "targets",
    };
    let counts = format!("{} packages loaded, {} {noun} configured", format_count(snapshot.packages_loaded), format_count(targets));
    let aspects = snapshot.aspects_applied;
    match aspects {
        0 => counts,
        1 => format!("{counts}, 1 aspect application"),
        _ => format!("{counts}, {} aspect applications", format_count(aspects)),
    }
}

/// `Status: message`, with the status in green, cut to `width`.
fn status_line(options: &UiOptions, status: &str, message: &str, width: usize) -> String {
    let message = truncate(message, width.saturating_sub(status.chars().count() + 1));
    format!("{} {message}", colored(options, GREEN, status))
}

fn execution_lines(snapshot: &Snapshot, now: Option<Instant>, options: &UiOptions) -> Vec<String> {
    let counts = format!("[{} / {}]", format_count(snapshot.actions_finished), format_count(snapshot.actions_started));
    let prefix = colored(options, GREEN, &counts);
    let rest = options.columns.saturating_sub(counts.len() + 1);
    let running = oldest_first(snapshot);
    match running.as_slice() {
        [] => vec![format!("{prefix} no actions running")],
        [only] => vec![format!("{prefix} {}", describe(only, now, rest))],
        [oldest, ..] if !options.cursor_control => {
            let more = format!(" ... ({})", count_actions(&running));
            let description = describe(oldest, now, rest.saturating_sub(more.len()));
            vec![format!("{prefix} {description}{more}")]
        }
        _ => sampled_lines(prefix, &running, now, options),
    }
}

/// The bar redrawn in place with several actions running: their count, then
/// the oldest few, one per line.
fn sampled_lines(prefix: String, running: &[&RunningAction], now: Option<Instant>, options: &UiOptions) -> Vec<String> {
    let columns = options.columns;
    let mut lines = vec![format!("{prefix} {}", count_actions(running))];
    let shown = running.len().min(options.actions_shown.max(1));
    for (index, action) in running.iter().take(shown).enumerate() {
        let last_of_more = index + 1 == shown && shown < running.len();
        let suffix = if last_of_more { AND_MORE } else { "" };
        let width = columns.saturating_sub(4 + suffix.len());
        lines.push(format!("    {}{suffix}", describe(action, now, width)));
    }
    lines
}

/// The running actions, those that are running before those waiting to,
/// each oldest first.
fn oldest_first(snapshot: &Snapshot) -> Vec<&RunningAction> {
    let mut running: Vec<&RunningAction> = snapshot.running.values().collect();
    running.sort_by_key(|action| (action.runner.is_none(), action.started));
    running
}

/// How many actions are running, as Bazel words it.
fn count_actions(running: &[&RunningAction]) -> String {
    let executing = running.iter().filter(|action| action.runner.is_some()).count();
    match running.len() {
        1 => " 1 action running".to_string(),
        count if count == executing => format!("{count} actions running"),
        count => format!("{count} actions, {executing} running"),
    }
}

/// An action as the progress bar shows it: what it's doing, how long it's
/// been running and with what, in at most `width` characters.
fn describe(action: &RunningAction, now: Option<Instant>, width: usize) -> String {
    let prefix = match action.runner {
        Some(_) => "",
        None => "[Sched] ",
    };
    let seconds = now.map(|now| now.saturating_duration_since(action.started).as_secs());
    let mut postfix = String::new();
    if let Some(seconds) = seconds.filter(|seconds| action.runner.is_some() || *seconds > SHOW_TIME_AFTER_SECONDS) {
        postfix = format!("; {seconds}s");
    }
    if let Some(runner) = &action.runner {
        postfix.push(' ');
        postfix.push_str(runner);
    }
    let room = width.saturating_sub(prefix.len() + postfix.len());
    format!("{prefix}{}{postfix}", truncate(&action.description, room))
}

/// `text` cut to at most `width` characters, ending in `...` if it was cut.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(3)).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn plain(cursor_control: bool) -> UiOptions {
        UiOptions {
            color: false,
            cursor_control,
            show_progress: true,
            columns: 80,
            actions_shown: 3,
        }
    }

    fn action(description: &str, started: Instant, runner: Option<&str>) -> RunningAction {
        RunningAction {
            description: description.to_string(),
            started,
            runner: runner.map(str::to_string),
        }
    }

    fn snapshot(running: Vec<RunningAction>) -> Snapshot {
        Snapshot {
            actions_started: 3,
            actions_finished: 1,
            running: running.into_iter().enumerate().map(|(id, action)| (id as u64, action)).collect(),
            ..Snapshot::default()
        }
    }

    #[test]
    fn loading_and_analysis_show_counts() {
        let snapshot = Snapshot {
            packages_loaded: 6,
            targets_configured: 1,
            ..Snapshot::default()
        };
        let options = plain(true);
        assert_eq!(progress_bar(&Phase::Loading, &snapshot, None, &options), ["Loading: 6 packages loaded"]);
        let analyzing = Phase::Analyzing("target //pkg:b".to_string());
        assert_eq!(
            progress_bar(&analyzing, &snapshot, None, &options),
            ["Analyzing: target //pkg:b (6 packages loaded, 1 target configured)"]
        );
    }

    #[test]
    fn one_running_action_is_shown_with_its_time_and_runner() {
        let start = Instant::now();
        let snapshot = snapshot(vec![action("Executing genrule //pkg:a", start, Some("linux-sandbox"))]);
        let now = Some(start + Duration::from_secs(1));
        assert_eq!(
            progress_bar(&Phase::Executing, &snapshot, now, &plain(true)),
            ["[1 / 3] Executing genrule //pkg:a; 1s linux-sandbox"]
        );
        assert_eq!(
            progress_bar(&Phase::Executing, &snapshot, None, &plain(true)),
            ["[1 / 3] Executing genrule //pkg:a linux-sandbox"]
        );
    }

    #[test]
    fn several_actions_are_listed_when_redrawn_in_place() {
        let start = Instant::now();
        let running = vec![
            action("Compiling a.cc", start, Some("linux-sandbox")),
            action("Compiling b.cc", start + Duration::from_secs(1), Some("linux-sandbox")),
            action("Compiling c.cc", start, None),
            action("Compiling d.cc", start, Some("linux-sandbox")),
        ];
        let now = Some(start + Duration::from_secs(2));
        let lines = progress_bar(&Phase::Executing, &snapshot(running.clone()), now, &plain(true));
        assert_eq!(
            lines,
            [
                "[1 / 3] 4 actions, 3 running",
                "    Compiling a.cc; 2s linux-sandbox",
                "    Compiling d.cc; 2s linux-sandbox",
                "    Compiling b.cc; 1s linux-sandbox ...",
            ]
        );
        let line = progress_bar(&Phase::Executing, &snapshot(running), now, &plain(false));
        assert_eq!(line, ["[1 / 3] Compiling a.cc; 2s linux-sandbox ... (4 actions, 3 running)"]);
    }

    #[test]
    fn as_many_actions_are_listed_as_ui_actions_shown_says() {
        let start = Instant::now();
        let running: Vec<_> = (0..10).map(|index| action(&format!("Compiling {index}.cc"), start, Some("linux-sandbox"))).collect();
        let mut options = plain(true);
        options.actions_shown = 8;
        let lines = progress_bar(&Phase::Executing, &snapshot(running.clone()), None, &options);
        assert_eq!(lines.len(), 1 + 8);
        assert!(lines[8].ends_with(" ..."));
        options.actions_shown = 0;
        let lines = progress_bar(&Phase::Executing, &snapshot(running), None, &options);
        assert_eq!(lines.len(), 1 + 1);
    }

    #[test]
    fn long_lines_are_cut_to_the_terminal() {
        let start = Instant::now();
        let description = format!("Compiling {}", "x".repeat(200));
        let snapshot = snapshot(vec![action(&description, start, Some("linux-sandbox"))]);
        let lines = progress_bar(&Phase::Executing, &snapshot, Some(start), &plain(true));
        assert_eq!(lines[0].chars().count(), 80);
        assert!(lines[0].ends_with("...; 0s linux-sandbox"));
    }
}
