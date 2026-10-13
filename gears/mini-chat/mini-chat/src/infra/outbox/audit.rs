//! Audit handler: delivers audit events to the audit plugin (ADR-0009).

use std::sync::Arc;

use mini_chat_sdk::MiniChatAuditEvent;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use crate::domain::audit::AuditDelivery;
use crate::domain::service::Svc;

/// Deliveries before an audit event is dead-lettered.
pub const MAX_AUDIT_ATTEMPTS: i16 = 120;

/// Leased handler of `mini-chat.audit`.
pub struct AuditHandler {
    /// Services.
    pub svc: Arc<Svc>,
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let m = &self.svc.metrics;
        let event: MiniChatAuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => {
                m.inc("audit_emit_total", &[("result", "reject")]);
                return MessageResult::Reject(format!("malformed audit payload: {e}"));
            }
        };
        match self.svc.audit.deliver(event).await {
            AuditDelivery::Delivered => {
                m.inc("audit_emit_total", &[("result", "ok")]);
                MessageResult::Ok
            }
            AuditDelivery::Dropped => {
                m.inc("audit_emit_total", &[("result", "dropped")]);
                MessageResult::Ok
            }
            AuditDelivery::Retry(e) => {
                if msg.attempts + 1 >= MAX_AUDIT_ATTEMPTS {
                    m.inc("audit_emit_total", &[("result", "reject")]);
                    return MessageResult::Reject(format!("audit delivery failed after {MAX_AUDIT_ATTEMPTS} attempts: {e}"));
                }
                m.inc("audit_emit_total", &[("result", "retry")]);
                MessageResult::Retry
            }
            AuditDelivery::Reject(e) => {
                m.inc("audit_emit_total", &[("result", "reject")]);
                MessageResult::Reject(e)
            }
        }
    }
}
