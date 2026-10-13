//! Audit queue handler (DESIGN "Audit plugin and audit outbox", ADR-0009).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::AuditEvent;
use opentelemetry::KeyValue;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use crate::infra::gateways::audit::{AuditDelivery, AuditGateway};
use crate::metrics::Metrics;

/// Bound of one plugin call; a timeout is a transient failure.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);
/// Redeliveries after which a still failing event is dead-lettered (the 120th attempt, about an
/// hour with the outbox backoff capped at 30 s), so a misconfigured plugin cannot block the
/// partition for ever.
const MAX_RETRIES: i16 = 119;

/// Delivers each audit event through the audit gateway.
///
/// The payload is deserialized before the plugin is looked up, so a corrupt payload is rejected
/// whether or not a plugin is registered. Outcomes are counted in `audit_emit{result}`.
pub struct AuditHandler {
    audit: Arc<dyn AuditGateway>,
    metrics: Arc<Metrics>,
    timeout: Duration,
}

impl AuditHandler {
    #[must_use]
    pub fn new(audit: Arc<dyn AuditGateway>, metrics: Arc<Metrics>) -> Self {
        Self {
            audit,
            metrics,
            timeout: DELIVERY_TIMEOUT,
        }
    }

    /// Overrides the plugin call timeout (tests).
    #[cfg(test)]
    #[must_use]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn count(&self, result: &'static str) {
        self.metrics
            .audit_emit
            .add(1, &[KeyValue::new("result", result)]);
    }

    fn reject(&self, reason: String) -> MessageResult {
        self.count("reject");
        MessageResult::Reject(reason)
    }
}

#[async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: AuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(event) => event,
            Err(err) => return self.reject(format!("malformed audit payload: {err}")),
        };
        let delivery = tokio::time::timeout(self.timeout, self.audit.deliver(event))
            .await
            .unwrap_or_else(|_| {
                tracing::warn!(timeout = ?self.timeout, "audit plugin call timed out");
                AuditDelivery::Retry
            });
        match delivery {
            AuditDelivery::Ok => {
                self.count("ok");
                MessageResult::Ok
            }
            AuditDelivery::Dropped => {
                self.count("dropped");
                MessageResult::Ok
            }
            AuditDelivery::Retry if msg.attempts >= MAX_RETRIES => self.reject(format!(
                "audit delivery: max attempts ({}) reached",
                MAX_RETRIES + 1
            )),
            AuditDelivery::Retry => {
                self.count("retry");
                MessageResult::Retry
            }
            AuditDelivery::Reject(reason) => self.reject(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use mini_chat_sdk::{AuditEvent, AuditPluginError};
    use uuid::Uuid;

    use super::*;
    use crate::infra::gateways::audit::AuditDelivery;
    use crate::infra::outbox::AUDIT_PAYLOAD_TYPE;
    use crate::test_support::app::TestApp;
    use crate::test_support::fixtures::mutation_audit_event;
    use crate::test_support::outbox::{outbox_message, raw_outbox_message};

    /// Gateway answering a fixed delivery (no plugin registered: `Dropped`).
    struct Fixed(AuditDelivery);

    #[async_trait]
    impl AuditGateway for Fixed {
        async fn deliver(&self, _ev: AuditEvent) -> AuditDelivery {
            self.0.clone()
        }
    }

    /// Gateway whose plugin never answers.
    struct Hanging;

    #[async_trait]
    impl AuditGateway for Hanging {
        async fn deliver(&self, _ev: AuditEvent) -> AuditDelivery {
            std::future::pending().await
        }
    }

    fn event() -> AuditEvent {
        mutation_audit_event(Uuid::new_v4(), Uuid::new_v4())
    }

    #[tokio::test]
    async fn audit_handler_outcomes() {
        let app = TestApp::builder().build().await;
        let handler = AuditHandler::new(
            Arc::clone(&app.services.audit),
            Arc::clone(&app.services.metrics),
        );
        let ev = event();
        let first = outbox_message(AUDIT_PAYLOAD_TYPE, &ev, 0);

        // Delivered.
        assert!(matches!(handler.handle(&first).await, MessageResult::Ok));
        assert_eq!(app.audit.events(), vec![ev.clone()]);

        // Transient plugin failure: retried until the 120th attempt.
        app.audit
            .fail_next(AuditPluginError::Transient("down".to_owned()));
        assert!(matches!(handler.handle(&first).await, MessageResult::Retry));
        app.audit.fail_next(AuditPluginError::PluginTimeout);
        let before_last = outbox_message(AUDIT_PAYLOAD_TYPE, &ev, 118);
        assert!(matches!(
            handler.handle(&before_last).await,
            MessageResult::Retry
        ));
        app.audit
            .fail_next(AuditPluginError::Transient("down".to_owned()));
        let last = outbox_message(AUDIT_PAYLOAD_TYPE, &ev, 119);
        assert!(matches!(
            handler.handle(&last).await,
            MessageResult::Reject(_)
        ));

        // Permanent plugin failure.
        app.audit
            .fail_next(AuditPluginError::Permanent("schema".to_owned()));
        assert!(matches!(handler.handle(&first).await, MessageResult::Reject(r) if r == "schema"));
    }

    #[tokio::test]
    async fn audit_handler_without_plugin_drops_valid_events_and_rejects_garbage() {
        let app = TestApp::builder().build().await;
        let handler = AuditHandler::new(
            Arc::new(Fixed(AuditDelivery::Dropped)),
            Arc::clone(&app.services.metrics),
        );
        let valid = outbox_message(AUDIT_PAYLOAD_TYPE, &event(), 0);
        assert!(matches!(handler.handle(&valid).await, MessageResult::Ok));

        let garbage = raw_outbox_message(AUDIT_PAYLOAD_TYPE, b"{\"kind\":\"nope\"}", 0);
        assert!(matches!(
            handler.handle(&garbage).await,
            MessageResult::Reject(_)
        ));
    }

    #[tokio::test]
    async fn audit_handler_treats_a_slow_plugin_as_transient() {
        let app = TestApp::builder().build().await;
        let handler = AuditHandler::new(Arc::new(Hanging), Arc::clone(&app.services.metrics))
            .with_timeout(Duration::from_millis(50));
        let ev = event();
        let msg = outbox_message(AUDIT_PAYLOAD_TYPE, &ev, 3);
        assert!(matches!(handler.handle(&msg).await, MessageResult::Retry));
        let last = outbox_message(AUDIT_PAYLOAD_TYPE, &ev, 119);
        assert!(matches!(
            handler.handle(&last).await,
            MessageResult::Reject(_)
        ));
    }
}
