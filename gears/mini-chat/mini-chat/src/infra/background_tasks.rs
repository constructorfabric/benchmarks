//! Background task registry of the gear: spawn, cancel and bounded join
//! (`cpt-cf-mini-chat-flow-gear-foundation-gear-stop`).

use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Bound of the join in `stop`; shorter than the framework stop deadline.
pub const TASK_JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// A named, spawned background task.
type NamedTask = (&'static str, JoinHandle<()>);

/// Registry of the gear's spawned background tasks.
pub struct BackgroundTasks {
    cancel: CancellationToken,
    tasks: Mutex<Vec<NamedTask>>,
}

impl BackgroundTasks {
    /// Creates a registry whose tasks are cancelled through a child of `parent`.
    #[must_use]
    pub fn new(parent: &CancellationToken) -> Self {
        Self {
            cancel: parent.child_token(),
            tasks: Mutex::new(Vec::new()),
        }
    }

    /// Token handed to the tasks; cancelled by [`Self::cancel_and_join`].
    #[must_use]
    pub fn token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Spawns `future` under `name`.
    pub fn spawn<F>(&self, name: &'static str, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let handle = tokio::spawn(future);
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.push((name, handle));
        }
    }

    /// Names of the tasks that have not finished.
    #[must_use]
    pub fn running(&self) -> Vec<&'static str> {
        self.tasks
            .lock()
            .map(|tasks| {
                tasks
                    .iter()
                    .filter(|(_, handle)| !handle.is_finished())
                    .map(|(name, _)| *name)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Cancels every task and waits at most `timeout` for them to finish.
    /// Returns the names of the tasks still running (a warning naming them is
    /// logged) instead of blocking.
    pub async fn cancel_and_join(&self, timeout: Duration) -> Vec<&'static str> {
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-cancel-tasks
        self.cancel.cancel();
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-cancel-tasks
        let handles: Vec<NamedTask> = self
            .tasks
            .lock()
            .map(|mut tasks| std::mem::take(&mut *tasks))
            .unwrap_or_default();
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-join-tasks
        let deadline = tokio::time::Instant::now() + timeout;
        let mut stuck = Vec::new();
        for (name, mut handle) in handles {
            match tokio::time::timeout_at(deadline, &mut handle).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::warn!(task = name, %error, "background task ended abnormally");
                }
                Err(_elapsed) => {
                    stuck.push(name);
                    if let Ok(mut tasks) = self.tasks.lock() {
                        tasks.push((name, handle));
                    }
                }
            }
        }
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-join-tasks
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-if-join-timeout
        if !stuck.is_empty() {
            // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-log-join-timeout
            tracing::warn!(tasks = ?stuck, "background tasks did not end within the join timeout");
            // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-log-join-timeout
        }
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-stop:p1:inst-if-join-timeout
        stuck
    }
}

#[cfg(test)]
#[path = "background_tasks_tests.rs"]
mod tests;
