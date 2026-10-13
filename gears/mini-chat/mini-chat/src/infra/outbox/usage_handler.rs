//! Usage publication handler (DESIGN section 5.6): hands each settled usage
//! event to the model policy plugin.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{PublishError, UsageEvent};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use crate::domain::ports::PolicyProvider;

/// `Ok` on publish; `Retry` on a transient failure (including plugin
/// resolution); `Reject` on a malformed payload or a permanent failure.
pub struct UsageHandler {
    policy: Arc<dyn PolicyProvider>,
}

impl UsageHandler {
    #[must_use]
    pub fn new(policy: Arc<dyn PolicyProvider>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(ev) => ev,
            Err(e) => {
                tracing::error!(seq = msg.seq, error = %e, "malformed usage outbox payload");
                return MessageResult::Reject(format!("malformed usage payload: {e}"));
            }
        };
        let dedupe_key = ev.dedupe_key.clone();
        match self.policy.publish_usage(ev).await {
            Ok(()) => MessageResult::Ok,
            Err(PublishError::Transient(m)) => {
                tracing::warn!(%dedupe_key, attempts = msg.attempts, error = %m, "usage publish failed; retrying");
                MessageResult::Retry
            }
            Err(PublishError::Permanent(m)) => {
                tracing::error!(%dedupe_key, error = %m, "usage publish permanently rejected");
                MessageResult::Reject(format!("usage publish rejected: {m}"))
            }
        }
    }
}
