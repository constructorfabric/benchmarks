//! Usage handler: publishes usage events through the model policy plugin.

use std::sync::Arc;

use mini_chat_sdk::UsageEvent;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use crate::domain::policy::PublishOutcome;
use crate::domain::service::Svc;

/// Leased handler of `mini-chat.usage_snapshot`.
pub struct UsageHandler {
    /// Services.
    pub svc: Arc<Svc>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed usage payload: {e}")),
        };
        match self.svc.policy.publish_usage(event).await {
            Ok(()) => MessageResult::Ok,
            Err(PublishOutcome::Retry(e)) => {
                tracing::warn!(error = %e, "usage publication failed, retrying");
                MessageResult::Retry
            }
            Err(PublishOutcome::Reject(e)) => MessageResult::Reject(e),
        }
    }
}
