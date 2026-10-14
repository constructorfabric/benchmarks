//! The leased handler of the usage queue (`cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler`):
//! delivers each `UsageEvent` to the model-policy plugin through the
//! [`UsagePublisher`] port. Outcomes: `Ok`, `Retry` (plugin resolution failure or a transient
//! publish error) and `Reject(reason)` (a payload that cannot be deserialized or a permanent
//! publish error); delivery is at least once and the handler itself performs no deduplication.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::UsageEvent;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use crate::domain::quota::{UsagePublishError, UsagePublisher};

/// The usage queue handler.
pub struct UsageOutboxHandler {
    publisher: Arc<dyn UsagePublisher>,
}

impl UsageOutboxHandler {
    /// Create the handler over a publisher.
    #[must_use]
    pub fn new(publisher: Arc<dyn UsagePublisher>) -> Self {
        Self { publisher }
    }
}

#[async_trait]
impl LeasedMessageHandler for UsageOutboxHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-deserialize
        let event: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(event) => event,
            Err(error) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-corrupt
                // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-return-reject-corrupt
                // the shared outbox moves a payload that cannot be read to its dead-letter store
                tracing::error!(
                    %error,
                    attempts = msg.attempts,
                    "the usage message cannot be deserialized; it is dead-lettered"
                );
                return MessageResult::Reject(format!("corrupt usage payload: {error}"));
                // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-return-reject-corrupt
                // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-corrupt
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-deserialize
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-try
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-duplicate-delivery
        // the plugin is resolved and called by the publisher; the handler performs no
        // deduplication (a redelivery after a crash reaches the consumer, which deduplicates by
        // the `dedupe_key`) and has no attempt cap
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement:p1:inst-settle-publish
        let published = self.publisher.publish_usage(event).await;
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement:p1:inst-settle-publish
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-duplicate-delivery
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-try
        match published {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-return-ok
            // @cpt-begin:cpt-cf-mini-chat-state-quota-and-billing-usage-delivery:p1:inst-usage-state-delivered
            // @cpt-begin:cpt-cf-mini-chat-state-quota-and-billing-usage-delivery:p1:inst-usage-state-retry-delivered
            Ok(()) => MessageResult::Ok,
            // @cpt-end:cpt-cf-mini-chat-state-quota-and-billing-usage-delivery:p1:inst-usage-state-retry-delivered
            // @cpt-end:cpt-cf-mini-chat-state-quota-and-billing-usage-delivery:p1:inst-usage-state-delivered
            // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-return-ok
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-return-retry
            // @cpt-begin:cpt-cf-mini-chat-state-quota-and-billing-usage-delivery:p1:inst-usage-state-retrying
            Err(UsagePublishError::Transient(cause)) => {
                tracing::warn!(
                    %cause,
                    attempts = msg.attempts,
                    "the usage message could not be published; it is retried"
                );
                MessageResult::Retry
            }
            // @cpt-end:cpt-cf-mini-chat-state-quota-and-billing-usage-delivery:p1:inst-usage-state-retrying
            // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-return-retry
            Err(UsagePublishError::Permanent(cause)) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-catch-permanent
                // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-return-reject-permanent
                // @cpt-begin:cpt-cf-mini-chat-state-quota-and-billing-usage-delivery:p1:inst-usage-state-dead-direct
                // @cpt-begin:cpt-cf-mini-chat-state-quota-and-billing-usage-delivery:p1:inst-usage-state-retry-dead
                tracing::error!(
                    %cause,
                    attempts = msg.attempts,
                    "the plugin rejected the usage message permanently; it is dead-lettered"
                );
                MessageResult::Reject(format!("permanent usage publish failure: {cause}"))
                // @cpt-end:cpt-cf-mini-chat-state-quota-and-billing-usage-delivery:p1:inst-usage-state-retry-dead
                // @cpt-end:cpt-cf-mini-chat-state-quota-and-billing-usage-delivery:p1:inst-usage-state-dead-direct
                // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-return-reject-permanent
                // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-usage-outbox-handler:p1:inst-uh-catch-permanent
            }
        }
    }
}

#[cfg(test)]
#[path = "usage_handler_tests.rs"]
mod usage_handler_tests;
