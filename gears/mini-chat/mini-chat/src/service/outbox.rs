//! Outbox integration: payload types, enqueue helpers, the usage / audit
//! handlers and pipeline start-up (five leased queues).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditEvent, MiniChatAuditPluginError, PublishError, UsageEvent};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::outbox::{
    LeaseConfig, LeasedMessageHandler, MessageResult, Outbox, OutboxError, OutboxHandle,
    OutboxMessage, OutboxProfile, Partitions, Record, Wake,
};
use uuid::Uuid;

use super::AppState;
use crate::domain::error::DomainError;

pub const PT_USAGE: &str = "application/json;mini_chat.usage_event.v1";
pub const PT_AUDIT: &str = "application/json;mini_chat.audit_event.v1";
pub const PT_ATTACHMENT_CLEANUP: &str = "application/json;mini_chat.attachment_cleanup.v1";
pub const PT_CHAT_CLEANUP: &str = "application/json;mini_chat.chat_cleanup.v1";
pub const PT_THREAD_SUMMARY: &str = "application/json;mini_chat.thread_summary.v1";

/// Audit handler gives up (dead letter) on this attempt.
const AUDIT_MAX_ATTEMPTS: i16 = 120;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecondaryRef {
    pub file_id: String,
    pub provider_kind: String,
    pub upstream_alias: String,
}

/// Attachment cleanup message (`attachment_deleted`,
/// `attachment_upload_abandoned`, `attachment_indexing_failed`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentCleanupEvent {
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: Option<String>,
    pub vector_store_id: Option<String>,
    pub storage_backend: String,
    pub attachment_kind: String,
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    pub secondary_ref: Option<SecondaryRef>,
}

/// Chat cleanup message emitted by the chat soft-delete.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatCleanupEvent {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    pub reason: String,
    #[serde(with = "time::serde::rfc3339")]
    pub chat_deleted_at: OffsetDateTime,
}

/// Durable thread-summary work item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadSummaryTask {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub system_request_id: Uuid,
    #[serde(with = "time::serde::rfc3339::option")]
    pub base_frontier_created_at: Option<OffsetDateTime>,
    pub base_frontier_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub frozen_target_created_at: OffsetDateTime,
    pub frozen_target_message_id: Uuid,
    pub system_task_type: String,
}

#[derive(Debug, thiserror::Error)]
pub enum EnqueueError {
    #[error("outbox payload too large: {0}")]
    TooLarge(String),
    #[error("outbox enqueue failed: {0}")]
    Other(String),
}

impl From<EnqueueError> for DomainError {
    fn from(e: EnqueueError) -> Self {
        DomainError::internal(e.to_string())
    }
}

/// Stable partition of a key.
#[must_use]
pub fn partition_for(key: Uuid, num_partitions: u32) -> u32 {
    let n = u128::from(num_partitions.max(1));
    u32::try_from(key.as_u128() % n).unwrap_or(0)
}

/// Serialize and enqueue inside the caller's transaction.
///
/// # Errors
/// [`EnqueueError`] when serialization or the write fails.
pub async fn enqueue_json<T: Serialize>(
    outbox: &Outbox,
    tx: &DbTx<'_>,
    queue: &str,
    partition: u32,
    payload_type: &str,
    value: &T,
) -> Result<Wake, EnqueueError> {
    let bytes = serde_json::to_vec(value).map_err(|e| EnqueueError::Other(e.to_string()))?;
    let record = Record::to(queue, partition)
        .payload(bytes, payload_type)
        .build()
        .map_err(|e| match e {
            OutboxError::PayloadTooLarge { .. } => EnqueueError::TooLarge(e.to_string()),
            other => EnqueueError::Other(other.to_string()),
        })?;
    outbox.enqueue(tx, record).await.map_err(|e| match e {
        OutboxError::PayloadTooLarge { .. } => EnqueueError::TooLarge(e.to_string()),
        other => EnqueueError::Other(other.to_string()),
    })
}

impl AppState {
    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn enqueue_usage(
        &self,
        outbox: &Outbox,
        tx: &DbTx<'_>,
        ev: &UsageEvent,
    ) -> Result<Wake, EnqueueError> {
        enqueue_json(
            outbox,
            tx,
            &self.cfg.outbox.queue_name,
            partition_for(ev.tenant_id, self.cfg.outbox.num_partitions),
            PT_USAGE,
            ev,
        )
        .await
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn enqueue_audit(
        &self,
        outbox: &Outbox,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        ev: &MiniChatAuditEvent,
    ) -> Result<Wake, EnqueueError> {
        enqueue_json(
            outbox,
            tx,
            &self.cfg.outbox.audit_queue_name,
            partition_for(tenant_id, self.cfg.outbox.num_partitions),
            PT_AUDIT,
            ev,
        )
        .await
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn enqueue_attachment_cleanup(
        &self,
        outbox: &Outbox,
        tx: &DbTx<'_>,
        ev: &AttachmentCleanupEvent,
    ) -> Result<Wake, EnqueueError> {
        enqueue_json(
            outbox,
            tx,
            &self.cfg.outbox.cleanup_queue_name,
            partition_for(ev.tenant_id, self.cfg.outbox.num_partitions),
            PT_ATTACHMENT_CLEANUP,
            ev,
        )
        .await
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn enqueue_chat_cleanup(
        &self,
        outbox: &Outbox,
        tx: &DbTx<'_>,
        ev: &ChatCleanupEvent,
    ) -> Result<Wake, EnqueueError> {
        enqueue_json(
            outbox,
            tx,
            &self.cfg.outbox.chat_cleanup_queue_name,
            partition_for(ev.chat_id, self.cfg.outbox.num_partitions),
            PT_CHAT_CLEANUP,
            ev,
        )
        .await
    }

    /// # Errors
    /// Returns the domain error of the step that failed (validation,
    /// authorization, persistence or a downstream dependency).
    pub async fn enqueue_thread_summary(
        &self,
        outbox: &Outbox,
        tx: &DbTx<'_>,
        ev: &ThreadSummaryTask,
    ) -> Result<Wake, EnqueueError> {
        enqueue_json(
            outbox,
            tx,
            &self.cfg.outbox.thread_summary_queue_name,
            partition_for(ev.chat_id, self.cfg.outbox.num_partitions),
            PT_THREAD_SUMMARY,
            ev,
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// Usage handler
// ---------------------------------------------------------------------------

pub struct UsageHandler {
    pub state: Arc<AppState>,
}

#[async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed usage payload: {e}")),
        };
        let plugin = match self.state.policy.plugin().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "usage publish: policy plugin unavailable");
                return MessageResult::Retry;
            }
        };
        match plugin.publish_usage(ev).await {
            Ok(()) => MessageResult::Ok,
            Err(PublishError::Transient(e)) => {
                tracing::warn!(error = %e, "usage publish transient failure");
                MessageResult::Retry
            }
            Err(PublishError::Permanent(e)) => MessageResult::Reject(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Audit handler
// ---------------------------------------------------------------------------

pub struct AuditHandler {
    pub state: Arc<AppState>,
}

impl AuditHandler {
    fn retry_or_reject(msg: &OutboxMessage, why: &str) -> MessageResult {
        if msg.attempts + 1 >= AUDIT_MAX_ATTEMPTS {
            MessageResult::Reject(format!(
                "audit delivery gave up after {AUDIT_MAX_ATTEMPTS} attempts: {why}"
            ))
        } else {
            MessageResult::Retry
        }
    }
}

#[async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: MiniChatAuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed audit payload: {e}")),
        };
        let plugin = match self.state.audit.plugin().await {
            Ok(Some(p)) => p,
            Ok(None) => return MessageResult::Ok,
            Err(e) => {
                tracing::warn!(error = %e, "audit plugin resolution failed");
                return Self::retry_or_reject(msg, &e.to_string());
            }
        };
        match tokio::time::timeout(Duration::from_secs(30), plugin.emit(ev)).await {
            Ok(Ok(())) => MessageResult::Ok,
            Ok(Err(MiniChatAuditPluginError::Permanent(e))) => MessageResult::Reject(e),
            Ok(Err(e)) => Self::retry_or_reject(msg, &e.to_string()),
            Err(_) => Self::retry_or_reject(msg, "audit plugin timeout"),
        }
    }
}

/// Build and start the outbox pipeline with the five mini-chat queues.
///
/// # Errors
/// Pipeline start failure.
pub async fn start_outbox(state: &Arc<AppState>) -> Result<OutboxHandle, OutboxError> {
    let cfg = &state.cfg;
    let parts = Partitions::of(u16::try_from(cfg.outbox.num_partitions).unwrap_or(4));
    let summary_lease = Duration::from_secs(cfg.thread_summary_worker.claim_timeout_secs);
    Outbox::builder(state.db.clone())
        .profile(OutboxProfile::low_latency())
        .queue(&cfg.outbox.queue_name, parts)
        .leased(UsageHandler {
            state: Arc::clone(state),
        })
        .queue(&cfg.outbox.cleanup_queue_name, parts)
        .leased(super::cleanup::AttachmentCleanupHandler {
            state: Arc::clone(state),
        })
        .queue(&cfg.outbox.chat_cleanup_queue_name, parts)
        .leased(super::cleanup::ChatCleanupHandler {
            state: Arc::clone(state),
        })
        .queue(&cfg.outbox.thread_summary_queue_name, parts)
        .leased(super::summary::ThreadSummaryHandler {
            state: Arc::clone(state),
        })
        .lease(LeaseConfig {
            duration: summary_lease,
            headroom: Duration::from_secs(2),
        })
        .queue(&cfg.outbox.audit_queue_name, parts)
        .leased(AuditHandler {
            state: Arc::clone(state),
        })
        .lease(LeaseConfig {
            duration: Duration::from_secs(60),
            headroom: Duration::from_secs(2),
        })
        .start()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partitions_are_stable_and_in_range() {
        let id = Uuid::from_u128(0x1234_5678);
        assert_eq!(partition_for(id, 4), partition_for(id, 4));
        for i in 0..100u128 {
            assert!(partition_for(Uuid::from_u128(i * 7919), 4) < 4);
        }
        assert_eq!(partition_for(id, 1), 0);
    }

    #[test]
    fn cleanup_payload_shape() {
        let ev = AttachmentCleanupEvent {
            event_type: "attachment_deleted".into(),
            tenant_id: Uuid::nil(),
            chat_id: Uuid::nil(),
            attachment_id: Uuid::nil(),
            provider_file_id: None,
            vector_store_id: None,
            storage_backend: "openai".into(),
            attachment_kind: "document".into(),
            deleted_at: OffsetDateTime::UNIX_EPOCH,
            secondary_ref: None,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert!(v["provider_file_id"].is_null());
        assert!(v["vector_store_id"].is_null());
        assert_eq!(v["event_type"], "attachment_deleted");
    }
}
