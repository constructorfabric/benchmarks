//! Audit outbox handler (DESIGN section 3.2, "Audit plugin and audit outbox").

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::MiniChatAuditEvent;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use crate::domain::ports::{AuditDelivery, AuditSink};

/// Deliveries after which a retried event is dead-lettered (about an hour
/// with the outbox backoff capped at 30 s).
pub const AUDIT_MAX_ATTEMPTS: i32 = 120;

/// Deserializes first (a corrupt payload is rejected whether or not a plugin
/// exists), then delivers through the audit gateway. A `Retry` on the 120th
/// delivery becomes a `Reject`.
pub struct AuditHandler {
    sink: Arc<dyn AuditSink>,
}

impl AuditHandler {
    #[must_use]
    pub fn new(sink: Arc<dyn AuditSink>) -> Self {
        Self { sink }
    }
}

#[async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: MiniChatAuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(ev) => ev,
            Err(e) => {
                tracing::error!(seq = msg.seq, error = %e, result = "reject", "malformed audit outbox payload");
                return MessageResult::Reject(format!("malformed audit payload: {e}"));
            }
        };
        match self.sink.deliver(ev).await {
            AuditDelivery::Delivered => MessageResult::Ok,
            AuditDelivery::NoPlugin => {
                tracing::debug!(
                    seq = msg.seq,
                    result = "dropped",
                    "no audit plugin; event dropped"
                );
                MessageResult::Ok
            }
            AuditDelivery::Retry(m) => {
                if i32::from(msg.attempts) + 1 >= AUDIT_MAX_ATTEMPTS {
                    tracing::error!(seq = msg.seq, error = %m, result = "reject", "audit delivery gave up");
                    MessageResult::Reject(format!(
                        "audit delivery: max attempts ({AUDIT_MAX_ATTEMPTS}) reached: {m}"
                    ))
                } else {
                    tracing::warn!(seq = msg.seq, attempts = msg.attempts, error = %m, "audit delivery failed; retrying");
                    MessageResult::Retry
                }
            }
            AuditDelivery::Reject(m) => {
                tracing::error!(seq = msg.seq, error = %m, result = "reject", "audit event permanently rejected");
                MessageResult::Reject(format!("audit plugin rejected the event: {m}"))
            }
        }
    }
}
