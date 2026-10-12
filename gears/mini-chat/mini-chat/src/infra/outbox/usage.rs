//! Handler of the usage queue: publishes settlement events to the model-policy plugin
//! (DESIGN sections 5.6, 5.7 "Shared Outbox Processing Model").

use std::sync::Arc;

use mini_chat_sdk::UsageEvent;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use tracing::{debug, warn};

use crate::domain::ports::{ModelPolicyGateway, UsagePublication};

/// Leased handler of the usage queue.
///
/// An undecodable payload and a permanent plugin failure are rejected (dead-lettered); a plugin
/// that cannot be resolved or fails transiently is retried by the shared outbox.
pub struct UsageOutboxHandler {
    policy: Arc<dyn ModelPolicyGateway>,
}

impl UsageOutboxHandler {
    /// Creates the handler.
    #[must_use]
    pub fn new(policy: Arc<dyn ModelPolicyGateway>) -> Self {
        Self { policy }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for UsageOutboxHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(event) => event,
            Err(err) => return MessageResult::Reject(format!("malformed usage payload: {err}")),
        };
        let dedupe_key = event.dedupe_key.clone();
        match self.policy.publish_usage(event).await {
            UsagePublication::Published => {
                debug!(dedupe_key = %dedupe_key, "usage event published");
                MessageResult::Ok
            }
            UsagePublication::Retry(cause) => {
                warn!(dedupe_key = %dedupe_key, cause = %cause, "usage publication failed transiently; retrying");
                MessageResult::Retry
            }
            UsagePublication::Reject(cause) => {
                warn!(dedupe_key = %dedupe_key, cause = %cause, "usage publication failed permanently");
                MessageResult::Reject(format!("permanent usage publication failure: {cause}"))
            }
        }
    }
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
