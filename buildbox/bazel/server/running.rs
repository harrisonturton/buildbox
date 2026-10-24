//! The commands the server is running, which can be listed and cancelled.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::sync::Notify;

/// Every command running, by id.
pub struct RunningCommands {
    next_id: AtomicU64,
    commands: Mutex<BTreeMap<u64, Arc<RunningCommand>>>,
    /// Told whenever the last running command finishes.
    idle: Notify,
    /// Called when each command finishes, with whether none is left.
    on_finish: Box<dyn Fn(bool) + Send + Sync>,
}

/// A command that's running.
pub struct RunningCommand {
    pub id: String,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub started: Instant,
    cancelled: AtomicBool,
    cancel: Notify,
}

impl RunningCommand {
    /// Ask the command to stop. It ends as an interrupted command does.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.cancel.notify_waiters();
    }

    /// Wait until the command is asked to stop.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.cancel.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.cancelled.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

impl RunningCommands {
    /// No commands running yet. `on_finish` is called as each finishes,
    /// cancelled or not, with whether it was the last.
    pub fn new(on_finish: impl Fn(bool) + Send + Sync + 'static) -> Arc<RunningCommands> {
        Arc::new(RunningCommands {
            next_id: AtomicU64::new(1),
            commands: Mutex::new(BTreeMap::new()),
            idle: Notify::new(),
            on_finish: Box::new(on_finish),
        })
    }

    /// Record that a command has started, until the registration returned
    /// is dropped.
    pub fn start(self: &Arc<Self>, args: Vec<String>, working_directory: PathBuf) -> Registration {
        let number = self.next_id.fetch_add(1, Ordering::Relaxed);
        let command = Arc::new(RunningCommand {
            id: number.to_string(),
            args,
            working_directory,
            started: Instant::now(),
            cancelled: AtomicBool::new(false),
            cancel: Notify::new(),
        });
        self.lock().insert(number, command.clone());
        Registration {
            commands: self.clone(),
            number,
            command,
        }
    }

    /// The commands running, oldest first.
    pub fn list(&self) -> Vec<Arc<RunningCommand>> {
        self.lock().values().cloned().collect()
    }

    /// Cancel the command with id `id`, returning whether it was running.
    pub fn cancel(&self, id: &str) -> bool {
        let command = self.lock().values().find(|command| command.id == id).cloned();
        match command {
            Some(command) => {
                command.cancel();
                true
            }
            None => false,
        }
    }

    /// Cancel every running command.
    pub fn cancel_all(&self) {
        for command in self.list() {
            command.cancel();
        }
    }

    /// Wait until no command is running.
    pub async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.lock().is_empty() {
                return;
            }
            notified.await;
        }
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<u64, Arc<RunningCommand>>> {
        self.commands.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A running command's place among the running commands, which it leaves
/// when this is dropped, however the command ends.
pub struct Registration {
    commands: Arc<RunningCommands>,
    number: u64,
    command: Arc<RunningCommand>,
}

impl Registration {
    pub fn command(&self) -> &Arc<RunningCommand> {
        &self.command
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let idle = {
            let mut commands = self.commands.lock();
            commands.remove(&self.number);
            commands.is_empty()
        };
        (self.commands.on_finish)(idle);
        if idle {
            self.commands.idle.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn commands() -> (Arc<RunningCommands>, Arc<AtomicUsize>) {
        let idles = Arc::new(AtomicUsize::new(0));
        let counted = idles.clone();
        let commands = RunningCommands::new(move |idle| {
            if idle {
                counted.fetch_add(1, Ordering::SeqCst);
            }
        });
        (commands, idles)
    }

    #[tokio::test]
    async fn commands_are_listed_until_they_end() {
        let (commands, idles) = commands();
        let first = commands.start(vec!["build".to_string()], PathBuf::from("/w"));
        let second = commands.start(vec!["test".to_string()], PathBuf::from("/w"));
        let ids: Vec<String> = commands.list().iter().map(|command| command.id.clone()).collect();
        assert_eq!(ids, ["1", "2"]);
        drop(first);
        assert_eq!(idles.load(Ordering::SeqCst), 0);
        drop(second);
        assert_eq!(idles.load(Ordering::SeqCst), 1);
        commands.wait_idle().await;
    }

    #[tokio::test]
    async fn cancelling_wakes_the_command() {
        let (commands, _) = commands();
        let registration = commands.start(vec!["build".to_string()], PathBuf::from("/w"));
        let command = registration.command().clone();
        let waiting = tokio::spawn(async move { command.cancelled().await });
        assert!(commands.cancel("1"));
        assert!(!commands.cancel("2"));
        waiting.await.unwrap();
        registration.command().cancelled().await;
    }
}
