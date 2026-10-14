//! The persistence ports of the upload path: the `attachments` writes and reads, and the
//! attachment cleanup queue.
//!
//! Every method filters on `tenant_id`; every status and cleanup transition is a guarded
//! conditional update. `infra::storage::attachment_repo` implements the ports.

use std::sync::Arc;

use async_trait::async_trait;
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::DbTx;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::attachments::limits::ChatLimits;
use crate::domain::attachments::model::{
    AttachmentKind, AttachmentRecord, AttachmentStatus, Thumbnail,
};
use crate::domain::error::DomainError;

/// The values of a new `pending` row. The owner, the tenant and the chat come from the scoped
/// chat and the security context, never from request input.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAttachment {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub uploaded_by_user_id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub storage_backend: String,
    pub kind: AttachmentKind,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    /// The application clock reading used for `created_at` and `updated_at`.
    pub now: OffsetDateTime,
}

/// A change of the secondary-copy columns.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecondaryUpdate {
    /// `secondary_status` becomes `pending`.
    Pending,
    /// `secondary_file_id`, `secondary_status = uploaded` and `secondary_provider_kind =
    /// anthropic`.
    Uploaded { file_id: String },
    /// `secondary_status` becomes `failed`.
    Failed,
}

/// The cleanup event types of the attachment cleanup queue.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::enum_variant_names,
    reason = "the names are the event types of the payload"
)]
pub enum CleanupEvent {
    AttachmentDeleted,
    AttachmentUploadAbandoned,
    AttachmentIndexingFailed,
}

impl CleanupEvent {
    /// The payload value: `attachment_deleted`, `attachment_upload_abandoned` or
    /// `attachment_indexing_failed`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AttachmentDeleted => "attachment_deleted",
            Self::AttachmentUploadAbandoned => "attachment_upload_abandoned",
            Self::AttachmentIndexingFailed => "attachment_indexing_failed",
        }
    }
}

/// The Anthropic secondary copy of a deleted attachment, as the cleanup payload carries it:
/// `secondary_ref` is `{file_id, provider_kind, upstream_alias}` (DESIGN 4 `Attachment Deletion`,
/// Phase 1 table). Only the `attachment_deleted` event carries it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryRef {
    /// The `secondary_file_id` of the row.
    pub file_id: String,
    /// Always `anthropic`.
    pub provider_kind: String,
    /// The Anthropic `upstream_alias` resolved for the tenant.
    pub upstream_alias: String,
}

/// What a transaction hands to the attachment cleanup queue. The queue serializes `event_type`,
/// `tenant_id`, `chat_id`, `attachment_id`, `provider_file_id`, `vector_store_id` (always null),
/// `storage_backend`, `attachment_kind`, `deleted_at` (the enqueue time) and `secondary_ref`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentCleanupMessage {
    pub event_type: CleanupEvent,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    pub provider_file_id: Option<String>,
    pub storage_backend: String,
    pub attachment_kind: AttachmentKind,
    /// The enqueue time.
    pub enqueued_at: OffsetDateTime,
    /// The secondary copy to delete (`attachment_deleted` with `secondary_status` `uploaded`
    /// only); `None` otherwise.
    pub secondary_ref: Option<SecondaryRef>,
}

/// The input of the delete transaction
/// (`cpt-cf-mini-chat-algo-attachments-and-file-search-delete-transaction`).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteRequest {
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub attachment_id: Uuid,
    /// The caller; a row of another uploader is `NotFound` (checked before the idempotency
    /// check).
    pub uploader_id: Uuid,
    /// The application clock reading: `deleted_at`, `updated_at`, `cleanup_updated_at` and the
    /// enqueue time.
    pub now: OffsetDateTime,
    /// The Anthropic `upstream_alias` resolved for the tenant, for the `secondary_ref` of a row
    /// whose `secondary_status` is `uploaded`; `None` when no Anthropic provider resolves.
    pub secondary_upstream_alias: Option<String>,
}

/// The result of the delete transaction.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// Soft-deleted with `cleanup_status` `pending` and one `attachment_deleted` message
    /// committed.
    Deleted,
    /// Already soft-deleted (also a concurrent delete that committed first): nothing enqueued.
    AlreadyDeleted,
    /// No such row in the chat, or another uploader's row.
    NotFound,
    /// A `message_attachments` row references the attachment; nothing changed.
    Locked,
}

/// Port of the `mini-chat.attachment_cleanup` outbox queue, implemented by the transactional
/// enqueuer of the infrastructure layer.
#[async_trait]
pub trait AttachmentCleanupQueue: Send + Sync {
    /// Enqueue exactly one message inside the caller's transaction `tx`, partitioned by
    /// `tenant_id`; the returned [`Wake`] is fired after the commit.
    ///
    /// # Errors
    ///
    /// `OutboxPayloadTooLarge` for an oversized payload; `Internal` when the outbox is not
    /// running or the database fails.
    async fn enqueue_attachment_cleanup(
        &self,
        tx: &DbTx<'_>,
        message: &AttachmentCleanupMessage,
    ) -> Result<Wake, DomainError>;
}

/// The persistence of the `attachments` rows for the upload and get flows.
#[async_trait]
pub trait AttachmentStore: Send + Sync {
    /// The limits-and-insert transaction: count the chat's live document rows (not deleted,
    /// status other than `failed`) for a document, sum the live `size_bytes` of the chat, reject
    /// at the limits and insert the `pending` row (`size_bytes` 0, `secondary_status`
    /// `not_attempted`, `cleanup_status` NULL).
    ///
    /// # Errors
    ///
    /// `Attachment(DocumentLimit)` or `Attachment(StorageLimit)` (nothing inserted), `Internal`.
    async fn insert_pending(
        &self,
        new: &NewAttachment,
        limits: &ChatLimits,
    ) -> Result<AttachmentRecord, DomainError>;

    /// Store the provider file id and the byte count and move the row `pending` -> `uploaded`
    /// (guarded by `status = pending`).
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure or when the guard matched no row.
    async fn mark_uploaded(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        provider_file_id: &str,
        size_bytes: i64,
        now: OffsetDateTime,
    ) -> Result<(), DomainError>;

    /// The storage re-check: whether the live non-failed rows of the chat other than
    /// `attachment_id` plus `final_bytes` stay within `limits`.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn storage_recheck(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_id: Uuid,
        final_bytes: i64,
        limits: &ChatLimits,
    ) -> Result<bool, DomainError>;

    /// Fail the row (`status = failed`, `error_code`, `updated_at`), guarded by `status` in
    /// (`pending`, `uploaded`); the status the row had, `None` when the guard matched no row.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn mark_failed(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        error_code: &str,
        now: OffsetDateTime,
    ) -> Result<Option<AttachmentStatus>, DomainError>;

    /// Update the secondary-copy columns.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn set_secondary(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        update: &SecondaryUpdate,
        now: OffsetDateTime,
    ) -> Result<(), DomainError>;

    /// Set `status = ready` and `updated_at`, with the thumbnail columns when given (NULL
    /// otherwise), by `id` and `tenant_id`.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn mark_ready(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        thumbnail: Option<&Thumbnail>,
        now: OffsetDateTime,
    ) -> Result<(), DomainError>;

    /// The live attachment of the chat uploaded by `uploader_id`.
    ///
    /// # Errors
    ///
    /// `NotFound { resource: Attachment }` for an unknown, soft-deleted, other-chat or
    /// other-uploader attachment; `Internal`.
    async fn get(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_id: Uuid,
        uploader_id: Uuid,
    ) -> Result<AttachmentRecord, DomainError>;

    /// The background round heartbeat: refresh `updated_at` where `id` and `tenant_id` match,
    /// `status = uploaded`, `deleted_at IS NULL` and `cleanup_status IS NULL`; whether a row was
    /// refreshed.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn heartbeat(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// The guarded `ready` write of the background task (same guard as the heartbeat); whether a
    /// row was updated.
    ///
    /// # Errors
    ///
    /// `Internal` on a database failure.
    async fn set_ready_guarded(
        &self,
        tenant_id: Uuid,
        attachment_id: Uuid,
        now: OffsetDateTime,
    ) -> Result<bool, DomainError>;

    /// The background failure transaction: with the same guard, set `status = failed`,
    /// `error_code = indexing_failed`, `cleanup_status = pending`, `cleanup_updated_at` and
    /// `updated_at`, and enqueue an `attachment_indexing_failed` message through `cleanup`; one
    /// transaction, the wake fires after the commit. Whether the failure was committed (`false`:
    /// the guard matched no row, nothing was changed or enqueued).
    ///
    /// # Errors
    ///
    /// The enqueue error or `Internal`; the transaction is rolled back.
    async fn fail_indexing(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        attachment_id: Uuid,
        now: OffsetDateTime,
        cleanup: Arc<dyn AttachmentCleanupQueue>,
    ) -> Result<bool, DomainError>;

    /// The delete transaction: select the row (a soft-deleted row included) by id, tenant and
    /// chat; `NotFound` for no row or another uploader (before the idempotency check),
    /// `AlreadyDeleted` for a soft-deleted row (nothing enqueued); in one transaction `Locked`
    /// when any `message_attachments` row of the chat references the attachment, else soft-delete
    /// the row (`deleted_at`, `updated_at`, and `cleanup_status` `pending` with
    /// `cleanup_updated_at` when it is NULL, guarded by `deleted_at IS NULL`; a lost guard is
    /// `AlreadyDeleted`) and enqueue one `attachment_deleted` message through `cleanup`; the wake
    /// fires after the commit.
    ///
    /// # Errors
    ///
    /// `OutboxPayloadTooLarge { site: AttachmentDelete }` (rolled back, the attachment stays
    /// active) or `Internal`.
    async fn delete_attachment(
        &self,
        request: &DeleteRequest,
        cleanup: Arc<dyn AttachmentCleanupQueue>,
    ) -> Result<DeleteOutcome, DomainError>;
}
