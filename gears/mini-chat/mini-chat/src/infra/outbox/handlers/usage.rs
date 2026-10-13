//! Usage outbox handler: publishes settled usage events to the model policy plugin (DESIGN §5.6).

use std::sync::Arc;

use mini_chat_sdk::UsageEvent;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use crate::domain::ports::PublishFailure;
use crate::domain::services::AppServices;

pub struct UsageHandler {
    app: Arc<AppServices>,
}

impl UsageHandler {
    #[must_use]
    pub fn new(app: Arc<AppServices>) -> Self {
        Self { app }
    }

    /// Handles one payload: corrupt → `Reject`, transient publish error → `Retry`,
    /// permanent publish error → `Reject`.
    pub async fn handle_payload(&self, payload: &[u8]) -> MessageResult {
        let event: UsageEvent = match serde_json::from_slice(payload) {
            Ok(e) => e,
            Err(e) => return reject_corrupt(&e),
        };
        let dedupe_key = event.dedupe_key.clone();
        let res = self.app.policy.publish_usage(event).await;
        map_publish_result(&dedupe_key, res)
    }
}

fn reject_corrupt(e: &serde_json::Error) -> MessageResult {
    tracing::error!(error = %e, "corrupt usage outbox payload; rejecting");
    MessageResult::Reject(format!("invalid usage payload: {e}"))
}

fn map_publish_result(dedupe_key: &str, res: Result<(), PublishFailure>) -> MessageResult {
    match res {
        Ok(()) => MessageResult::Ok,
        Err(PublishFailure::Transient(e)) => {
            tracing::warn!(%dedupe_key, error = %e, "usage publication failed (transient); retrying");
            MessageResult::Retry
        }
        Err(PublishFailure::Permanent(e)) => {
            tracing::error!(%dedupe_key, error = %e, "usage publication failed (permanent); rejecting");
            MessageResult::Reject(format!("permanent publish error: {e}"))
        }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.handle_payload(&msg.payload).await
    }
}

#[cfg(test)]
#[path = "quota_handlers_tests.rs"]
mod tests;
