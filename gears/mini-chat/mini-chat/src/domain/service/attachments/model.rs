//! Vocabulary of the attachments feature: rows, classification, the storage
//! targets and the requests the attachment store port takes.
//!
//! Nothing here is serialized: the wire shapes are the DTOs of `api::rest::dto`
//! and the cleanup payload of `infra::outbox::attachment_cleanup`.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use super::payload::CleanupMessage;
use crate::config::StorageKind;
pub use crate::domain::service::stream::ports::AttachmentKind;

impl AttachmentKind {
    /// `attachments.attachment_kind` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Image => "image",
        }
    }

    /// Parses a stored kind.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "document" => Some(Self::Document),
            "image" => Some(Self::Image),
            _ => None,
        }
    }
}

/// `attachments.status`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentStatus {
    /// Row inserted, provider upload not stored yet.
    Pending,
    /// The provider file id is stored, processing continues.
    Uploaded,
    /// Usable by the send pipeline.
    Ready,
    /// Terminal failure (`error_code` says why).
    Failed,
}

impl AttachmentStatus {
    /// `attachments.status` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Uploaded => "uploaded",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    /// Parses a stored status.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(Self::Pending),
            "uploaded" => Some(Self::Uploaded),
            "ready" => Some(Self::Ready),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// `attachments.error_code` values written by this feature.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCode {
    /// The provider upload (or a database write around it) failed.
    UploadFailed,
    /// The chat vector store could not be resolved.
    VectorStoreFailed,
    /// Indexing failed or timed out.
    IndexingFailed,
    /// The reaper found the row stale.
    UploadAbandoned,
}

impl FailureCode {
    /// `attachments.error_code` value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UploadFailed => "upload_failed",
            Self::VectorStoreFailed => "vector_store_failed",
            Self::IndexingFailed => "indexing_failed",
            Self::UploadAbandoned => "upload_abandoned",
        }
    }
}

/// A WebP thumbnail, as generated and as stored.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    /// Encoded WebP bytes.
    pub bytes: Vec<u8>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// An `attachments` row with every column the feature reads.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAttachment {
    /// Attachment id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Parent chat.
    pub chat_id: Uuid,
    /// Uploading user.
    pub uploaded_by_user_id: Uuid,
    /// File name.
    pub filename: String,
    /// Stored (normalized) content type.
    pub content_type: String,
    /// Size of the content.
    pub size_bytes: i64,
    /// Storage backend label of the provider entry.
    pub storage_backend: String,
    /// Provider file id.
    pub provider_file_id: Option<String>,
    /// Lifecycle status.
    pub status: AttachmentStatus,
    /// Failure reason, when failed.
    pub error_code: Option<String>,
    /// Kind.
    pub kind: AttachmentKind,
    /// Document searched through the chat vector store.
    pub for_file_search: bool,
    /// Document used by the code interpreter.
    pub for_code_interpreter: bool,
    /// Stored thumbnail.
    pub thumbnail: Option<Thumbnail>,
    /// Cleanup state (`pending`, `done`, `failed`), `None` when none.
    pub cleanup_status: Option<String>,
    /// Anthropic secondary copy id.
    pub secondary_file_id: Option<String>,
    /// `not_attempted`, `pending`, `uploaded` or `failed`.
    pub secondary_status: String,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last write (heartbeat of the background indexing).
    pub updated_at: OffsetDateTime,
    /// Soft-delete time.
    pub deleted_at: Option<OffsetDateTime>,
}

/// The `AttachmentDetail` projection (`cpt-cf-mini-chat-algo-attachments-project-detail`).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentDetail {
    /// Attachment id.
    pub id: Uuid,
    /// File name.
    pub filename: String,
    /// Stored content type.
    pub content_type: String,
    /// Size of the content.
    pub size_bytes: i64,
    /// Lifecycle status.
    pub status: AttachmentStatus,
    /// Kind.
    pub kind: AttachmentKind,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Only for a failed row.
    pub error_code: Option<String>,
    /// Only for a ready image with a stored thumbnail.
    pub thumbnail: Option<Thumbnail>,
}

// ---- classification ----------------------------------------------------------------------------

/// The switches classification depends on.
#[allow(clippy::struct_excessive_bools)] // independent switches, not a state
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassifyRules {
    /// `rag.allow_csv_upload`.
    pub allow_csv_upload: bool,
    /// Kill switch `disable_images`.
    pub disable_images: bool,
    /// The chat model supports the code interpreter and its kill switch is off.
    pub code_interpreter_available: bool,
}

/// The outcome of `classify_file`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    /// Stored (normalized, alias-resolved) content type.
    pub content_type: String,
    /// Kind.
    pub kind: AttachmentKind,
    /// Document searched through the chat vector store.
    pub for_file_search: bool,
    /// Document used by the code interpreter.
    pub for_code_interpreter: bool,
    /// Provider file extension of the stored type (`pdf`, `txt`, `jpg`, ...).
    pub extension: &'static str,
}

// ---- storage targets ---------------------------------------------------------------------------

/// Where files and vector stores of a chat live.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageTarget {
    /// Provider id of the storage entry (the chat provider or its `rag_provider`).
    pub provider_id: String,
    /// `openai` (`/v1`) or `azure` (`/openai` with `api-version`).
    pub kind: StorageKind,
    /// `attachments.storage_backend` / `chat_vector_stores.provider` label.
    pub backend: String,
    /// OAGW alias of the entry for the request tenant.
    pub alias: String,
    /// `api-version` of an Azure entry.
    pub api_version: Option<String>,
}

/// Targets of an upload into a chat of one model.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadTargets {
    /// Primary storage.
    pub storage: StorageTarget,
    /// Alias of the Anthropic entry when the chat provider kind is
    /// `anthropic_messages` (the secondary copy of images), else `None`.
    pub secondary_alias: Option<String>,
}

// ---- store requests ----------------------------------------------------------------------------

/// The pending row to insert.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAttachment {
    /// Pre-generated attachment id.
    pub id: Uuid,
    /// Chat tenant.
    pub tenant_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
    /// Uploading user.
    pub user_id: Uuid,
    /// File name (already normalized).
    pub filename: String,
    /// Stored content type.
    pub content_type: String,
    /// Size of the content.
    pub size_bytes: i64,
    /// Storage backend label.
    pub storage_backend: String,
    /// Kind.
    pub kind: AttachmentKind,
    /// `for_file_search`.
    pub for_file_search: bool,
    /// `for_code_interpreter`.
    pub for_code_interpreter: bool,
    /// `created_at` and `updated_at`.
    pub now: OffsetDateTime,
}

/// The per-chat limits of the pending insert.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatLimits {
    /// `rag.max_documents_per_chat`.
    pub max_documents: u32,
    /// `rag.max_total_upload_mb_per_chat` in bytes.
    pub max_total_bytes: u64,
}

/// A `chat_vector_stores` row.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreRow {
    /// Row id.
    pub id: Uuid,
    /// Tenant.
    pub tenant_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
    /// Provider store id, `None` while the creation is in progress.
    pub vector_store_id: Option<String>,
    /// Storage backend label.
    pub provider: String,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// The outcome of the `uploaded` to `ready` CAS.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyOutcome {
    /// The row is `ready`.
    Ready,
    /// The row could not move (deleted, reaped, claimed by chat cleanup).
    Taken {
        /// The chat is soft-deleted.
        chat_deleted: bool,
    },
}

/// The secondary-copy column writes.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecondaryUpdate {
    /// `secondary_status = pending`, `secondary_provider_kind = anthropic`.
    Pending,
    /// `secondary_status = uploaded` with the Anthropic file id.
    Uploaded(String),
    /// `secondary_status = failed`.
    Failed,
}

/// One write-first failure transaction of a row that has a provider file.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailWithCleanup {
    /// Row tenant.
    pub tenant_id: Uuid,
    /// Row chat.
    pub chat_id: Uuid,
    /// Row id.
    pub attachment_id: Uuid,
    /// `error_code` to write.
    pub code: FailureCode,
    /// The provider file id to record on the row.
    pub provider_file_id: String,
    /// `updated_at` and `cleanup_updated_at`.
    pub now: OffsetDateTime,
    /// Enqueued when the CAS wins.
    pub message: CleanupMessage,
    /// Enqueued instead (row-less) when the CAS loses and the file id was held
    /// only in memory.
    pub orphan: Option<CleanupMessage>,
}

/// The outcome of `fail_with_cleanup`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    /// The row is `failed` with `cleanup_status` `pending`, the message is enqueued.
    Failed,
    /// The CAS lost; the row-less orphan message is enqueued.
    OrphanEnqueued,
    /// The CAS lost and the id is on the row: rolled back, nothing enqueued.
    RowTaken,
}

/// The soft-delete request.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoftDelete {
    /// Row chat.
    pub chat_id: Uuid,
    /// Row id.
    pub attachment_id: Uuid,
    /// Anthropic alias resolved before the transaction; the `secondary_ref`
    /// is built inside the transaction from the row read there, only when its
    /// secondary copy is `uploaded`.
    pub secondary_alias: Option<String>,
    /// `deleted_at`, `updated_at`, `cleanup_updated_at`.
    pub now: OffsetDateTime,
}

/// The outcome of the soft delete.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// Soft-deleted by this call (with a cleanup message when the row had no
    /// cleanup status).
    Deleted,
    /// A concurrent delete won; nothing written.
    AlreadyDeleted,
    /// A `message_attachments` row references the attachment; rolled back.
    Locked,
}

/// A stale `pending` or `uploaded` row selected by the reaper.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleUpload {
    /// Row id.
    pub id: Uuid,
    /// Row tenant.
    pub tenant_id: Uuid,
    /// Row chat.
    pub chat_id: Uuid,
    /// `pending` or `uploaded`.
    pub status: AttachmentStatus,
    /// Provider file id.
    pub provider_file_id: Option<String>,
    /// Anthropic secondary copy id.
    pub secondary_file_id: Option<String>,
    /// Storage backend label.
    pub storage_backend: String,
    /// Kind.
    pub kind: AttachmentKind,
    /// The `updated_at` the row was selected with.
    pub updated_at: OffsetDateTime,
}

/// `attachments.cleanup_status`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupState {
    /// Provider cleanup is owed.
    Pending,
    /// The provider file is deleted.
    Done,
    /// The attempts are exhausted.
    Failed,
}

impl CleanupState {
    /// Stored value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// What the cleanup handler reads of a row.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupRow {
    /// `cleanup_status`, `None` when none.
    pub state: Option<CleanupState>,
    /// `cleanup_attempts`.
    pub attempts: i32,
}
