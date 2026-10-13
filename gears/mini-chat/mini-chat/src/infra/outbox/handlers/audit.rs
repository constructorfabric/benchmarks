//! Audit outbox handler: delivers audit events to the audit plugin (DESIGN §3.2 "Audit plugin and
//! audit outbox", §5.6 "Shared Outbox Processing Model").

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::MiniChatAuditEvent;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use crate::domain::ports::{AuditDelivery, AuditFailure};
use crate::domain::services::AppServices;
use crate::infra::metrics;

/// Attempt on which a still-failing (transient) delivery is dead-lettered.
pub const MAX_AUDIT_ATTEMPTS: i32 = 120;
/// Plugin call timeout (a timeout is a transient failure).
const EMIT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct AuditHandler {
    app: Arc<AppServices>,
}

fn count(result: &str) {
    metrics::incr("mini_chat_audit_emit", 1, &[("result", result.to_owned())]);
}

impl AuditHandler {
    #[must_use]
    pub fn new(app: Arc<AppServices>) -> Self {
        Self { app }
    }

    /// Handles one payload; `attempts` = retries so far (0 on the first attempt).
    pub async fn handle_payload(&self, payload: &[u8], attempts: i16) -> MessageResult {
        let event: MiniChatAuditEvent = match serde_json::from_slice(payload) {
            Ok(e) => e,
            Err(e) => return reject_corrupt(&e),
        };
        let event_type = event.event_type().to_owned();
        let outcome = tokio::time::timeout(EMIT_TIMEOUT, self.app.audit.emit(event))
            .await
            .unwrap_or_else(|_| Err(AuditFailure::Transient("audit plugin timed out".to_owned())));
        map_outcome(&event_type, attempts, outcome)
    }
}

fn reject_corrupt(e: &serde_json::Error) -> MessageResult {
    tracing::error!(error = %e, "corrupt audit outbox payload; rejecting");
    count("reject");
    MessageResult::Reject(format!("invalid audit payload: {e}"))
}

fn map_outcome(event_type: &str, attempts: i16, outcome: Result<AuditDelivery, AuditFailure>) -> MessageResult {
    match outcome {
        Ok(AuditDelivery::Delivered) => {
            count("ok");
            MessageResult::Ok
        }
        Ok(AuditDelivery::NoPlugin) => {
            count("dropped");
            MessageResult::Ok
        }
        Err(AuditFailure::Transient(e)) => transient(event_type, attempts, &e),
        Err(AuditFailure::Permanent(e)) => {
            tracing::error!(event_type, error = %e, "audit delivery failed (permanent); rejecting");
            count("reject");
            MessageResult::Reject(format!("permanent audit error: {e}"))
        }
    }
}

/// `Retry`, or `Reject` on the last allowed attempt.
fn transient(event_type: &str, attempts: i16, e: &str) -> MessageResult {
    if i32::from(attempts) + 1 >= MAX_AUDIT_ATTEMPTS {
        tracing::error!(event_type, attempts, error = %e, "audit delivery attempts exhausted; rejecting");
        count("reject");
        return MessageResult::Reject(format!("audit delivery failed after {MAX_AUDIT_ATTEMPTS} attempts: {e}"));
    }
    tracing::warn!(event_type, attempts, error = %e, "audit delivery failed (transient); retrying");
    count("retry");
    MessageResult::Retry
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.handle_payload(&msg.payload, msg.attempts).await
    }
}
