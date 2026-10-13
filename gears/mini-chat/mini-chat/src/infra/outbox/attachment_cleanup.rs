//! The attachment cleanup queue: the JSON payload
//! (`cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload`) and the handler
//! that replaces the foundation placeholder
//! (`cpt-cf-mini-chat-algo-attachments-handle-cleanup`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::service::attachments::model::{AttachmentKind, CleanupState, StorageTarget};
use crate::domain::service::attachments::payload::{CleanupEvent, CleanupMessage, SecondaryRef};
use crate::domain::service::attachments::ports::{
    AttachmentMetrics, AttachmentRepository, BackgroundRuntime, StorageClient, StorageDirectory,
};
use crate::domain::service::quota::service::Clock;

/// A payload that cannot be decoded: an unknown `event_type`, a missing
/// required key, a non-UUID id, a wrong type, or not JSON at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed attachment cleanup payload: {0}")]
pub struct MalformedPayload(pub String);

#[derive(Serialize, Deserialize)]
struct SecondaryWire {
    file_id: String,
    provider_kind: String,
    upstream_alias: String,
}

#[derive(Serialize, Deserialize)]
struct Wire {
    event_type: String,
    tenant_id: Uuid,
    chat_id: Uuid,
    attachment_id: Uuid,
    provider_file_id: Option<String>,
    vector_store_id: Option<String>,
    storage_backend: String,
    attachment_kind: String,
    #[serde(with = "time::serde::rfc3339")]
    deleted_at: OffsetDateTime,
    secondary_ref: Option<SecondaryWire>,
}

/// The JSON object enqueued on `outbox.cleanup_queue_name`: exactly the keys
/// `event_type`, `tenant_id`, `chat_id`, `attachment_id`, `provider_file_id`
/// (string or `null`), `vector_store_id` (always `null`), `storage_backend`,
/// `attachment_kind`, `deleted_at` (RFC 3339 UTC) and `secondary_ref`
/// (`{ file_id, provider_kind: "anthropic", upstream_alias }` or `null`).
///
/// # Errors
///
/// Serialization failure (not expected).
pub fn encode_cleanup(message: &CleanupMessage) -> Result<Vec<u8>, serde_json::Error> {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload:p1:inst-build-payload
    serde_json::to_vec(&Wire {
        event_type: message.event.as_str().to_owned(),
        tenant_id: message.tenant_id,
        chat_id: message.chat_id,
        attachment_id: message.attachment_id,
        provider_file_id: message.provider_file_id.clone(),
        vector_store_id: None,
        storage_backend: message.storage_backend.clone(),
        attachment_kind: message.kind.as_str().to_owned(),
        deleted_at: message.deleted_at,
        secondary_ref: message
            .secondary_ref
            .as_ref()
            .map(|secondary| SecondaryWire {
                file_id: secondary.file_id.clone(),
                provider_kind: "anthropic".to_owned(),
                upstream_alias: secondary.upstream_alias.clone(),
            }),
    })
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload:p1:inst-build-payload
}

/// Decodes a payload for the handler.
///
/// # Errors
///
/// [`MalformedPayload`].
pub fn decode_cleanup(bytes: &[u8]) -> Result<CleanupMessage, MalformedPayload> {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload:p1:inst-decode-payload
    let wire: Wire =
        serde_json::from_slice(bytes).map_err(|error| MalformedPayload(error.to_string()))?;
    let event = CleanupEvent::parse(&wire.event_type)
        .ok_or_else(|| MalformedPayload(format!("event_type `{}`", wire.event_type)))?;
    let kind = AttachmentKind::parse(&wire.attachment_kind)
        .ok_or_else(|| MalformedPayload(format!("attachment_kind `{}`", wire.attachment_kind)))?;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload:p1:inst-decode-payload
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload:p1:inst-return-payload
    Ok(CleanupMessage {
        event,
        tenant_id: wire.tenant_id,
        chat_id: wire.chat_id,
        attachment_id: wire.attachment_id,
        provider_file_id: wire.provider_file_id,
        storage_backend: wire.storage_backend,
        kind,
        deleted_at: wire.deleted_at,
        secondary_ref: wire.secondary_ref.map(|secondary| SecondaryRef {
            file_id: secondary.file_id,
            upstream_alias: secondary.upstream_alias,
        }),
    })
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload:p1:inst-return-payload
}

/// What the handler is built from.
pub struct CleanupHandlerDeps {
    /// Attachment store.
    pub repo: Arc<dyn AttachmentRepository>,
    /// Provider storage client.
    pub storage: Arc<dyn StorageClient>,
    /// Provider entries by storage backend label.
    pub directory: Arc<dyn StorageDirectory>,
    /// Instruments.
    pub metrics: Arc<dyn AttachmentMetrics>,
    /// The S2S context of the gear.
    pub runtime: Arc<dyn BackgroundRuntime>,
    /// `cleanup_worker.max_attempts`.
    pub max_attempts: u32,
    /// Wall clock.
    pub clock: Clock,
    /// `PROVIDER_DELETE_TIMEOUT`.
    pub delete_timeout: Duration,
}

// @cpt-dod:cpt-cf-mini-chat-dod-attachments-cleanup-handler:p1
/// The handler of the attachment cleanup queue.
pub struct AttachmentCleanupHandler {
    deps: CleanupHandlerDeps,
}

/// How one provider delete ended.
enum Deleted {
    Done,
    Failed(String),
}

impl AttachmentCleanupHandler {
    /// Creates the handler.
    #[must_use]
    pub fn new(deps: CleanupHandlerDeps) -> Self {
        Self { deps }
    }

    /// Deletes the primary file of `message` through its storage backend.
    async fn delete_primary(
        &self,
        ctx: &SecurityContext,
        message: &CleanupMessage,
        file: &str,
    ) -> Deleted {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-delete-primary
        let Some(target): Option<StorageTarget> = self
            .deps
            .directory
            .storage_for_backend(&message.storage_backend, message.tenant_id)
        else {
            return Deleted::Failed("storage backend not configured".to_owned());
        };
        match tokio::time::timeout(
            self.deps.delete_timeout,
            self.deps.storage.delete_file(ctx, &target, file),
        )
        .await
        {
            Ok(Ok(())) => Deleted::Done,
            Ok(Err(error)) => Deleted::Failed(error.cause()),
            Err(_elapsed) => Deleted::Failed("timeout".to_owned()),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-delete-primary
    }

    /// Best-effort delete of the secondary copy; never blocks the outcome.
    async fn delete_secondary(&self, ctx: &SecurityContext, message: &CleanupMessage) {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-delete-secondary
        let Some(secondary) = &message.secondary_ref else {
            return;
        };
        let outcome = tokio::time::timeout(
            self.deps.delete_timeout,
            self.deps
                .storage
                .delete_secondary(ctx, &secondary.upstream_alias, &secondary.file_id),
        )
        .await;
        let failed = match outcome {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error.cause()),
            Err(_elapsed) => Some("timeout".to_owned()),
        };
        if let Some(cause) = failed {
            tracing::warn!(
                attachment_id = %message.attachment_id,
                cause,
                "the secondary copy was not deleted"
            );
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-cleanup-metrics
            self.deps.metrics.secondary_cleanup_skipped();
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-record-metrics:p1:inst-cleanup-metrics
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-delete-secondary
    }

    /// The row-less `attachment_orphan_file` variant.
    async fn handle_orphan(
        &self,
        ctx: &SecurityContext,
        message: &CleanupMessage,
        delivered: i16,
    ) -> MessageResult {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-orphan-variant
        let Some(file) = message.provider_file_id.as_deref() else {
            return MessageResult::Reject("an orphan message without a file".to_owned());
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-orphan-delete-primary
        match self.delete_primary(ctx, message, file).await {
            Deleted::Done => {
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-orphan-delete-secondary
                self.delete_secondary(ctx, message).await;
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-orphan-delete-secondary
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-orphan-return-ok
                self.deps.metrics.cleanup_completed();
                MessageResult::Ok
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-orphan-return-ok
            }
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-orphan-if-failed
            Deleted::Failed(cause) => {
                if i64::from(delivered) + 1 >= i64::from(self.deps.max_attempts) {
                    tracing::error!(
                        attachment_id = %message.attachment_id,
                        cause,
                        "an orphan provider file could not be deleted"
                    );
                    self.deps.metrics.cleanup_failed();
                    MessageResult::Reject(format!("orphan file delete failed: {cause}"))
                } else {
                    self.deps.metrics.cleanup_retry();
                    MessageResult::Retry
                }
            } // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-orphan-if-failed
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-orphan-delete-primary
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-orphan-variant
    }

    /// Row-bearing events.
    async fn handle_row(&self, ctx: &SecurityContext, message: &CleanupMessage) -> MessageResult {
        let repo = &self.deps.repo;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-check-chat
        match repo.chat_deleted(message.tenant_id, message.chat_id).await {
            Err(_) => return MessageResult::Retry,
            Ok(Some(false)) => {}
            Ok(Some(true) | None) => return MessageResult::Ok,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-check-chat
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-load-row
        let row = match repo
            .cleanup_row(message.tenant_id, message.chat_id, message.attachment_id)
            .await
        {
            Err(_) => return MessageResult::Retry,
            Ok(None) => {
                tracing::warn!(attachment_id = %message.attachment_id, "no row for a cleanup message");
                return MessageResult::Reject("attachment not found".to_owned());
            }
            Ok(Some(row)) => row,
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-load-row
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-if-terminal
        if matches!(row.state, Some(CleanupState::Done | CleanupState::Failed)) {
            return MessageResult::Ok;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-if-terminal
        let mut deleted = false;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-if-no-file
        if let Some(file) = message.provider_file_id.as_deref() {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-if-delete-failed
            if let Deleted::Failed(cause) = self.delete_primary(ctx, message, file).await {
                return self.record_failure(message, &cause).await;
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-if-delete-failed
            deleted = true;
            self.delete_secondary(ctx, message).await;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-if-no-file
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-mark-done
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-cleanup-status:p1:inst-to-done
        if repo
            .finish_cleanup(
                message.tenant_id,
                message.attachment_id,
                CleanupState::Done,
                (self.deps.clock)(),
            )
            .await
            .is_err()
        {
            return MessageResult::Retry;
        }
        // @cpt-end:cpt-cf-mini-chat-state-attachments-cleanup-status:p1:inst-to-done
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-mark-done
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-return-ok
        if deleted {
            self.deps.metrics.cleanup_completed();
        }
        MessageResult::Ok
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-return-ok
    }

    /// Counts a failed attempt; the last allowed one is terminal.
    async fn record_failure(&self, message: &CleanupMessage, cause: &str) -> MessageResult {
        let repo = &self.deps.repo;
        let counted = repo
            .record_cleanup_attempt(
                message.tenant_id,
                message.attachment_id,
                cause,
                (self.deps.clock)(),
            )
            .await;
        match counted {
            Err(_) => MessageResult::Retry,
            Ok(None) => MessageResult::Ok,
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-if-attempts-exhausted
            // @cpt-begin:cpt-cf-mini-chat-state-attachments-cleanup-status:p1:inst-to-failed
            Ok(Some(attempts)) if i64::from(attempts) >= i64::from(self.deps.max_attempts) => {
                if let Err(error) = repo
                    .finish_cleanup(
                        message.tenant_id,
                        message.attachment_id,
                        CleanupState::Failed,
                        (self.deps.clock)(),
                    )
                    .await
                {
                    tracing::error!(attachment_id = %message.attachment_id, %error, "the failed cleanup could not be recorded");
                }
                tracing::error!(attachment_id = %message.attachment_id, cause, "provider file cleanup failed for good");
                self.deps.metrics.cleanup_failed();
                MessageResult::Reject(format!("provider file delete failed: {cause}"))
            }
            // @cpt-end:cpt-cf-mini-chat-state-attachments-cleanup-status:p1:inst-to-failed
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-if-attempts-exhausted
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-return-retry
            Ok(Some(_)) => {
                self.deps.metrics.cleanup_retry();
                MessageResult::Retry
            } // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-return-retry
        }
    }
}

#[async_trait]
impl LeasedMessageHandler for AttachmentCleanupHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-decode-message
        let message = match decode_cleanup(&msg.payload) {
            Ok(message) => message,
            Err(error) => {
                tracing::error!(%error, "a malformed attachment cleanup payload is rejected");
                return MessageResult::Reject(error.to_string());
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-decode-message
        let Some(ctx) = self.deps.runtime.s2s_context() else {
            return MessageResult::Retry;
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-if-orphan-variant
        if message.event == CleanupEvent::AttachmentOrphanFile {
            self.handle_orphan(&ctx, &message, msg.attempts).await
        } else {
            self.handle_row(&ctx, &message).await
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-handle-cleanup:p1:inst-if-orphan-variant
    }
}

#[cfg(test)]
#[path = "attachment_cleanup_payload_tests.rs"]
mod payload_tests;

#[cfg(test)]
#[path = "attachment_cleanup_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "attachment_cleanup_orphan_tests.rs"]
mod orphan_tests;
