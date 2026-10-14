//! The attachment cleanup handler of the `mini-chat.attachment_cleanup` queue
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler` and the cleanup status
//! state machine `cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status`).
//!
//! [`AttachmentCleanupWorker::handle`] decides one leased message: `Reject` for a malformed
//! payload, `Ok` without action for a soft-deleted or missing parent chat, a missing row or a
//! `done` row, `done` for a null provider file id, the provider delete through OAGW (2xx and 404
//! are success), `cleanup_attempts` plus `Retry` below `cleanup_worker.max_attempts` and `failed`
//! plus `Reject` at the limit, `Retry` without increment on a database error, the best-effort
//! secondary delete, no Vector Stores call, and, for a `failed` row (a dead-letter replay), exactly
//! one provider delete re-attempt. `infra::attachment_cleanup` adapts the outcome to the shared
//! outbox.

use std::sync::Arc;

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::config::StorageKind;
use crate::domain::attachments::metrics::AttachmentMetrics;
use crate::domain::attachments::provider::{ProviderFailure, StorageProvider, StorageTarget};
use crate::domain::attachments::store::SecondaryRef;
use crate::domain::error::DomainError;
use crate::domain::provider_registry::ProviderRegistry;
use crate::domain::quota::Clock;

/// The result of one delivery, mapped by the infrastructure adapter to the shared outbox
/// (`Ok`, `Retry`, `Reject(reason)`).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    Ok,
    Retry,
    Reject(String),
}

/// The attachment cleanup message as the queue serializes it (DESIGN 4 `Attachment Deletion`,
/// Phase 1 table).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupPayload {
    /// `attachment_deleted`, `attachment_upload_abandoned` or `attachment_indexing_failed`.
    pub event_type: String,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    /// `None` when the upload never reached the provider.
    pub provider_file_id: Option<String>,
    /// The `storage_backend` label that selects the storage provider.
    pub storage_backend: String,
    pub secondary_ref: Option<SecondaryRef>,
}

/// Parse a queue payload; the error text is the dead-letter reason of a malformed message.
///
/// # Errors
///
/// The reason text when `bytes` are not a valid cleanup payload (not JSON, a missing or mistyped
/// field, an unknown `event_type`).
pub fn parse_payload(bytes: &[u8]) -> Result<CleanupPayload, String> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("the payload is not JSON: {e}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "the payload is not an object".to_owned())?;
    let text = |key: &str| -> Result<String, String> {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("`{key}` is missing or not a string"))
    };
    let id = |key: &str| -> Result<Uuid, String> {
        Uuid::parse_str(&text(key)?).map_err(|_| format!("`{key}` is not a UUID"))
    };
    let event_type = text("event_type")?;
    if !matches!(
        event_type.as_str(),
        "attachment_deleted" | "attachment_upload_abandoned" | "attachment_indexing_failed"
    ) {
        return Err(format!("unknown event type `{event_type}`"));
    }
    let provider_file_id = match object.get("provider_file_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(file)) => Some(file.clone()),
        Some(_) => return Err("`provider_file_id` is not a string".to_owned()),
    };
    let secondary_ref = match object.get("secondary_ref") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(secondary)) => {
            let field = |key: &str| -> Result<String, String> {
                secondary
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| format!("`secondary_ref.{key}` is missing"))
            };
            Some(SecondaryRef {
                file_id: field("file_id")?,
                provider_kind: field("provider_kind")?,
                upstream_alias: field("upstream_alias")?,
            })
        }
        Some(_) => return Err("`secondary_ref` is not an object".to_owned()),
    };
    Ok(CleanupPayload {
        event_type,
        tenant_id: id("tenant_id")?,
        chat_id: id("chat_id")?,
        attachment_id: id("attachment_id")?,
        provider_file_id,
        storage_backend: text("storage_backend")?,
        secondary_ref,
    })
}

/// The storage provider that the `storage_backend` label of a payload maps to for `tenant_id`
/// (the storage dispatcher): the provider entry whose `storage_backend` equals the label, with
/// the tenant's override applied. `None` for a label no provider serves; that counts as a failed
/// attempt of the handler.
#[must_use]
pub fn storage_target_for_backend(
    providers: &ProviderRegistry,
    tenant_id: Uuid,
    storage_backend: &str,
) -> Option<StorageTarget> {
    let entry = providers
        .entries()
        .find(|entry| entry.base.storage_backend == storage_backend)?;
    let provider = providers.resolve(&entry.base.provider_id, tenant_id).ok()?;
    Some(StorageTarget {
        provider_id: provider.provider_id.clone(),
        upstream_alias: provider.upstream_alias.clone(),
        storage_kind: provider.storage_kind,
        api_version: provider.api_version.clone(),
        storage_backend: provider.storage_backend.clone(),
    })
}

/// The Anthropic Files target of a `secondary_ref`: its `upstream_alias`, the `/v1` file prefix.
#[must_use]
pub fn secondary_target(secondary: &SecondaryRef) -> StorageTarget {
    StorageTarget {
        provider_id: secondary.provider_kind.clone(),
        upstream_alias: secondary.upstream_alias.clone(),
        storage_kind: StorageKind::Openai,
        api_version: None,
        storage_backend: secondary.provider_kind.clone(),
    }
}

/// The cleanup columns of an `attachments` row that the handler reads.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupRow {
    /// `None`, `pending`, `done` or `failed`.
    pub cleanup_status: Option<String>,
    pub cleanup_attempts: i32,
}

/// What recording a failed provider delete of a `pending` row did.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptRecord {
    /// `cleanup_attempts` after the increment.
    pub attempts: i32,
    /// `cleanup_attempts` reached `max_attempts` and the row is now `failed`.
    pub exhausted: bool,
}

/// The status a guarded `done` write expects.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupFrom {
    /// The normal path: `pending` to `done`.
    Pending,
    /// The dead-letter replay: `failed` to `done`.
    Failed,
}

/// The persistence of the handler. Every method filters on `tenant_id` (the handler runs outside
/// a user scope) and every write is a guarded conditional update.
#[async_trait]
pub trait CleanupStore: Send + Sync {
    /// Whether the parent chat exists and is not soft-deleted (the deleted filter is NOT applied
    /// by the read itself).
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn chat_is_live(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, DomainError>;

    /// The cleanup columns of the row by `tenant_id`, `chat_id` and `attachment_id`; `None` for a
    /// missing row.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn load_row(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<Option<CleanupRow>, DomainError>;

    /// Guarded by `cleanup_status` `pending`: `cleanup_attempts` plus 1, `last_cleanup_error` and
    /// `cleanup_updated_at`; when `cleanup_attempts` reaches `max_attempts` the same guarded write
    /// sets `cleanup_status` `failed`. `None` when the guard matched no row.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn record_failed_attempt(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        cause: &str,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<Option<AttemptRecord>, DomainError>;

    /// Guarded by `from`: `cleanup_status` `done` and `cleanup_updated_at`; whether a row was
    /// updated.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn mark_done(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        from: CleanupFrom,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// Guarded by `cleanup_status` `failed`: `last_cleanup_error` and `cleanup_updated_at`;
    /// `cleanup_status` and `cleanup_attempts` are unchanged. Whether a row was updated.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn record_replay_failure(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        cause: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;
}

/// The collaborators of [`AttachmentCleanupWorker`].
#[domain_model]
pub struct CleanupDeps {
    pub store: Arc<dyn CleanupStore>,
    /// The Files API client of the storage providers (primary file delete).
    pub storage: Arc<dyn StorageProvider>,
    /// The Anthropic Files client; `None` records `mini_chat_secondary_cleanup_skipped`.
    pub secondary_storage: Option<Arc<dyn StorageProvider>>,
    pub providers: Arc<ProviderRegistry>,
    pub clock: Arc<dyn Clock>,
    pub metrics: Arc<dyn AttachmentMetrics>,
    /// `cleanup_worker.max_attempts` (default 5).
    pub max_attempts: u32,
}

/// The attachment cleanup handler logic.
#[domain_model]
pub struct AttachmentCleanupWorker {
    deps: CleanupDeps,
}

impl AttachmentCleanupWorker {
    /// Create the worker over its collaborators.
    #[must_use]
    pub fn new(deps: CleanupDeps) -> Self {
        Self { deps }
    }

    /// Decide one leased message.
    pub async fn handle(&self, payload: &[u8]) -> CleanupOutcome {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-parse
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-malformed
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-malformed
        let payload = match parse_payload(payload) {
            Ok(payload) => payload,
            Err(reason) => {
                tracing::error!(%reason, "the attachment cleanup message is malformed");
                return CleanupOutcome::Reject(format!("malformed cleanup payload: {reason}"));
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-malformed
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-malformed
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-parse
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-load-chat
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-chat-deleted
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-chat-owned
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-db-error
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-db-retry
        // a soft-deleted or missing parent chat is owned by chat cleanup
        match d
            .store
            .chat_is_live(payload.tenant_id, payload.chat_id)
            .await
        {
            Ok(true) => {}
            Ok(false) => return CleanupOutcome::Ok,
            Err(error) => return Self::infrastructure(&error),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-db-retry
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-db-error
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-chat-owned
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-chat-deleted
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-load-chat
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-load-row
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-db-error
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-db-retry
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-terminal
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-terminal
        // a missing or finished row is a redelivery or a replay of a finished message
        let row = match d
            .store
            .load_row(payload.tenant_id, payload.chat_id, payload.attachment_id)
            .await
        {
            Ok(Some(row)) if row.cleanup_status.as_deref() != Some("done") => row,
            Ok(Some(_) | None) => return CleanupOutcome::Ok,
            Err(error) => return Self::infrastructure(&error),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-terminal
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-terminal
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-db-retry
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-db-error
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-load-row
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-no-file
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-no-file
        let Some(file_id) = payload.provider_file_id.clone() else {
            return self.finish(&payload, CleanupFrom::Pending).await;
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-no-file
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-no-file
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-replay
        // a `failed` row means a dead-letter replay (or a redelivery after the exhausting
        // delivery crashed before its `Reject`): one delete attempt, never `Retry`
        if row.cleanup_status.as_deref() == Some("failed") {
            return self.replay(&payload, &file_id).await;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-replay
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-delete-file
        match self.delete_primary(&payload, &file_id).await {
            Ok(()) => {}
            Err(cause) => return self.attempt_failed(&payload, &cause).await,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-delete-file
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-secondary
        self.delete_secondary(&payload).await;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-secondary
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-no-vector-store
        // no Vector Stores API call: the document leaves the chat vector store with its provider
        // file, and the store itself is deleted by chat cleanup
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-no-vector-store
        self.finish(&payload, CleanupFrom::Pending).await
    }

    fn infrastructure(error: &DomainError) -> CleanupOutcome {
        tracing::warn!(%error, "an attachment cleanup database error; the message is retried");
        CleanupOutcome::Retry
    }

    /// The dead-letter replay of a `failed` row: one provider delete; 2xx or 404 moves the row to
    /// `done`, a failure keeps it `failed` and rejects the message again.
    async fn replay(&self, payload: &CleanupPayload, file_id: &str) -> CleanupOutcome {
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability:p1:inst-adm-replay
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-replay-delete
        let deleted = self.delete_primary(payload, file_id).await;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-replay-delete
        match deleted {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-replay-ok
            Ok(()) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-replay-secondary
                self.delete_secondary(payload).await;
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-replay-secondary
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-replay-done
                // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-replay-done
                self.finish(payload, CleanupFrom::Failed).await
                // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-replay-done
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-replay-done
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-replay-ok
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-replay-failed
            // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-replay-failed
            Err(cause) => self.replay_failed(payload, &cause).await,
            // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-replay-failed
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-replay-failed
        }
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability:p1:inst-adm-replay
    }

    /// The provider delete: 2xx and 404 are success; the cause holds no provider text. An
    /// unresolvable `storage_backend` counts as a failed attempt.
    async fn delete_primary(&self, payload: &CleanupPayload, file_id: &str) -> Result<(), String> {
        let d = &self.deps;
        let Some(target) =
            storage_target_for_backend(&d.providers, payload.tenant_id, &payload.storage_backend)
        else {
            return Err("the storage backend maps to no provider".to_owned());
        };
        match d.storage.delete_file(&target, file_id).await {
            Ok(()) | Err(ProviderFailure::NotFound) => Ok(()),
            Err(ProviderFailure::Transient) => {
                Err("the provider delete failed transiently".to_owned())
            }
            Err(ProviderFailure::Other) => Err("the provider rejected the delete".to_owned()),
        }
    }

    /// The best-effort secondary delete; a missing Anthropic Files client is counted.
    async fn delete_secondary(&self, payload: &CleanupPayload) {
        let d = &self.deps;
        let Some(secondary) = &payload.secondary_ref else {
            return;
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-delete-secondary
        let Some(client) = &d.secondary_storage else {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-cleanup
            d.metrics
                .secondary_cleanup_skipped(&secondary.provider_kind);
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-cleanup
            return;
        };
        if let Err(failure) = client
            .delete_secondary_file(&secondary_target(secondary), &secondary.file_id)
            .await
        {
            tracing::warn!(
                ?failure,
                "the secondary file delete failed; the cleanup goes on"
            );
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-delete-secondary
    }

    /// The guarded `done` write and the completion metric.
    async fn finish(&self, payload: &CleanupPayload, from: CleanupFrom) -> CleanupOutcome {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-mark-done
        // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-done
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-ok
        match d
            .store
            .mark_done(
                payload.tenant_id,
                payload.attachment_id,
                from,
                d.clock.now(),
            )
            .await
        {
            Ok(true) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-cleanup
                d.metrics.cleanup_completed();
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-cleanup
                CleanupOutcome::Ok
            }
            Ok(false) => CleanupOutcome::Ok,
            Err(error) => Self::infrastructure(&error),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-ok
        // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-done
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-mark-done
    }

    /// A failed provider delete of a `pending` row: the attempt is recorded, then `Retry` below
    /// `max_attempts` and `failed` plus `Reject` at the limit.
    async fn attempt_failed(&self, payload: &CleanupPayload, cause: &str) -> CleanupOutcome {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-delete-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-record-attempt
        let recorded = d
            .store
            .record_failed_attempt(
                payload.tenant_id,
                payload.attachment_id,
                cause,
                d.max_attempts,
                d.clock.now(),
            )
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-record-attempt
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-delete-failed
        match recorded {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-exhausted
            // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-failed
            Ok(Some(record)) if record.exhausted => {
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-cleanup
                d.metrics.cleanup_failed();
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-cleanup
                CleanupOutcome::Reject(format!("the cleanup failed {} times", record.attempts))
            }
            // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-failed
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-exhausted
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-retry
            // @cpt-begin:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-retry
            Ok(Some(_)) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-cleanup
                d.metrics.cleanup_retry("provider_error");
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-cleanup
                CleanupOutcome::Retry
            }
            Ok(None) => CleanupOutcome::Retry,
            // an infrastructure error leaves `cleanup_attempts` unchanged
            Err(error) => Self::infrastructure(&error),
            // @cpt-end:cpt-cf-mini-chat-state-attachments-and-file-search-cleanup-status:p1:inst-cs-retry
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-cleanup-handler:p1:inst-ch-return-retry
        }
    }

    /// A failed replay: `last_cleanup_error` refreshed, `cleanup_status` and `cleanup_attempts`
    /// unchanged, no failure or retry metric, and the message is dead-lettered again.
    async fn replay_failed(&self, payload: &CleanupPayload, cause: &str) -> CleanupOutcome {
        let d = &self.deps;
        match d
            .store
            .record_replay_failure(
                payload.tenant_id,
                payload.attachment_id,
                cause,
                d.clock.now(),
            )
            .await
        {
            Ok(_) => CleanupOutcome::Reject(format!("the replayed cleanup failed again: {cause}")),
            Err(error) => Self::infrastructure(&error),
        }
    }
}

#[cfg(test)]
#[path = "cleanup_tests.rs"]
mod cleanup_tests;
