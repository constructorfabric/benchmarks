//! Attachment model: statuses, rows, the upload value types and the indexing timings.

use std::time::Duration;

use bytes::Bytes;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::message::Thumbnail;

/// `attachments.status`: the row exists, the provider upload is outstanding.
pub const STATUS_PENDING: &str = "pending";
/// `attachments.status`: the provider upload is done, indexing has not finished.
pub const STATUS_UPLOADED: &str = "uploaded";
/// `attachments.status`: usable by messages and tools.
pub const STATUS_READY: &str = "ready";
/// `attachments.status`: the upload or the indexing failed.
pub const STATUS_FAILED: &str = "failed";

/// `attachments.cleanup_status`: provider cleanup is outstanding.
pub const CLEANUP_PENDING: &str = "pending";
/// `attachments.cleanup_status`: provider cleanup finished.
pub const CLEANUP_DONE: &str = "done";
/// `attachments.cleanup_status`: cleanup exhausted its attempts (terminal debt).
pub const CLEANUP_FAILED: &str = "failed";

/// `error_code` of a failed provider file upload.
pub const ERROR_UPLOAD_FAILED: &str = "upload_failed";
/// `error_code` of a failed vector store creation.
pub const ERROR_VECTOR_STORE_FAILED: &str = "vector_store_failed";
/// `error_code` of a failed add, a failed indexing status or an indexing timeout.
pub const ERROR_INDEXING_FAILED: &str = "indexing_failed";
/// `error_code` set by the upload reaper.
pub const ERROR_UPLOAD_ABANDONED: &str = "upload_abandoned";

/// `attachments.secondary_status`: no secondary copy was made.
pub const SECONDARY_NOT_ATTEMPTED: &str = "not_attempted";
/// `attachments.secondary_status`: the secondary copy exists.
pub const SECONDARY_UPLOADED: &str = "uploaded";
/// `attachments.secondary_status`: the secondary upload failed.
pub const SECONDARY_FAILED: &str = "failed";
/// `attachments.secondary_provider_kind` of an Anthropic secondary copy.
pub const PROVIDER_KIND_ANTHROPIC: &str = "anthropic";

/// Whether an attachment is a document or an image.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    /// Indexed or analysed by a tool.
    Document,
    /// Multimodal input only.
    Image,
}

impl AttachmentKind {
    /// The stored value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Image => "image",
        }
    }

    /// Parses a stored value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "document" => Some(Self::Document),
            "image" => Some(Self::Image),
            _ => None,
        }
    }
}

/// The tool paths an attachment takes after the provider upload.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Purposes {
    /// Indexed in the chat vector store.
    pub file_search: bool,
    /// Offered to the code interpreter by its provider file id.
    pub code_interpreter: bool,
}

/// An `attachments` row as the services read it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAttachment {
    /// Attachment id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Parent chat.
    pub chat_id: Uuid,
    /// The uploader.
    pub uploaded_by: Uuid,
    /// The filename.
    pub filename: String,
    /// The MIME type.
    pub content_type: Option<String>,
    /// The size in bytes.
    pub size_bytes: Option<i64>,
    /// The storage backend label of the primary file.
    pub storage_backend: String,
    /// The primary provider file; internal only.
    pub provider_file_id: Option<String>,
    /// `pending`, `uploaded`, `ready` or `failed`.
    pub status: String,
    /// The failure reason, set with the `failed` status.
    pub error_code: Option<String>,
    /// Document or image.
    pub kind: AttachmentKind,
    /// The attachment is indexed for `file_search`.
    pub for_file_search: bool,
    /// The attachment is offered to the code interpreter.
    pub for_code_interpreter: bool,
    /// The preview of an image.
    pub thumbnail: Option<Thumbnail>,
    /// `pending`, `done` or `failed` once provider cleanup is owned.
    pub cleanup_status: Option<String>,
    /// The secondary (Anthropic) copy; internal only.
    pub secondary_file_id: Option<String>,
    /// Upload time.
    pub created_at: OffsetDateTime,
    /// Soft-delete time.
    pub deleted_at: Option<OffsetDateTime>,
}

/// The `AttachmentDetail` of the API.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentDetail {
    /// Attachment id.
    pub id: Uuid,
    /// The filename.
    pub filename: String,
    /// The MIME type.
    pub content_type: Option<String>,
    /// The size in bytes.
    pub size_bytes: Option<i64>,
    /// `pending`, `uploaded`, `ready` or `failed`.
    pub status: String,
    /// Document or image.
    pub kind: AttachmentKind,
    /// The failure reason, present only for a failed attachment.
    pub error_code: Option<String>,
    /// The preview of a ready image.
    pub thumbnail: Option<Thumbnail>,
    /// Upload time.
    pub created_at: OffsetDateTime,
}

impl From<StoredAttachment> for AttachmentDetail {
    fn from(row: StoredAttachment) -> Self {
        let failed = row.status == STATUS_FAILED;
        let ready_image = row.status == STATUS_READY && row.kind == AttachmentKind::Image;
        let error_code = row.error_code.filter(|_| failed);
        let thumbnail = row.thumbnail.filter(|_| ready_image);
        Self {
            id: row.id,
            filename: row.filename,
            content_type: row.content_type,
            size_bytes: row.size_bytes,
            status: row.status,
            kind: row.kind,
            error_code,
            thumbnail,
            created_at: row.created_at,
        }
    }
}

/// An attachment row to insert as `pending`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAttachment {
    /// Attachment id.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Parent chat.
    pub chat_id: Uuid,
    /// The uploader.
    pub uploaded_by: Uuid,
    /// The normalized filename.
    pub filename: String,
    /// The resolved MIME type.
    pub content_type: String,
    /// The size in bytes.
    pub size_bytes: i64,
    /// The storage backend label of the primary file.
    pub storage_backend: String,
    /// Document or image.
    pub kind: AttachmentKind,
    /// The tool paths of the attachment.
    pub purposes: Purposes,
    /// Creation time.
    pub now: OffsetDateTime,
}

/// The per-chat limits checked in the insert transaction.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatLimits {
    /// Documents per chat (`rag.max_documents_per_chat`).
    pub max_documents: u64,
    /// Total bytes per chat (`rag.max_total_upload_mb_per_chat`).
    pub max_total_bytes: u64,
}

/// The secondary copy recorded with the `ready` transition of an image.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecondaryCopy {
    /// No copy was attempted.
    NotAttempted,
    /// The copy exists under `file_id`.
    Uploaded {
        /// The file id in the Anthropic Files API.
        file_id: String,
    },
    /// The copy could not be made.
    Failed,
}

/// What the `ready` transition writes besides the status.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyUpdate {
    /// The preview of an image.
    pub thumbnail: Option<Thumbnail>,
    /// The secondary copy.
    pub secondary: SecondaryCopy,
}

impl ReadyUpdate {
    /// The transition of a document or spreadsheet: nothing besides the status.
    #[must_use]
    pub fn plain() -> Self {
        Self {
            thumbnail: None,
            secondary: SecondaryCopy::NotAttempted,
        }
    }
}

/// A vector store row of a chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorStoreRow {
    /// Row id.
    pub id: Uuid,
    /// The provider vector store; `None` while creation is in progress.
    pub vector_store_id: Option<String>,
    /// The storage backend label of the RAG provider.
    pub provider: String,
    /// When the row was created.
    pub created_at: OffsetDateTime,
}

/// The file of an upload, buffered up to the size limit.
#[domain_model]
#[derive(Debug, Clone)]
pub struct IncomingFile {
    /// The body chunks in order.
    pub chunks: Vec<Bytes>,
    /// The total size in bytes.
    pub size: u64,
}

/// Timings of the document indexing wait of an upload (DESIGN "File Upload").
///
/// The production values are fixed; only the test harness shortens them.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct IndexingTimings {
    /// The upload stops waiting for `in_progress` this long after it started.
    pub request_deadline: Duration,
    /// First wait between status reads; it doubles up to `poll_max` (request) or
    /// `background_poll_max` (background task).
    pub poll_initial: Duration,
    /// Longest wait between status reads inside the request.
    pub poll_max: Duration,
    /// The background task polls in rounds of this length; every round first refreshes
    /// `updated_at`.
    pub background_round: Duration,
    /// Longest wait between status reads in the background task.
    pub background_poll_max: Duration,
    /// The background task gives up after this long.
    pub background_timeout: Duration,
}

impl Default for IndexingTimings {
    fn default() -> Self {
        Self {
            request_deadline: Duration::from_secs(25),
            poll_initial: Duration::from_millis(250),
            poll_max: Duration::from_secs(2),
            background_round: Duration::from_secs(20),
            background_poll_max: Duration::from_secs(5),
            background_timeout: Duration::from_secs(600),
        }
    }
}
