//! Handler of the audit queue (DESIGN "Audit plugin and audit outbox", ADR-0009).

use std::sync::Arc;

use mini_chat_sdk::{TurnAuditEvent, TurnMutationAuditEvent};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use tracing::warn;

use crate::domain::metrics::Metrics;
use crate::infra::audit_gateway::{AuditDelivery, AuditGateway};

/// A transient failure is retried until this delivery attempt, then dead-lettered (about an hour
/// with the outbox backoff capped at 30 s).
const MAX_ATTEMPTS: u32 = 120;

/// An audit queue payload: a turn-finalization event or a turn-mutation event.
enum AuditPayload {
    Turn(Box<TurnAuditEvent>),
    Mutation(TurnMutationAuditEvent),
}

/// Reads a payload: a finalization event first, then a mutation event (their `event_type` values
/// are disjoint). The error is the finalization parse error of a payload neither type accepts.
fn decode(bytes: &[u8]) -> Result<AuditPayload, serde_json::Error> {
    match serde_json::from_slice::<TurnAuditEvent>(bytes) {
        Ok(event) => Ok(AuditPayload::Turn(Box::new(event))),
        Err(turn_err) => serde_json::from_slice::<TurnMutationAuditEvent>(bytes)
            .map(AuditPayload::Mutation)
            .map_err(|_| turn_err),
    }
}

/// Leased handler of the audit queue: deserializes first, then delivers through the gateway.
pub struct AuditOutboxHandler {
    gateway: Arc<AuditGateway>,
    metrics: Arc<dyn Metrics>,
}

impl AuditOutboxHandler {
    /// Creates the handler.
    #[must_use]
    pub fn new(gateway: Arc<AuditGateway>, metrics: Arc<dyn Metrics>) -> Self {
        Self { gateway, metrics }
    }

    fn retry_or_reject(&self, msg: &OutboxMessage, cause: &str) -> MessageResult {
        let delivery = u32::try_from(msg.attempts).unwrap_or(0).saturating_add(1);
        if delivery >= MAX_ATTEMPTS {
            self.metrics.audit_emit("reject");
            MessageResult::Reject(format!(
                "audit delivery failed after {MAX_ATTEMPTS} attempts: {cause}"
            ))
        } else {
            self.metrics.audit_emit("retry");
            warn!(cause = %cause, delivery, "audit delivery failed transiently; retrying");
            MessageResult::Retry
        }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for AuditOutboxHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload = match decode(&msg.payload) {
            Ok(payload) => payload,
            Err(err) => {
                self.metrics.audit_emit("reject");
                return MessageResult::Reject(format!("malformed audit payload: {err}"));
            }
        };
        let delivery = match payload {
            AuditPayload::Turn(event) => self.gateway.emit_turn(*event).await,
            AuditPayload::Mutation(event) => self.gateway.emit_mutation(event).await,
        };
        match delivery {
            AuditDelivery::Delivered => {
                self.metrics.audit_emit("ok");
                MessageResult::Ok
            }
            AuditDelivery::Dropped => {
                self.metrics.audit_emit("dropped");
                MessageResult::Ok
            }
            AuditDelivery::Retry(cause) => self.retry_or_reject(msg, &cause),
            AuditDelivery::Reject(cause) => {
                self.metrics.audit_emit("reject");
                MessageResult::Reject(format!("permanent audit failure: {cause}"))
            }
        }
    }
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
