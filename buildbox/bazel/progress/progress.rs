//! What a workspace's evaluations are doing as they do it: packages loaded,
//! targets configured and actions run, and what actions printed or why they
//! failed. Commands show it as the progress bar, messages and the build
//! summary.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

/// A graph service that the functions loading packages, configuring targets
/// and running actions report to. Commands [`track`](Progress::track) it
/// while they run.
#[derive(Default)]
pub struct Progress {
    trackers: Mutex<Vec<Weak<Tracker>>>,
    next_action: AtomicU64,
}

impl Progress {
    pub fn new() -> Arc<Progress> {
        Arc::new(Progress::default())
    }

    /// Record what happens from now until the tracker is dropped.
    pub fn track(&self) -> Arc<Tracker> {
        let tracker = Arc::new(Tracker::default());
        let mut trackers = lock(&self.trackers);
        trackers.retain(|tracker| tracker.strong_count() > 0);
        trackers.push(Arc::downgrade(&tracker));
        tracker
    }

    /// A package was loaded.
    pub fn package_loaded(&self) {
        self.update(|state| state.packages_loaded += 1);
    }

    /// A target was configured.
    pub fn target_configured(&self) {
        self.update(|state| state.targets_configured += 1);
    }

    /// An aspect was applied to a target.
    pub fn aspect_applied(&self) {
        self.update(|state| state.aspects_applied += 1);
    }

    /// An action whose inputs are ready has started. It's shown as running
    /// until the handle returned is finished or dropped. `key` identifies
    /// the action, and `inputs` the actions that generated its inputs, for
    /// finding the critical path.
    pub fn action_started(self: &Arc<Self>, key: u64, inputs: Vec<u64>, description: String) -> ActionProgress {
        let id = self.next_action.fetch_add(1, Ordering::Relaxed);
        let action = RunningAction {
            description,
            started: Instant::now(),
            runner: None,
        };
        self.update(|state| {
            state.actions_started += 1;
            state.running.insert(id, action.clone());
        });
        ActionProgress {
            progress: self.clone(),
            id,
            key,
            inputs,
            started: action.started,
            finished: false,
        }
    }

    /// Something happened that whoever is tracking should hear about now.
    pub fn report(&self, event: Event) {
        for tracker in self.trackers() {
            lock(&tracker.events).push(event.clone());
        }
    }

    fn update(&self, change: impl Fn(&mut Snapshot)) {
        for tracker in self.trackers() {
            change(&mut lock(&tracker.state));
        }
    }

    fn trackers(&self) -> Vec<Arc<Tracker>> {
        lock(&self.trackers).iter().filter_map(Weak::upgrade).collect()
    }
}

/// An action that has started, which reports how it goes.
pub struct ActionProgress {
    progress: Arc<Progress>,
    id: u64,
    key: u64,
    inputs: Vec<u64>,
    started: Instant,
    finished: bool,
}

impl ActionProgress {
    /// The action is running with `runner`, such as `linux-sandbox`.
    pub fn running(&self, runner: &str) {
        self.progress.update(|state| {
            if let Some(action) = state.running.get_mut(&self.id) {
                action.runner = Some(runner.to_string());
            }
        });
    }

    /// The action finished `how`: the runner that ran it, a cache hit such
    /// as `disk cache hit`, or `internal` for an action without a process.
    pub fn finish(mut self, how: &str) {
        self.finished = true;
        let took = self.started.elapsed();
        for tracker in self.progress.trackers() {
            let path = tracker.add_to_paths(self.key, &self.inputs, took);
            let mut state = lock(&tracker.state);
            state.running.remove(&self.id);
            state.actions_finished += 1;
            *state.processes.entry(how.to_string()).or_default() += 1;
            state.critical_path = state.critical_path.max(path);
        }
    }
}

/// An action that stops without finishing, because it failed or was
/// cancelled, is no longer running, but isn't counted as done.
impl Drop for ActionProgress {
    fn drop(&mut self) {
        if !self.finished {
            self.progress.update(|state| {
                state.running.remove(&self.id);
            });
        }
    }
}

/// What has happened since a command started tracking [`Progress`].
#[derive(Default)]
pub struct Tracker {
    state: Mutex<Snapshot>,
    /// The longest chain of actions ending at each action that finished,
    /// by key.
    paths: Mutex<HashMap<u64, Duration>>,
    /// Events reported and not yet taken.
    events: Mutex<Vec<Event>>,
}

impl Tracker {
    /// What has happened so far.
    pub fn snapshot(&self) -> Snapshot {
        lock(&self.state).clone()
    }

    /// The events reported since they were last taken, in order.
    pub fn take_events(&self) -> Vec<Event> {
        std::mem::take(&mut *lock(&self.events))
    }

    /// Record that the action `key` took `took`, after the actions that
    /// generated its inputs, returning the longest chain it ends.
    fn add_to_paths(&self, key: u64, inputs: &[u64], took: Duration) -> Duration {
        let mut paths = lock(&self.paths);
        let before = inputs.iter().filter_map(|input| paths.get(input)).max().copied().unwrap_or_default();
        let path = before + took;
        paths.insert(key, path);
        path
    }
}

/// What a [`Tracker`] has recorded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub packages_loaded: u64,
    pub targets_configured: u64,
    pub aspects_applied: u64,
    pub actions_started: u64,
    pub actions_finished: u64,
    /// The actions running, in the order they started.
    pub running: BTreeMap<u64, RunningAction>,
    /// How many actions finished each way: by runner, cache, or `internal`.
    pub processes: BTreeMap<String, u64>,
    /// The longest chain of actions that finished, each run after the
    /// actions generating its inputs: what a build can take at least.
    pub critical_path: Duration,
}

/// An action that's running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningAction {
    /// What it's doing, such as `Compiling foo.cc`.
    pub description: String,
    pub started: Instant,
    /// What it's running with, once it has started to run.
    pub runner: Option<String>,
}

/// Something that happened that a command tells its user about as it
/// happens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// An action succeeded, printing something.
    ActionOutput(ActionOutput),
    /// An action failed.
    ActionFailed(Arc<ActionFailure>),
    /// Something else went wrong, as an error message.
    Error(String),
    /// Starlark code printed something: `file:line:column: text`.
    Debug(String),
}

/// What an action printed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionOutput {
    /// What the action does, such as `Compiling a.cc`.
    pub description: String,
    pub stdout: String,
    pub stderr: String,
}

/// Why an action failed, and what it printed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionFailure {
    /// The message of the error the action failed with, which identifies
    /// the failure once it's been reported.
    pub error: String,
    /// Where the action's target was declared: `BUILD.bazel:line:column`.
    pub location: Option<String>,
    pub description: String,
    /// What the action is for, as Bazel describes it:
    /// `<kind> rule target //pkg:name`, after the aspects whose action it
    /// is, if any.
    pub owner: String,
    pub mnemonic: String,
    pub reason: FailureReason,
    pub output: ActionOutput,
}

/// How an action failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailureReason {
    /// Its command exited with a code other than 0.
    Exit {
        code: i32,
        command: Command,
        /// Whether it ran in a sandbox on this machine.
        sandboxed: bool,
    },
    /// Its command didn't create these outputs, by path from their root.
    MissingOutputs(Vec<String>),
    /// These source files it reads don't exist, by label.
    MissingInputs(Vec<String>),
}

/// A command an action ran, as a failure describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Command {
    pub arguments: Vec<String>,
    /// Sorted by name.
    pub env: Vec<(String, String)>,
    /// The checksum of the action's configuration.
    pub configuration: String,
    pub execution_platform: String,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trackers_see_what_happens_while_they_are_kept() {
        let progress = Progress::new();
        progress.package_loaded();
        let tracker = progress.track();
        progress.package_loaded();
        progress.target_configured();
        let action = progress.action_started(1, Vec::new(), "Compiling a.cc".to_string());
        action.running("linux-sandbox");

        let running = tracker.snapshot();
        assert_eq!(running.packages_loaded, 1);
        assert_eq!(running.targets_configured, 1);
        let shown: Vec<_> = running.running.values().map(|action| (action.description.as_str(), action.runner.as_deref())).collect();
        assert_eq!(shown, [("Compiling a.cc", Some("linux-sandbox"))]);

        action.finish("linux-sandbox");
        let done = tracker.snapshot();
        assert!(done.running.is_empty());
        assert_eq!((done.actions_started, done.actions_finished), (1, 1));
        assert_eq!(done.processes.get("linux-sandbox"), Some(&1));
    }

    #[test]
    fn dropped_actions_stop_running_without_finishing() {
        let progress = Progress::new();
        let tracker = progress.track();
        drop(progress.action_started(1, Vec::new(), "Linking a".to_string()));
        let snapshot = tracker.snapshot();
        assert!(snapshot.running.is_empty());
        assert_eq!((snapshot.actions_started, snapshot.actions_finished), (1, 0));
    }

    #[test]
    fn the_critical_path_follows_inputs() {
        let progress = Progress::new();
        let tracker = progress.track();
        let compile = progress.action_started(1, Vec::new(), "Compiling a.cc".to_string());
        std::thread::sleep(Duration::from_millis(20));
        compile.finish("linux-sandbox");
        let link = progress.action_started(3, vec![1], "Linking a".to_string());
        std::thread::sleep(Duration::from_millis(20));
        link.finish("linux-sandbox");
        let critical = tracker.snapshot().critical_path;
        assert!(critical >= Duration::from_millis(40), "{critical:?}");
    }

    #[test]
    fn trackers_take_the_events_reported_to_them() {
        let progress = Progress::new();
        let tracker = progress.track();
        let output = ActionOutput {
            description: "Compiling a.cc".to_string(),
            stderr: "warning: unused".to_string(),
            ..ActionOutput::default()
        };
        progress.report(Event::ActionOutput(output.clone()));
        assert_eq!(tracker.take_events(), [Event::ActionOutput(output)]);
        assert!(tracker.take_events().is_empty());
    }

    #[test]
    fn dropped_trackers_are_forgotten() {
        let progress = Progress::new();
        drop(progress.track());
        let tracker = progress.track();
        progress.package_loaded();
        assert_eq!(lock(&progress.trackers).len(), 1);
        assert_eq!(tracker.snapshot().packages_loaded, 1);
    }
}
