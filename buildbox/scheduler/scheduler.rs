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
use tracing::Instrument;

/// The platform property that names the environment an action runs in.
pub const ENVIRONMENT_PROPERTY: &str = "environment";

/// The environment an action runs in if it doesn't name one.
pub const DEFAULT_ENVIRONMENT: &str = "default";

/// An [`Executor`] that runs each action in the environment it asks for, at
/// most `max_concurrent` at a time across all environments. Actions beyond
/// that wait in a queue.
///
/// Each environment is another executor. Actions choose one with the
/// [`ENVIRONMENT_PROPERTY`] platform property, or run in
/// [`DEFAULT_ENVIRONMENT`].
pub struct Scheduler {
    environments: BTreeMap<String, Arc<dyn Executor>>,
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
    pub environment: String,
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
    /// Create a scheduler that runs actions on the executors in
    /// `environments`, by name.
    pub fn new(environments: BTreeMap<String, Arc<dyn Executor>>, max_concurrent: usize) -> Scheduler {
        Scheduler {
            environments,
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

    /// The executor for the environment `action` asks for.
    fn executor_for(&self, action: &Action) -> Result<(&str, &Arc<dyn Executor>), ExecuteError> {
        let name = environment_name(action)?;
        let Some((name, executor)) = self.environments.get_key_value(name) else {
            let known: Vec<&str> = self.environments.keys().map(String::as_str).collect();
            let message = format!("unknown environment {name:?}; this server has {}", known.join(", "));
            return Err(ExecuteError::InvalidAction(message));
        };
        Ok((name, executor))
    }
}

/// The environment named by the action's platform properties.
fn environment_name(action: &Action) -> Result<&str, ExecuteError> {
    let mut names = action
        .platform
        .iter()
        .filter(|(property, _)| property == ENVIRONMENT_PROPERTY)
        .map(|(_, value)| value.as_str());

    let Some(name) = names.next() else {
        return Ok(DEFAULT_ENVIRONMENT);
    };
    if names.next().is_some() {
        let message = format!("the {ENVIRONMENT_PROPERTY:?} platform property is given more than once");
        return Err(ExecuteError::InvalidAction(message));
    }
    Ok(name)
}

#[async_trait]
impl Executor for Scheduler {
    async fn execute(&self, action: &Action) -> Result<ActionResult, ExecuteError> {
        let (environment, executor) = self.executor_for(action)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let queued_ahead = self.tracker.queued();
        let entry = Entry::add(self.tracker.clone(), id, action, environment);

        // The semaphore is never closed, so acquiring can't fail.
        let wait_span = tracing::info_span!("wait_for_slot", queued_ahead);
        let Ok(_slot) = self.slots.acquire().instrument(wait_span).await else {
            return Err(ExecuteError::Internal("scheduler is shutting down".to_string()));
        };
        entry.mark_running();

        executor.execute(action).await
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

    /// How many actions are waiting for a slot.
    fn queued(&self) -> usize {
        let actions = self.lock();
        actions.values().filter(|info| info.state == ActionState::Queued).count()
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
    fn add(tracker: Arc<Tracker>, id: u64, action: &Action, environment: &str) -> Entry {
        let info = ActionInfo {
            id,
            digest: action.digest.clone(),
            arguments: action.command.arguments.clone(),
            environment: environment.to_string(),
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
            platform: vec![],
        }
    }

    fn action_in(name: &str, environment: &str) -> Action {
        Action {
            platform: vec![(ENVIRONMENT_PROPERTY.to_string(), environment.to_string())],
            ..action(name)
        }
    }

    /// A scheduler whose only environment is the default one.
    fn scheduler_with(executor: Arc<dyn Executor>, max_concurrent: usize) -> Arc<Scheduler> {
        let environments = BTreeMap::from([(DEFAULT_ENVIRONMENT.to_string(), executor)]);
        Arc::new(Scheduler::new(environments, max_concurrent))
    }

    /// An executor that reports which one it is in the result's exit code.
    struct NumberedExecutor(i32);

    #[async_trait]
    impl Executor for NumberedExecutor {
        async fn execute(&self, _action: &Action) -> Result<ActionResult, ExecuteError> {
            Ok(ActionResult {
                exit_code: self.0,
                ..Default::default()
            })
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
        let scheduler = scheduler_with(executor.clone(), 2);

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
        let scheduler = scheduler_with(executor.clone(), 1);

        let task = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.execute(&action("compile")).await })
        };
        wait_until(|| count_in_state(&scheduler, ActionState::Running) == 1).await;

        let info = &scheduler.actions()[0];
        assert_eq!(info.digest, Digest::of_bytes(b"compile"));
        assert_eq!(info.arguments, ["compile"]);
        assert_eq!(info.environment, DEFAULT_ENVIRONMENT);
        assert!(info.started_at.is_some());

        executor.release.notify_waiters();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancelled_actions_are_forgotten() {
        let executor = Arc::new(BlockingExecutor::default());
        let scheduler = scheduler_with(executor.clone(), 1);

        let task = {
            let scheduler = scheduler.clone();
            tokio::spawn(async move { scheduler.execute(&action("abandoned")).await })
        };
        wait_until(|| scheduler.actions().len() == 1).await;

        task.abort();
        let _ = task.await;

        assert!(scheduler.actions().is_empty());
    }

    #[tokio::test]
    async fn runs_actions_in_the_environment_they_name() {
        let environments: BTreeMap<String, Arc<dyn Executor>> = BTreeMap::from([
            (DEFAULT_ENVIRONMENT.to_string(), Arc::new(NumberedExecutor(1)) as Arc<dyn Executor>),
            ("container".to_string(), Arc::new(NumberedExecutor(2)) as Arc<dyn Executor>),
        ]);
        let scheduler = Scheduler::new(environments, 1);

        let default = scheduler.execute(&action("a")).await.unwrap();
        let container = scheduler.execute(&action_in("b", "container")).await.unwrap();

        assert_eq!(default.exit_code, 1);
        assert_eq!(container.exit_code, 2);
    }

    #[tokio::test]
    async fn rejects_unknown_environments() {
        let scheduler = scheduler_with(Arc::new(NumberedExecutor(0)), 1);

        let err = scheduler.execute(&action_in("a", "missing")).await.unwrap_err();

        assert!(matches!(&err, ExecuteError::InvalidAction(message) if message.contains("missing")), "{err}");
        assert!(scheduler.actions().is_empty());
    }

    #[tokio::test]
    async fn rejects_conflicting_environments() {
        let scheduler = scheduler_with(Arc::new(NumberedExecutor(0)), 1);
        let mut action = action_in("a", DEFAULT_ENVIRONMENT);
        action.platform.push((ENVIRONMENT_PROPERTY.to_string(), "other".to_string()));

        let err = scheduler.execute(&action).await.unwrap_err();

        assert!(matches!(err, ExecuteError::InvalidAction(_)), "{err}");
    }

    #[tokio::test]
    async fn ignores_other_platform_properties() {
        let scheduler = scheduler_with(Arc::new(NumberedExecutor(7)), 1);
        let mut action = action("a");
        action.platform.push(("OSFamily".to_string(), "linux".to_string()));

        let result = scheduler.execute(&action).await.unwrap();

        assert_eq!(result.exit_code, 7);
    }
}
