//! Background task set of the gear: the OAGW provisioning reconcile task and the hook that
//! the background-maintenance feature uses to spawn the orphan watchdog and the upload
//! reaper. `stop` cancels the token and joins every task with a bounded timeout.

use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// What [`BackgroundTasks::shutdown`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Names of the tasks that finished within the timeout.
    pub joined: Vec<String>,
    /// Names of the tasks still running when the timeout fired.
    pub timed_out: Vec<String>,
}

/// A set of named background tasks sharing one cancellation token.
#[derive(Debug)]
pub struct BackgroundTasks {
    token: CancellationToken,
    tasks: Mutex<Vec<(String, JoinHandle<()>)>>,
}

impl BackgroundTasks {
    /// A task set whose token is a child of `parent`: cancelling `parent` cancels every task.
    #[must_use]
    pub fn new(parent: &CancellationToken) -> Self {
        Self {
            token: parent.child_token(),
            tasks: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(String, JoinHandle<()>)>> {
        match self.tasks.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Spawn the task `name`; `make` receives the cancellation token the task must watch.
    pub fn spawn<F, Fut>(&self, name: &str, make: F)
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-spawn-workers
        // the hook of the leader-only workers (orphan watchdog, upload reaper) that
        // `background-maintenance` spawns after the outbox pipeline runs
        let handle = tokio::spawn(make(self.token.clone()));
        self.lock().push((name.to_owned(), handle));
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-spawn-workers
    }

    /// The number of spawned tasks that have not finished yet.
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.lock()
            .iter()
            .filter(|(_, handle)| !handle.is_finished())
            .count()
    }

    /// Cancel the token of every task without joining.
    pub fn cancel(&self) {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-stop:p1:inst-stop-cancel-workers
        self.token.cancel();
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-stop:p1:inst-stop-cancel-workers
    }

    /// Cancel every task and join them all within `timeout` (one bound for the whole set).
    /// Tasks that did not finish are reported by name and left aborted.
    pub async fn shutdown(&self, timeout: Duration) -> ShutdownReport {
        self.cancel();
        let tasks = std::mem::take(&mut *self.lock());
        let mut report = ShutdownReport::default();
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-stop:p1:inst-stop-join-workers
        // one deadline for the whole set, so a stuck task does not stretch the stop
        let deadline = tokio::time::Instant::now() + timeout;
        for (name, mut handle) in tasks {
            match tokio::time::timeout_at(deadline, &mut handle).await {
                Ok(outcome) => {
                    if let Err(error) = outcome
                        && error.is_panic()
                    {
                        tracing::error!(task = %name, "background task panicked");
                    }
                    report.joined.push(name);
                }
                Err(_elapsed) => {
                    handle.abort();
                    report.timed_out.push(name);
                }
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-stop:p1:inst-stop-join-workers
        report
    }
}

#[cfg(test)]
#[path = "tasks_tests.rs"]
mod tasks_tests;
