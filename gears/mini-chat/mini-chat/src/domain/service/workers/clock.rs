//! The injectable monotonic clock of the worker deadlines
//! (`cpt-cf-mini-chat-dod-background-workers-quality-gates`, "Deterministic
//! time"): the chat-cleanup delivery budget, the per-call deadlines, the
//! summary call deadline and the Lease timers are measured on it.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::{Either, select};

/// A monotonic clock with a sleep.
#[async_trait]
pub trait WorkerClock: Send + Sync {
    /// Time since an arbitrary fixed origin; never goes back.
    fn now(&self) -> Duration;

    /// Completes after `duration` of this clock.
    async fn sleep(&self, duration: Duration);
}

/// The production clock: tokio time.
#[derive(Debug, Clone)]
pub struct TokioWorkerClock {
    origin: tokio::time::Instant,
}

impl TokioWorkerClock {
    /// A clock whose origin is now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: tokio::time::Instant::now(),
        }
    }
}

impl Default for TokioWorkerClock {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WorkerClock for TokioWorkerClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// A shared clock.
pub type SharedWorkerClock = Arc<dyn WorkerClock>;

/// Runs `future` for at most `limit` on `clock`; `None` when the limit elapsed
/// first (the future is dropped).
pub async fn with_timeout<T>(
    clock: &dyn WorkerClock,
    limit: Duration,
    future: impl Future<Output = T> + Send,
) -> Option<T> {
    let future = Box::pin(future);
    let sleeper = clock.sleep(limit);
    match select(future, sleeper).await {
        Either::Left((value, _sleeper)) => Some(value),
        Either::Right(((), _future)) => None,
    }
}
