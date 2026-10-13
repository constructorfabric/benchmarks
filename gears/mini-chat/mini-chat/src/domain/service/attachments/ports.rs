//! Ports of the attachments feature: the attachment store, the provider
//! storage client and directory, the background runtime and the metrics sink.
//! The infrastructure layer implements them.

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::model::{
    AttachmentKind, AttachmentStatus, ChatLimits, CleanupRow, CleanupState, DeleteOutcome,
    FailOutcome, FailWithCleanup, FailureCode, NewAttachment, ReadyOutcome, SecondaryUpdate,
    SoftDelete, StaleUpload, StorageTarget, StoreRow, StoredAttachment, Thumbnail, UploadTargets,
};
use super::payload::CleanupMessage;
use crate::domain::error::DomainError;

// ---- the attachment store -------------------------------------------------------------------------

/// Storage of attachments and chat vector stores. Every method is one statement
/// or one transaction of the feature; every provider call happens outside.
///
/// A CAS method returns `false` (or a `Taken`/`RowTaken` outcome) when its guard
/// matched no row; a database failure is [`DomainError::Database`].
#[async_trait]
pub trait AttachmentRepository: Send + Sync {
    /// The attachment `attachment_id` of `chat_id`, soft-deleted rows included,
    /// through Secure ORM with the tenant part of `scope`. The caller has loaded
    /// the chat already.
    async fn find(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<Option<StoredAttachment>, DomainError>;

    /// The chat's `chat_vector_stores` row, selected by `tenant_id` and `chat_id`.
    async fn vector_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<StoreRow>, DomainError>;

    /// `cpt-cf-mini-chat-algo-attachments-insert-pending`: one write-first
    /// transaction (INSERT, then the limit counts). [`DomainError::DocumentLimit`]
    /// or [`DomainError::StorageLimit`] roll the insert back.
    async fn insert_pending(
        &self,
        new: &NewAttachment,
        limits: &ChatLimits,
    ) -> Result<StoredAttachment, DomainError>;

    /// `pending` to `uploaded` with the provider file id (single-statement CAS on
    /// `pending`, `deleted_at IS NULL`, `cleanup_status IS NULL`).
    async fn set_uploaded(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        provider_file_id: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// `cpt-cf-mini-chat-algo-attachments-mark-ready`: `uploaded` to `ready`
    /// (single-statement CAS on `uploaded`, `provider_file_id IS NOT NULL`,
    /// `deleted_at IS NULL`, `cleanup_status IS NULL`), storing the thumbnail.
    async fn set_ready(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        thumbnail: Option<&Thumbnail>,
        now: OffsetDateTime,
    ) -> Result<ReadyOutcome, DomainError>;

    /// Failure of a row without a provider file: `failed` with `code`
    /// (`pending` or `uploaded`, `deleted_at IS NULL`; `cleanup_status` stays
    /// NULL). Single statement.
    async fn set_failed(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        code: FailureCode,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// `cpt-cf-mini-chat-algo-attachments-fail-upload` step 3: one write-first
    /// transaction (CAS to `failed` with the file id and `cleanup_status`
    /// `pending`, then the enqueue).
    async fn fail_with_cleanup(
        &self,
        request: &FailWithCleanup,
    ) -> Result<FailOutcome, DomainError>;

    /// The `attachment_orphan_file` enqueue, alone in its own transaction.
    async fn enqueue_orphan_file(&self, message: &CleanupMessage) -> Result<(), DomainError>;

    /// The secondary-copy columns (best effort for the caller).
    async fn set_secondary(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        update: &SecondaryUpdate,
        now: OffsetDateTime,
    ) -> Result<(), DomainError>;

    /// Auto-committed INSERT of the vector store placeholder (`vector_store_id`
    /// NULL). A second row for the same `(tenant_id, chat_id)` is
    /// [`DomainError::UniqueViolation`].
    async fn insert_store_placeholder(&self, row: &StoreRow) -> Result<(), DomainError>;

    /// CAS: sets `vector_store_id` where it is still NULL.
    async fn bind_store(
        &self,
        tenant_id: Uuid,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError>;

    /// DELETE of the row where `vector_store_id IS NULL` (failed creation, stale
    /// reclaim); a bound row is never deleted.
    async fn delete_placeholder(&self, tenant_id: Uuid, row_id: Uuid) -> Result<bool, DomainError>;

    /// The background heartbeat: `updated_at = now` where `uploaded`,
    /// `deleted_at IS NULL`, `cleanup_status IS NULL`; `false` when none.
    async fn heartbeat(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// `cpt-cf-mini-chat-algo-attachments-soft-delete`: one write-first
    /// transaction; the cleanup payload is built inside it from the row read
    /// there.
    async fn soft_delete(
        &self,
        scope: &AccessScope,
        request: &SoftDelete,
    ) -> Result<DeleteOutcome, DomainError>;

    /// The reaper's scan: at most `limit` `pending` or `uploaded` rows without
    /// cleanup status, not deleted, `updated_at < cutoff`, oldest first.
    async fn stale_uploads(
        &self,
        cutoff: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<StaleUpload>, DomainError>;

    /// One reaper transaction: CAS to `failed` / `upload_abandoned` (guarded by
    /// the selected status and `updated_at < cutoff`); for a row with a provider
    /// file, `cleanup_status` `pending` and the `attachment_upload_abandoned`
    /// message in the same transaction. `false` when the CAS lost.
    async fn abandon(
        &self,
        stale: &StaleUpload,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// `Some(deleted)` for a chat of the tenant, soft-deleted chats included.
    async fn chat_deleted(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<bool>, DomainError>;

    /// The cleanup columns of a row, soft-deleted rows included.
    async fn cleanup_row(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        id: Uuid,
    ) -> Result<Option<CleanupRow>, DomainError>;

    /// `cleanup_attempts + 1`, `last_cleanup_error`, `cleanup_updated_at` where
    /// the row is still `pending`; the new attempt count, `None` when the row is
    /// no longer pending.
    async fn record_cleanup_attempt(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        error: &str,
        now: OffsetDateTime,
    ) -> Result<Option<i32>, DomainError>;

    /// `cleanup_status = state` where the row is `pending`.
    async fn finish_cleanup(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        state: CleanupState,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;
}

// ---- the provider storage client ----------------------------------------------------------------------

/// A provider call failure, classified for the transient rules of the
/// indexing wait.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageError {
    /// The provider (or the gateway) answered a non-2xx status.
    Status(u16),
    /// The call failed before a response (gateway or transport failure, an OAGW
    /// problem, a timeout of the call itself).
    Transport,
    /// A 2xx answer that cannot be used (not JSON, no non-empty string `id`).
    Malformed,
}

impl StorageError {
    /// Provider 5xx and gateway or transport failures are transient; every
    /// other error (429 included, an unparsable body) is not.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Status(status) => (500..600).contains(status),
            Self::Transport => true,
            Self::Malformed => false,
        }
    }

    /// Short sanitized cause for `last_cleanup_error` and logs: `http <status>`,
    /// `gateway error`, `malformed response`.
    #[must_use]
    pub fn cause(&self) -> String {
        match self {
            Self::Status(status) => format!("http {status}"),
            Self::Transport => "gateway error".to_owned(),
            Self::Malformed => "malformed response".to_owned(),
        }
    }
}

/// Indexing status of a file in a vector store.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    /// `in_progress` or absent.
    InProgress,
    /// `completed`.
    Completed,
    /// `failed`, `cancelled` or any other value.
    Failed,
}

/// The file to store with the provider.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileUpload {
    /// Chat id (first half of the provider filename).
    pub chat_id: Uuid,
    /// Attachment id (second half of the provider filename).
    pub attachment_id: Uuid,
    /// Stored content type (the `Content-Type` of the `file` part).
    pub content_type: String,
    /// Provider extension of the stored type.
    pub extension: String,
    /// The file bytes; shared, so clones are reference-count bumps.
    pub bytes: Bytes,
}

/// The provider Files and Vector Stores APIs behind the OAGW proxy
/// (`cpt-cf-mini-chat-algo-attachments-build-storage-request`). No call is
/// retried and none carries a credential. The caller bounds every call by its
/// own deadline.
#[async_trait]
pub trait StorageClient: Send + Sync {
    /// `POST {prefix}/files`: the new provider file id.
    async fn upload_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        file: &FileUpload,
    ) -> Result<String, StorageError>;

    /// `POST /{alias}/v1/files` of the Anthropic Files API: the secondary file id.
    async fn upload_secondary(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        file: &FileUpload,
    ) -> Result<String, StorageError>;

    /// `POST {prefix}/vector_stores`: the new store id.
    async fn create_vector_store(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        chat_id: Uuid,
    ) -> Result<String, StorageError>;

    /// `POST {prefix}/vector_stores/{id}/files` with the `attachment_id` attribute.
    async fn add_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        vector_store_id: &str,
        provider_file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexStatus, StorageError>;

    /// `GET {prefix}/vector_stores/{id}/files/{file_id}`.
    async fn indexing_status(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        vector_store_id: &str,
        provider_file_id: &str,
    ) -> Result<IndexStatus, StorageError>;

    /// `DELETE {prefix}/files/{file_id}`; 2xx and 404 are success.
    async fn delete_file(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        provider_file_id: &str,
    ) -> Result<(), StorageError>;

    /// `DELETE {prefix}/vector_stores/{id}`; 2xx and 404 are success.
    async fn delete_vector_store(
        &self,
        ctx: &SecurityContext,
        target: &StorageTarget,
        vector_store_id: &str,
    ) -> Result<(), StorageError>;

    /// `DELETE /{alias}/v1/files/{id}` of the Anthropic Files API; 2xx and 404
    /// are success.
    async fn delete_secondary(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        file_id: &str,
    ) -> Result<(), StorageError>;
}

/// The configured provider entries, as the storage targets of a tenant.
pub trait StorageDirectory: Send + Sync {
    /// The storage target (the model provider's entry or its `rag_provider`) and
    /// the Anthropic secondary alias of an upload into a chat whose model is
    /// served by `provider_id`.
    ///
    /// # Errors
    ///
    /// [`DomainError::ProviderResolution`] when the provider or its storage
    /// entry is not configured.
    fn upload_targets(
        &self,
        provider_id: &str,
        tenant_id: Uuid,
    ) -> Result<UploadTargets, DomainError>;

    /// The first provider entry (configuration order) whose storage backend
    /// label is `backend`, with its alias for the tenant.
    fn storage_for_backend(&self, backend: &str, tenant_id: Uuid) -> Option<StorageTarget>;
}

// ---- runtime and metrics -------------------------------------------------------------------------------

/// The gear's late-bound runtime: the background task registry, the stop
/// token and the S2S security context obtained at `start`.
pub trait BackgroundRuntime: Send + Sync {
    /// Spawns `task` on the gear's registry; `false` when the gear has not
    /// started (nothing is spawned).
    fn spawn(&self, name: &'static str, task: BoxFuture<'static, ()>) -> bool;

    /// Cancelled when the gear stops.
    fn stop_token(&self) -> CancellationToken;

    /// The S2S context; `None` before `start`.
    fn s2s_context(&self) -> Option<SecurityContext>;
}

/// `result` label of `mini_chat_attachment_upload`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadResult {
    /// 201.
    Ok,
    /// 4xx or 429 decided by this feature.
    Rejected,
    /// 5xx or a 503 after the row was inserted.
    Failed,
}

impl UploadResult {
    /// Label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
        }
    }
}

/// `result` label of `mini_chat_attachment_background_indexing`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundResult {
    /// The row is `ready`.
    Ready,
    /// Indexing failed.
    Failed,
    /// The 10-minute window ended.
    Timeout,
    /// Every attempt to write `ready` failed.
    SetReadyFailed,
}

impl BackgroundResult {
    /// Label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::SetReadyFailed => "set_ready_failed",
        }
    }
}

/// The attachment instruments (`cpt-cf-mini-chat-algo-attachments-record-metrics`);
/// only bounded labels are ever attached.
pub trait AttachmentMetrics: Send + Sync {
    /// `mini_chat_attachment_upload{kind,result}`.
    fn upload(&self, kind: AttachmentKind, result: UploadResult);
    /// `mini_chat_attachment_upload_bytes{kind}`.
    fn upload_bytes(&self, kind: AttachmentKind, bytes: u64);
    /// `mini_chat_attachments_pending` (up-down).
    fn pending_add(&self, delta: i64);
    /// `mini_chat_attachment_background_indexing{result}`.
    fn background_indexing(&self, result: BackgroundResult);
    /// `mini_chat_attachment_upload_abandoned{from_status}`.
    fn upload_abandoned(&self, from_status: AttachmentStatus);
    /// `mini_chat_upload_reaper_scan_duration_seconds`.
    fn reaper_scan_seconds(&self, seconds: f64);
    /// `mini_chat_cleanup_completed{resource_type="file"}`.
    fn cleanup_completed(&self);
    /// `mini_chat_cleanup_failed{resource_type="file"}`.
    fn cleanup_failed(&self);
    /// `mini_chat_cleanup_retry{resource_type="file",reason="provider_error"}`.
    fn cleanup_retry(&self);
    /// `mini_chat_secondary_cleanup_skipped{provider_kind="anthropic"}`.
    fn secondary_cleanup_skipped(&self);
}
