//! Usage queue handler: publishes `UsageEvent`s to the model policy plugin.
//!
//! Delivery is at-least-once; the plugin deduplicates by the event's `dedupe_key`.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::UsageEvent;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use crate::infra::gateways::policy::{PolicyGateway, PublishOutcome};

/// Publishes each usage event through the policy gateway: `Ok` when the plugin accepted it,
/// `Retry` on a transient failure, `Reject` for a payload that is not a `UsageEvent` or a
/// permanent plugin failure.
pub struct UsageHandler {
    policy: Arc<dyn PolicyGateway>,
}

impl UsageHandler {
    #[must_use]
    pub fn new(policy: Arc<dyn PolicyGateway>) -> Self {
        Self { policy }
    }
}

#[async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(event) => event,
            Err(err) => return MessageResult::Reject(format!("malformed usage payload: {err}")),
        };
        match self.policy.publish_usage(event).await {
            Ok(()) => MessageResult::Ok,
            Err(PublishOutcome::Retry) => MessageResult::Retry,
            Err(PublishOutcome::Reject(reason)) => MessageResult::Reject(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use mini_chat_sdk::PublishError;
    use uuid::Uuid;

    use super::*;
    use crate::infra::outbox::{OutboxRecord, USAGE_PAYLOAD_TYPE};
    use crate::test_support::app::TestApp;
    use crate::test_support::fixtures::usage_event;
    use crate::test_support::outbox::{outbox_message, raw_outbox_message};

    #[tokio::test]
    async fn usage_handler_publishes_and_classifies() {
        let app = TestApp::builder().build().await;
        let handler = UsageHandler::new(Arc::clone(&app.services.policy));
        let event = usage_event(Uuid::new_v4());
        let msg = outbox_message(USAGE_PAYLOAD_TYPE, &event, 0);

        assert!(matches!(handler.handle(&msg).await, MessageResult::Ok));
        assert_eq!(app.usage.usage_events(), vec![event.clone()]);

        app.usage
            .fail_next_publish(PublishError::Transient("plugin down".to_owned()));
        assert!(matches!(handler.handle(&msg).await, MessageResult::Retry));

        app.usage
            .fail_next_publish(PublishError::Permanent("bad event".to_owned()));
        assert!(matches!(handler.handle(&msg).await, MessageResult::Reject(r) if r == "bad event"));
        assert_eq!(
            app.usage.usage_events().len(),
            3,
            "every delivery reached the plugin"
        );

        let garbage = raw_outbox_message(USAGE_PAYLOAD_TYPE, b"{not json", 0);
        assert!(matches!(
            handler.handle(&garbage).await,
            MessageResult::Reject(_)
        ));
        assert_eq!(
            app.usage.usage_events().len(),
            3,
            "garbage is not published"
        );
        // The enqueue-side record and the handler agree on the payload shape.
        let rec = OutboxRecord::usage(&event).unwrap();
        let msg = raw_outbox_message(rec.payload_type, &rec.payload, 0);
        assert!(matches!(handler.handle(&msg).await, MessageResult::Ok));
    }
}
