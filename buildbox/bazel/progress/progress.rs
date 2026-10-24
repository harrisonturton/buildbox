//! What a workspace's evaluations are doing as they do it: packages loaded,
//! targets configured and actions run. Commands show it as Bazel's progress
//! bar and build summary.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Instant;

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

    /// An action whose inputs are ready has started. It's shown as running
    /// until the handle returned is finished or dropped.
    pub fn action_started(self: &Arc<Self>, description: String) -> ActionProgress {
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
            finished: false,
        }
    }

    fn update(&self, change: impl Fn(&mut Snapshot)) {
        let trackers: Vec<Arc<Tracker>> = lock(&self.trackers).iter().filter_map(Weak::upgrade).collect();
        for tracker in trackers {
            change(&mut lock(&tracker.state));
        }
    }
}

/// An action that has started, which reports how it goes.
pub struct ActionProgress {
    progress: Arc<Progress>,
    id: u64,
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
        self.progress.update(|state| {
            state.running.remove(&self.id);
            state.actions_finished += 1;
            *state.processes.entry(how.to_string()).or_default() += 1;
        });
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
}

impl Tracker {
    /// What has happened so far.
    pub fn snapshot(&self) -> Snapshot {
        lock(&self.state).clone()
    }
}

/// What a [`Tracker`] has recorded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub packages_loaded: u64,
    pub targets_configured: u64,
    pub actions_started: u64,
    pub actions_finished: u64,
    /// The actions running, in the order they started.
    pub running: BTreeMap<u64, RunningAction>,
    /// How many actions finished each way: by runner, cache, or `internal`.
    pub processes: BTreeMap<String, u64>,
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
        let action = progress.action_started("Compiling a.cc".to_string());
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
        drop(progress.action_started("Linking a".to_string()));
        let snapshot = tracker.snapshot();
        assert!(snapshot.running.is_empty());
        assert_eq!((snapshot.actions_started, snapshot.actions_finished), (1, 0));
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
