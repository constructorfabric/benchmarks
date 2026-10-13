//! Shared provisioning state: aliases whose OAGW upstream is not provisioned yet, a nudge to retry
//! immediately, and a change notification for waiters.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::Notify;

#[derive(Debug, Default)]
pub struct ProvisioningGate {
    pending: Mutex<HashSet<String>>,
    retry_now: Notify,
    changed: Notify,
}

impl ProvisioningGate {
    /// Records the aliases still pending after a pass.
    pub fn set_pending(&self, aliases: impl IntoIterator<Item = String>) {
        if let Ok(mut p) = self.pending.lock() {
            *p = aliases.into_iter().collect();
        }
        self.changed.notify_waiters();
    }

    #[must_use]
    pub fn is_pending(&self, alias: &str) -> bool {
        self.pending.lock().map(|p| p.contains(alias)).unwrap_or(false)
    }

    /// Wakes the background loop for an immediate retry.
    pub fn nudge(&self) {
        self.retry_now.notify_one();
    }

    /// Resolves when a retry was requested.
    pub async fn retry_requested(&self) {
        self.retry_now.notified().await;
    }

    /// Nudges and waits (bounded) until `alias` is no longer pending.
    pub async fn wait_ready(&self, alias: &str, timeout: Duration) {
        if !self.is_pending(alias) {
            return;
        }
        self.nudge();
        let _ = tokio::time::timeout(timeout, async {
            while self.is_pending(alias) {
                let changed = self.changed.notified();
                if !self.is_pending(alias) {
                    break;
                }
                changed.await;
            }
        })
        .await;
    }
}
