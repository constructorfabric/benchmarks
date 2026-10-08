//! Usage publication handler (queue `outbox.queue_name`).

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{PublishError, UsageEvent};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use tracing::warn;

use super::decode;
use crate::domain::ports::PolicyPort;

/// Publishes settled usage events to the model policy plugin: transient
/// failures (plugin resolution included) `Retry`, permanent ones `Reject`.
pub struct UsageHandler {
    policy: Arc<dyn PolicyPort>,
}

impl UsageHandler {
    #[must_use]
    pub fn new(policy: Arc<dyn PolicyPort>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: UsageEvent = match decode("usage", msg) {
            Ok(ev) => ev,
            Err(reject) => return reject,
        };
        let dedupe_key = ev.dedupe_key.clone();
        match self.policy.publish_usage(ev).await {
            Ok(()) => MessageResult::Ok,
            Err(PublishError::Transient(e)) => {
                warn!(%dedupe_key, error = %e, "usage publish failed; retrying");
                MessageResult::Retry
            }
            Err(PublishError::Permanent(e)) => {
                warn!(%dedupe_key, error = %e, "usage publish rejected; dead-lettering");
                MessageResult::Reject(format!("usage publish rejected: {e}"))
            }
        }
    }
}
