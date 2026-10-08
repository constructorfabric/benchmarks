//! Usage outbox handler: delivers `UsageEvent`s to the model policy plugin
//! (spec §13.1, DESIGN §5.6 "Usage publication handler").

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::UsageEvent;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use tracing::{debug, warn};

use crate::infra::gateways::model_policy::{ModelPolicyGateway, PublishOutcome};

/// Handler of the usage queue.
///
/// Deserializes the payload first (malformed -> `Reject`), then publishes it
/// through the gateway, which resolves the plugin: an unavailable plugin or a
/// transient publish error -> `Retry`, a permanent publish error -> `Reject`.
/// Retries are unbounded: billing events must not be dropped while the plugin is
/// merely unavailable.
pub struct UsageHandler {
    policy: Arc<dyn ModelPolicyGateway>,
}

impl UsageHandler {
    #[must_use]
    pub fn new(policy: Arc<dyn ModelPolicyGateway>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(event) => event,
            Err(e) => {
                warn!(
                    partition = msg.partition_id,
                    seq = msg.seq,
                    error = %e,
                    "rejecting malformed usage outbox payload"
                );
                return MessageResult::Reject(format!("malformed usage payload: {e}"));
            }
        };
        let dedupe_key = event.dedupe_key.clone();
        match self.policy.publish_usage(event).await {
            Ok(()) => {
                debug!(%dedupe_key, "usage event published");
                MessageResult::Ok
            }
            Err(PublishOutcome::Retry(reason)) => {
                warn!(%dedupe_key, attempts = msg.attempts, %reason, "usage publish failed; will retry");
                MessageResult::Retry
            }
            Err(PublishOutcome::Reject(reason)) => {
                warn!(%dedupe_key, %reason, "usage publish permanently rejected");
                MessageResult::Reject(reason)
            }
        }
    }
}
