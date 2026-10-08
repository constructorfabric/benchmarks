//! Background tasks spawned by request handlers (attachment indexing). The
//! gear cancels the token and waits for the tracker on stop.

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

#[derive(Clone, Debug, Default)]
pub struct Background {
    pub cancel: CancellationToken,
    pub tracker: TaskTracker,
}

impl Background {
    /// Cancels every task and waits up to `timeout` for them to end.
    pub async fn shutdown(&self, timeout: std::time::Duration) {
        self.cancel.cancel();
        self.tracker.close();
        if tokio::time::timeout(timeout, self.tracker.wait())
            .await
            .is_err()
        {
            tracing::warn!("mini-chat background tasks did not stop in time");
        }
    }
}
