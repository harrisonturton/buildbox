//! Limits how many actions run at once, and keeps track of them.

use async_trait::async_trait;
use cas::Digest;
use executor::{Action, ExecuteError, Executor};
use remote_execution_rust_connect::build::bazel::remote::execution::v2::ActionResult;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;
use tokio::sync::Semaphore;

/// An [`Executor`] that runs actions on another executor, at most
/// `max_concurrent` at a time. Actions beyond that wait in a queue.
pub struct Scheduler {
    executor: Arc<dyn Executor>,
    slots: Semaphore,
    max_concurrent: usize,
    tracker: Arc<Tracker>,
    next_id: AtomicU64,
}

/// A snapshot of an action the scheduler is handling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionInfo {
    /// Identifies the action while the scheduler is handling it.
    pub id: u64,
    pub digest: Digest,
    pub arguments: Vec<String>,
    pub state: ActionState,
    pub queued_at: SystemTime,
    pub started_at: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionState {
    /// Waiting for a free slot.
    Queued,
    Running,
}

impl Scheduler {
    pub fn new(executor: Arc<dyn Executor>, max_concurrent: usize) -> Scheduler {
        Scheduler {
            executor,
            slots: Semaphore::new(max_concurrent),
            max_concurrent,
            tracker: Arc::new(Tracker::default()),
            next_id: AtomicU64::new(0),
        }
    }

    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// The actions currently queued or running, oldest first.
    pub fn actions(&self) -> Vec<ActionInfo> {
        self.tracker.snapshot()
    }
}

#[async_trait]
impl Executor for Scheduler {
    async fn execute(&self, action: &Action) -> Result<ActionResult, ExecuteError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let entry = Entry::add(self.tracker.clone(), id, action);

        // The semaphore is never closed, so acquiring can't fail.
        let Ok(_slot) = self.slots.acquire().await else {
            return Err(ExecuteError::Internal("scheduler is shutting down".to_string()));
        };
        entry.mark_running();

        self.executor.execute(action).await
    }
}

/// The actions being handled, by ID.
#[derive(Default)]
struct Tracker {
    actions: Mutex<BTreeMap<u64, ActionInfo>>,
}

impl Tracker {
    fn snapshot(&self) -> Vec<ActionInfo> {
        self.lock().values().cloned().collect()
    }

    /// The map is never left inconsistent, so a panic while it was held
    /// doesn't make it unusable.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<u64, ActionInfo>> {
        self.actions.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// An action's entry in the tracker, removed when dropped. This covers every
/// way an action can end, including being cancelled.
struct Entry {
    tracker: Arc<Tracker>,
    id: u64,
}

impl Entry {
    fn add(tracker: Arc<Tracker>, id: u64, action: &Action) -> Entry {
        let info = ActionInfo {
            id,
            digest: action.digest.clone(),
            arguments: action.command.arguments.clone(),
            state: ActionState::Queued,
            queued_at: SystemTime::now(),
            started_at: None,
        };
        tracker.lock().insert(id, info);
        Entry { tracker, id }
    }

    fn mark_running(&self) {
        let mut actions = self.tracker.lock();
        if let Some(info) = actions.get_mut(&self.id) {
            info.state = ActionState::Running;
            info.started_at = Some(SystemTime::now());
        }
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.tracker.lock().remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remote_execution_rust_connect::build::bazel::remote::execution::v2::Command;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;
    use tokio::sync::Notify;

    /// An executor that blocks each action until released, and records how
    /// many ran at once.
    #[derive(Default)]
    struct BlockingExecutor {
        running: AtomicUsize,
        max_running: AtomicUsize,
        release: Notify,
    }

    #[async_trait]
    impl Executor for BlockingExecutor {
        async fn execute(&self, _action: &Action) -> Result<ActionResult, ExecuteError> {
            let running = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_running.fetch_max(running, Ordering::SeqCst);
            self.release.notified().await;
            self.running.fetch_sub(1, Ordering::SeqCst);
            Ok(ActionResult::default())
        }
    }

    fn action(name: &str) -> Action {
        Action {
            digest: Digest::of_bytes(name.as_bytes()),
            command: Command {
                arguments: vec![name.to_string()],
                ..Default::default()
            },
            input_root: Digest::empty(),
            timeout: None,
        }
    }

    /// Wait until `condition` holds, failing the test if it takes too long.
    async fn wait_until(condition: impl Fn() -> bool) {
        for _ in 0..500 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("condition never held");
    }

    fn count_in_state(scheduler: &Scheduler, state: ActionState) -> usize {
        scheduler.actions().iter().filter(|info| info.state == state).count()
    }

    #[tokio::test]
    async fn limits_concurrent_actions_and_tracks_them() {
        let executor = Arc::new(BlockingExecutor::default());
        let scheduler = Arc::new(Scheduler::new(executor.clone(), 2));

        let mut tasks = vec![];
        for i in 0..5 {
            let scheduler = scheduler.clone();
            tasks.push(tokio::spawn(async move { scheduler.execute(&action(&format!("a{i}"))).await }));
        }

        wait_until(|| scheduler.actions().len() == 5 && count_in_state(&scheduler, ActionState::Running) == 2).await;
        assert_eq!(count_in_state(&scheduler, ActionState::Queued), 3);

        // Release actions until all have run.
        while !tasks.iter().all(|task| task.is_finished()) {
            executor.release.notify_waiters();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }

        assert_eq!(executor.max_running.load(Ordering::SeqCst), 2);
        assert!(scheduler.actions().is_empty());
    }

    #[tokio::test]
    async fn records_details_of_running_actions() {
        let executor = Arc::new(BlockingExecutor::default());
        let scheduler = Arc::new(Scheduler::new(executor.clone(), 1));

        let task = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.execute(&action("compile")).await })
        };
        wait_until(|| count_in_state(&scheduler, ActionState::Running) == 1).await;

        let info = &scheduler.actions()[0];
        assert_eq!(info.digest, Digest::of_bytes(b"compile"));
        assert_eq!(info.arguments, ["compile"]);
        assert!(info.started_at.is_some());

        executor.release.notify_waiters();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancelled_actions_are_forgotten() {
        let executor = Arc::new(BlockingExecutor::default());
        let scheduler = Arc::new(Scheduler::new(executor.clone(), 1));

        let task = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.execute(&action("abandoned")).await })
        };
        wait_until(|| scheduler.actions().len() == 1).await;

        task.abort();
        let _ = task.await;

        assert!(scheduler.actions().is_empty());
    }
}
