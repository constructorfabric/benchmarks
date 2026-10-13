//! Audit outbox handler (`cpt-cf-mini-chat-algo-gear-foundation-handle-audit`).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditEvent, TurnAuditEvent, TurnMutationAuditEvent,
    decode_audit_event, encode_audit_event,
};
use toolkit_db::DbTx;
use toolkit_db::outbox::{
    LeasedMessageHandler, MessageResult, OutboxError, OutboxMessage, Record, Wake,
};

use crate::domain::error::DomainError;
use crate::infra::outbox::queues::{OutboxEnqueuer, QueueKind};

use crate::infra::audit_gateway::{AuditGateway, AuditGatewayError};
use crate::infra::metrics::{AuditEmitMetrics, AuditEmitResult};

/// Period of the "no audit plugin" warning.
const NO_PLUGIN_WARN_PERIOD: Duration = Duration::from_secs(60);

/// A `Retry` on this delivery attempt (1-based, so `attempts == 119`) is
/// turned into `Reject`.
pub const AUDIT_MAX_DELIVERY_ATTEMPTS: i16 = 120;

/// Delivers audit events to the audit plugin.
pub struct AuditHandler {
    gateway: Arc<AuditGateway>,
    metrics: Arc<dyn AuditEmitMetrics>,
    last_no_plugin_warning: Mutex<Option<Instant>>,
}

impl AuditHandler {
    /// Creates the handler.
    #[must_use]
    pub fn new(gateway: Arc<AuditGateway>, metrics: Arc<dyn AuditEmitMetrics>) -> Self {
        Self {
            gateway,
            metrics,
            last_no_plugin_warning: Mutex::new(None),
        }
    }

    fn warn_no_plugin(&self) {
        let Ok(mut last) = self.last_no_plugin_warning.lock() else {
            return;
        };
        let due = last.is_none_or(|at| at.elapsed() >= NO_PLUGIN_WARN_PERIOD);
        if due {
            *last = Some(Instant::now());
            tracing::warn!(
                "no audit plugin is registered, audit events are acknowledged and dropped"
            );
        }
    }
}

#[async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-decode-audit
        let decoded = decode_audit_event(&msg.payload);
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-decode-audit
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-if-audit-malformed
        let event = match decoded {
            Ok(event) => event,
            Err(error) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-reject-malformed-audit
                self.metrics.record_audit_emit(AuditEmitResult::Reject);
                return MessageResult::Reject(error.to_string());
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-reject-malformed-audit
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-if-audit-malformed
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-resolve-audit
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-emit
        let delivered = self.gateway.emit(event).await;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-emit
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-resolve-audit
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-match-emit
        match delivered {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-case-emit-ok
            Ok(()) => {
                self.metrics.record_audit_emit(AuditEmitResult::Ok);
                return MessageResult::Ok;
            }
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-case-emit-ok
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-if-no-audit-plugin
            Err(AuditGatewayError::NotRegistered) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-drop-event
                self.warn_no_plugin();
                self.metrics.record_audit_emit(AuditEmitResult::Dropped);
                return MessageResult::Ok;
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-drop-event
            }
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-if-no-audit-plugin
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-case-emit-permanent
            Err(AuditGatewayError::Plugin(AuditPluginError::Permanent(reason))) => {
                self.metrics.record_audit_emit(AuditEmitResult::Reject);
                return MessageResult::Reject(reason);
            }
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-case-emit-permanent
            Err(AuditGatewayError::Resolution(_) | AuditGatewayError::Plugin(_)) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-if-resolution-failed
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-resolution-retry
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-case-emit-transient
                tracing::debug!(attempts = msg.attempts, "audit delivery failed transiently");
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-case-emit-transient
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-resolution-retry
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-if-resolution-failed
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-match-emit
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-if-last-attempt
        if msg.attempts.saturating_add(1) >= AUDIT_MAX_DELIVERY_ATTEMPTS {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-reject-last-attempt
            self.metrics.record_audit_emit(AuditEmitResult::Reject);
            return MessageResult::Reject(format!(
                "audit delivery gave up after {AUDIT_MAX_DELIVERY_ATTEMPTS} attempts"
            ));
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-reject-last-attempt
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-if-last-attempt
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-return-audit-retry
        self.metrics.record_audit_emit(AuditEmitResult::Retry);
        MessageResult::Retry
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-handle-audit:p1:inst-return-audit-retry
    }
}

/// Encodes `event` and enqueues it to the audit queue (`outbox.audit_queue_name`),
/// partitioned by `tenant_id`, inside the caller's transaction
/// (`cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event`). The caller fires
/// the returned [`Wake`] after the commit.
///
/// # Errors
///
/// [`DomainError::Internal`] when the event cannot be encoded, the pipeline is
/// not running or the queue is not registered; [`DomainError::Database`]
/// (driver text preserved, so contention is retryable) when the insert fails.
pub async fn enqueue_audit_event(
    tx: &DbTx<'_>,
    outbox: &OutboxEnqueuer,
    event: &TurnAuditEvent,
) -> Result<Wake, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event:p1:inst-audit-encode
    let payload = encode_audit_event(&MiniChatAuditEvent::Turn(event.clone()))
        .map_err(|error| DomainError::Internal(format!("audit event encoding failed: {error}")))?;
    // @cpt-end:cpt-cf-mini-chat-algo-streaming-turns-turn-audit-event:p1:inst-audit-encode
    let running = outbox
        .outbox()
        .ok_or_else(|| DomainError::Internal("the outbox pipeline is not running".to_owned()))?;
    let queue = outbox.queue_name(QueueKind::Audit).to_owned();
    let partition = outbox.partition(QueueKind::Audit, event.tenant_id, event.chat_id);
    let record = Record::to(&queue, partition)
        .payload(payload, "application/json")
        .build()
        .map_err(|error| DomainError::Internal(format!("audit enqueue failed: {error}")))?;
    running
        .enqueue(tx, record)
        .await
        .map_err(|error| match error {
            OutboxError::Database(db_error) => DomainError::Database(db_error.to_string()),
            other => DomainError::Internal(format!("audit enqueue failed: {other}")),
        })
}

/// Encodes the mutation `event` and enqueues it to the audit queue
/// (`outbox.audit_queue_name`), partitioned by `tenant_id`, inside the
/// caller's transaction
/// (`cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event`). The caller
/// fires the returned [`Wake`] after the commit.
///
/// # Errors
///
/// As [`enqueue_audit_event`].
pub async fn enqueue_mutation_audit_event(
    tx: &DbTx<'_>,
    outbox: &OutboxEnqueuer,
    event: &TurnMutationAuditEvent,
) -> Result<Wake, DomainError> {
    let (tenant_id, chat_id) = match event {
        TurnMutationAuditEvent::TurnRetry {
            tenant_id, chat_id, ..
        }
        | TurnMutationAuditEvent::TurnEdit {
            tenant_id, chat_id, ..
        }
        | TurnMutationAuditEvent::TurnDelete {
            tenant_id, chat_id, ..
        } => (*tenant_id, *chat_id),
    };
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-encode-mutation-event
    let payload = encode_audit_event(&MiniChatAuditEvent::Mutation(event.clone()))
        .map_err(|error| DomainError::Internal(format!("audit event encoding failed: {error}")))?;
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-mutation-audit-event:p1:inst-encode-mutation-event
    let running = outbox
        .outbox()
        .ok_or_else(|| DomainError::Internal("the outbox pipeline is not running".to_owned()))?;
    let queue = outbox.queue_name(QueueKind::Audit).to_owned();
    let partition = outbox.partition(QueueKind::Audit, tenant_id, chat_id);
    let record = Record::to(&queue, partition)
        .payload(payload, "application/json")
        .build()
        .map_err(|error| DomainError::Internal(format!("audit enqueue failed: {error}")))?;
    running
        .enqueue(tx, record)
        .await
        .map_err(|error| match error {
            OutboxError::Database(db_error) => DomainError::Database(db_error.to_string()),
            other => DomainError::Internal(format!("audit enqueue failed: {other}")),
        })
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
