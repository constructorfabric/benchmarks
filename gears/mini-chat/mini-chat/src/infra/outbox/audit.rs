//! Audit outbox handler: delivers `MiniChatAuditEvent`s to the audit plugin
//! (spec §13.1, DESIGN §3.2 "Audit plugin and audit outbox", §5.6).

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::MiniChatAuditEvent;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use tracing::{debug, warn};

use crate::infra::gateways::audit::{AuditEmitResult, AuditGateway};

/// Delivery number (1-based) on which a `Retry` is turned into a `Reject`, so a
/// misconfigured plugin cannot block the partition forever (about an hour at the
/// outbox backoff cap of 30 s). `OutboxMessage::attempts` is 0-based, so the
/// 120th delivery has `attempts == 119`.
pub const AUDIT_MAX_DELIVERIES: i32 = 120;

/// Handler of the audit queue.
///
/// Deserializes the payload first: a corrupt payload is rejected whether or not a
/// plugin is registered. The gateway then resolves the plugin and emits with a
/// 30 s timeout: delivered or no plugin (dropped) -> `Ok`; transient error,
/// timeout, resolution failure -> `Retry` (`Reject` on the
/// [`AUDIT_MAX_DELIVERIES`]th delivery); permanent error -> `Reject`.
pub struct AuditHandler {
    audit: Arc<dyn AuditGateway>,
}

impl AuditHandler {
    #[must_use]
    pub fn new(audit: Arc<dyn AuditGateway>) -> Self {
        Self { audit }
    }
}

#[async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: MiniChatAuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(event) => event,
            Err(e) => {
                warn!(
                    partition = msg.partition_id,
                    seq = msg.seq,
                    error = %e,
                    "rejecting malformed audit outbox payload"
                );
                return MessageResult::Reject(format!("malformed audit payload: {e}"));
            }
        };
        match self.audit.emit(event).await {
            AuditEmitResult::Delivered => MessageResult::Ok,
            AuditEmitResult::Dropped => {
                debug!("audit event dropped: no audit plugin registered");
                MessageResult::Ok
            }
            AuditEmitResult::Retry(reason)
                if i32::from(msg.attempts) + 1 >= AUDIT_MAX_DELIVERIES =>
            {
                warn!(attempts = msg.attempts, %reason, "audit delivery attempts exhausted; dead-lettering");
                MessageResult::Reject(format!(
                    "audit delivery failed after {} deliveries: {reason}",
                    i32::from(msg.attempts) + 1
                ))
            }
            AuditEmitResult::Retry(reason) => {
                warn!(attempts = msg.attempts, %reason, "audit delivery failed; will retry");
                MessageResult::Retry
            }
            AuditEmitResult::Reject(reason) => {
                warn!(%reason, "audit event permanently rejected");
                MessageResult::Reject(reason)
            }
        }
    }
}
