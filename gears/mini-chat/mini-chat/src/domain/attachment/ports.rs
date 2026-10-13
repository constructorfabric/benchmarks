//! Ports of the attachment slice: persistence and the provider storage gateway.

use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::model::{ChatLimits, NewAttachment, ReadyUpdate, StoredAttachment, VectorStoreRow};
use crate::domain::cleanup::AttachmentCleanupPayload;
use crate::domain::error::DomainError;

/// The result of deleting an attachment.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// The row was soft-deleted by this call.
    Deleted,
    /// Another call (or an earlier one) already deleted it.
    AlreadyDeleted,
    /// A submitted message references the attachment.
    Locked,
}

/// Persistence of attachments and chat vector store rows. Every method applies the tenant of the
/// scope; the callers load the chat through its owner scope first.
#[async_trait]
pub trait AttachmentRepository: Send + Sync {
    /// Inserts a `pending` row after checking the per-chat limits (non-deleted, non-failed rows)
    /// in the same transaction.
    ///
    /// # Errors
    /// `DocumentLimit` or `StorageLimit` when a limit would be exceeded; otherwise an internal
    /// error.
    async fn insert_pending(
        &self,
        scope: &AccessScope,
        new: NewAttachment,
        limits: ChatLimits,
    ) -> Result<(), DomainError>;

    /// The row of the chat, soft-deleted ones included.
    async fn find(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<Option<StoredAttachment>, DomainError>;

    /// `pending` to `uploaded` with the provider file; `false` when the row is no longer a live
    /// `pending` row.
    async fn mark_uploaded(
        &self,
        scope: &AccessScope,
        attachment_id: Uuid,
        provider_file_id: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// `uploaded` to `ready`; `false` when the row is no longer a live `uploaded` row without a
    /// cleanup owner.
    async fn mark_ready(
        &self,
        scope: &AccessScope,
        attachment_id: Uuid,
        update: &ReadyUpdate,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// `pending` or `uploaded` to `failed` with `error_code`; `false` when the row moved on or
    /// has a cleanup owner.
    async fn mark_failed(
        &self,
        scope: &AccessScope,
        attachment_id: Uuid,
        error_code: &str,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// Refreshes `updated_at` of a live `uploaded` row without a cleanup owner; `false` when the
    /// row no longer qualifies (the background task stops).
    async fn touch_uploaded(
        &self,
        scope: &AccessScope,
        attachment_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// The failure of a background indexing task, one transaction: `uploaded` to `failed` with
    /// `indexing_failed`, `cleanup_status = pending` and the cleanup event. `false` when the row no
    /// longer qualifies (nothing was written).
    async fn fail_indexing(
        &self,
        scope: &AccessScope,
        payload: &AttachmentCleanupPayload,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// The deletion of an attachment, one transaction: the reference check, the soft delete and,
    /// when no cleanup owner exists yet, `cleanup_status = pending` with the cleanup event.
    async fn delete(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        payload: &AttachmentCleanupPayload,
        now: OffsetDateTime,
    ) -> Result<DeleteOutcome, DomainError>;

    /// The vector store row of the chat.
    async fn find_store(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRow>, DomainError>;

    /// Inserts the placeholder row (`vector_store_id = NULL`, auto-committed); `None` when the
    /// unique constraint says another request owns the chat's row.
    async fn insert_store_placeholder(
        &self,
        scope: &AccessScope,
        tenant_id: Uuid,
        chat_id: Uuid,
        provider: &str,
        now: OffsetDateTime,
    ) -> Result<Option<Uuid>, DomainError>;

    /// Compare-and-set of `vector_store_id` on a placeholder; `false` when the row was reclaimed.
    async fn set_store_id(
        &self,
        scope: &AccessScope,
        row_id: Uuid,
        vector_store_id: &str,
    ) -> Result<bool, DomainError>;

    /// Deletes a placeholder row that still has no `vector_store_id`.
    async fn delete_placeholder(
        &self,
        scope: &AccessScope,
        row_id: Uuid,
    ) -> Result<bool, DomainError>;
}

/// An attachment the upload reaper found stale.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleUpload {
    /// Attachment id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Parent chat.
    pub chat_id: Uuid,
    /// `pending` or `uploaded`.
    pub status: String,
    /// The primary provider file, when the upload got that far.
    pub provider_file_id: Option<String>,
    /// The storage backend label of the primary file.
    pub storage_backend: String,
    /// `document` or `image`.
    pub attachment_kind: String,
    /// The Anthropic secondary copy (not deleted by the reaper).
    pub secondary_file_id: Option<String>,
}

/// Persistence of the upload reaper.
#[async_trait]
pub trait UploadReaperRepository: Send + Sync {
    /// At most `limit` rows of any tenant with `status IN (pending, uploaded)`, not soft-deleted,
    /// without `cleanup_status` and with `updated_at < cutoff`, oldest first.
    async fn stale_uploads(
        &self,
        cutoff: OffsetDateTime,
        limit: u64,
    ) -> Result<Vec<StaleUpload>, DomainError>;

    /// One transaction: the guarded `failed` / `upload_abandoned` update and, when `cleanup` is
    /// given, `cleanup_status = pending` with the cleanup event. `false` when the row no longer
    /// qualifies.
    async fn abandon(
        &self,
        upload: &StaleUpload,
        cutoff: OffsetDateTime,
        now: OffsetDateTime,
        cleanup: Option<&AttachmentCleanupPayload>,
    ) -> Result<bool, DomainError>;
}

/// An attachment whose provider file a chat cleanup still has to delete.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCleanup {
    /// Attachment id.
    pub id: Uuid,
    /// The primary provider file; `None` when the upload never reached the provider.
    pub provider_file_id: Option<String>,
    /// The storage backend label of the primary file.
    pub storage_backend: String,
    /// The secondary (Anthropic) copy.
    pub secondary_file_id: Option<String>,
}

/// The outcome of recording a failed provider delete.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupFailure {
    /// `cleanup_attempts` after the increment.
    pub attempts: u32,
    /// The attempt limit was reached: `cleanup_status` is now `failed`.
    pub terminal: bool,
}

/// Persistence of the cleanup handlers. Background work carries the tenant of the message in
/// every query.
#[async_trait]
pub trait CleanupRepository: Send + Sync {
    /// Whether the chat exists in the tenant and is soft-deleted.
    async fn chat_is_deleted(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<bool, DomainError>;

    /// The `cleanup_status` of the attachment; `None` when the row does not exist or has no
    /// cleanup owner (`cleanup_status IS NULL`).
    async fn cleanup_status(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
    ) -> Result<Option<String>, DomainError>;

    /// Gives every attachment of the (soft-deleted) chat that has no cleanup owner
    /// (`cleanup_status IS NULL`) to the chat cleanup: sets `cleanup_status = pending`. Returns
    /// the number of rows taken.
    async fn claim_unowned_cleanups(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<u64, DomainError>;

    /// The model of the chat (soft-deleted chats included), for locating its secondary copies.
    async fn chat_model(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<String>, DomainError>;

    /// Sets `cleanup_status = done`.
    async fn mark_cleanup_done(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<(), DomainError>;

    /// Records a failed provider delete: `cleanup_attempts += 1`, `last_cleanup_error`,
    /// `cleanup_updated_at`; at `max_attempts` the row becomes `failed`.
    async fn record_cleanup_failure(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        error: &str,
        max_attempts: u32,
        now: OffsetDateTime,
    ) -> Result<CleanupFailure, DomainError>;

    /// The attachments of the chat with `cleanup_status = pending`.
    async fn pending_cleanups(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Vec<PendingCleanup>, DomainError>;

    /// How many attachments of the chat are terminally `failed`.
    async fn failed_cleanups(&self, tenant_id: Uuid, chat_id: Uuid) -> Result<u64, DomainError>;

    /// The vector store row of the chat.
    async fn chat_store(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
    ) -> Result<Option<VectorStoreRow>, DomainError>;

    /// Deletes the vector store row (the completion marker of the store cleanup).
    async fn delete_store_row(&self, tenant_id: Uuid, row_id: Uuid) -> Result<(), DomainError>;
}

/// How a provider operation failed.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct StorageError {
    /// A retry may succeed: gateway failure, provider 5xx, 408 or 429.
    pub transient: bool,
    /// The provider's HTTP status, when it answered.
    pub status: Option<u16>,
    /// Operator-facing cause without provider identifiers.
    pub message: String,
}

impl StorageError {
    /// The provider answered with a non-success status.
    #[must_use]
    pub fn status(status: u16) -> Self {
        Self {
            transient: status >= 500 || status == 408 || status == 429,
            status: Some(status),
            message: format!("the provider answered with HTTP {status}"),
        }
    }

    /// The gateway or the transport failed.
    #[must_use]
    pub fn gateway(message: impl Into<String>) -> Self {
        Self {
            transient: true,
            status: None,
            message: message.into(),
        }
    }

    /// The provider answered with a body that cannot be used.
    #[must_use]
    pub fn malformed(message: impl Into<String>) -> Self {
        Self {
            transient: false,
            status: None,
            message: message.into(),
        }
    }
}

/// The wire flavor of a storage backend.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageFlavor {
    /// `OpenAI`: `/v1/...`.
    Openai,
    /// Azure `OpenAI`: `/openai/...?api-version=`.
    Azure {
        /// The `api-version` query value.
        api_version: String,
    },
}

/// Where a chat's files and vector store live.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageRoute {
    /// The provider entry serving storage (`rag_provider` or the model's own provider).
    pub provider_id: String,
    /// The storage backend label stored on rows (`attachments.storage_backend`).
    pub backend: String,
    /// The OAGW upstream alias of the provider for the tenant.
    pub alias: String,
    /// The API flavor.
    pub flavor: StorageFlavor,
}

/// The secondary (Anthropic) Files API of a chat served by an `anthropic_messages` provider.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryRoute {
    /// The OAGW upstream alias of the Anthropic provider for the tenant.
    pub alias: String,
    /// `anthropic`.
    pub provider_kind: &'static str,
}

/// The storage of one chat: the primary route and the optional secondary copy route.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatStorage {
    /// Files and vector store.
    pub primary: StorageRoute,
    /// Secondary image copy; only for chats served by an `anthropic_messages` provider.
    pub secondary: Option<SecondaryRoute>,
}

/// A file to upload: name, type and the body chunks.
#[domain_model]
#[derive(Debug, Clone)]
pub struct FileUpload {
    /// The provider-side filename `{chat_id}_{attachment_id}.{ext}`.
    pub filename: String,
    /// The MIME type of the file part.
    pub content_type: String,
    /// The body chunks in order.
    pub chunks: Vec<Bytes>,
}

/// The indexing state of a vector store file.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexingStatus {
    /// Still indexing (also a response without a status).
    InProgress,
    /// Searchable.
    Completed,
    /// `failed`, `cancelled` or any unknown status.
    Failed(String),
}

/// The provider storage gateway: Files and Vector Stores operations through OAGW, dispatched by
/// the storage kind of the provider entry (ADR-0005).
#[async_trait]
pub trait StorageGateway: Send + Sync {
    /// The storage of a chat whose model is served by `provider_id`.
    ///
    /// # Errors
    /// `PolicyUnavailable` when the provider entry (or its `rag_provider`) is not configured.
    fn chat_storage(&self, tenant_id: Uuid, provider_id: &str) -> Result<ChatStorage, DomainError>;

    /// The route of a stored backend label.
    ///
    /// # Errors
    /// `PolicyUnavailable` when no provider entry has the label.
    fn route_for_backend(
        &self,
        tenant_id: Uuid,
        backend: &str,
    ) -> Result<StorageRoute, DomainError>;

    /// Uploads a file (`purpose = assistants`) and returns the provider file id.
    ///
    /// # Errors
    /// A [`StorageError`] when the provider refuses or the call fails.
    async fn upload_file(
        &self,
        ctx: &SecurityContext,
        route: &StorageRoute,
        file: FileUpload,
    ) -> Result<String, StorageError>;

    /// Deletes a file; a 2xx or 404 answer is success.
    ///
    /// # Errors
    /// A [`StorageError`] for any other status or a failed call.
    async fn delete_file(
        &self,
        ctx: &SecurityContext,
        route: &StorageRoute,
        file_id: &str,
    ) -> Result<(), StorageError>;

    /// Creates a vector store and returns its provider id.
    ///
    /// # Errors
    /// A [`StorageError`] when the provider refuses or the call fails.
    async fn create_vector_store(
        &self,
        ctx: &SecurityContext,
        route: &StorageRoute,
    ) -> Result<String, StorageError>;

    /// Adds a file to a vector store with the attribute `attachment_id` and returns its status.
    ///
    /// # Errors
    /// A [`StorageError`] when the provider refuses or the call fails.
    async fn add_file_to_store(
        &self,
        ctx: &SecurityContext,
        route: &StorageRoute,
        vector_store_id: &str,
        file_id: &str,
        attachment_id: Uuid,
    ) -> Result<IndexingStatus, StorageError>;

    /// Reads the indexing status of a vector store file.
    ///
    /// # Errors
    /// A [`StorageError`] when the provider refuses or the call fails.
    async fn file_status(
        &self,
        ctx: &SecurityContext,
        route: &StorageRoute,
        vector_store_id: &str,
        file_id: &str,
    ) -> Result<IndexingStatus, StorageError>;

    /// Deletes a vector store; a 2xx or 404 answer is success.
    ///
    /// # Errors
    /// A [`StorageError`] for any other status or a failed call.
    async fn delete_vector_store(
        &self,
        ctx: &SecurityContext,
        route: &StorageRoute,
        vector_store_id: &str,
    ) -> Result<(), StorageError>;

    /// Uploads the secondary copy of an image to the Anthropic Files API.
    ///
    /// # Errors
    /// A [`StorageError`] when the provider refuses or the call fails.
    async fn upload_secondary(
        &self,
        ctx: &SecurityContext,
        route: &SecondaryRoute,
        file: FileUpload,
    ) -> Result<String, StorageError>;

    /// Deletes a secondary copy through `alias`; a 2xx or 404 answer is success.
    ///
    /// # Errors
    /// A [`StorageError`] for any other status or a failed call.
    async fn delete_secondary(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        file_id: &str,
    ) -> Result<(), StorageError>;
}
