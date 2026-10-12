//! Attachments (DESIGN 3.3 "Upload Attachment" / "Get Attachment" /
//! "Attachment Deletion", 3.6 "File Upload", 3.7 `attachments` and
//! `chat_vector_stores`).
//!
//! [`AttachmentService`] owns the whole lifecycle of an upload:
//!
//! * `upload` (module `upload`): PDP, owner-scoped chat, the chat's model,
//!   multipart parsing, MIME / kind / size rules, the per-chat limits and the
//!   insert in one transaction, then the provider phase (Files API, the lazy
//!   per-chat vector store, indexing);
//! * `index`: the request-phase wait for indexing and the background task that
//!   finishes a document still being indexed at the request deadline;
//! * `store`: the concurrency-safe creation of the chat vector store;
//! * `manage`: `GET` and `DELETE`.
//!
//! Every status change is a conditional update, so the request, the
//! background task, the upload reaper and the cleanup handlers never overwrite
//! each other's decision. The provider file ids and vector store ids stay in
//! this layer: no view type carries them.

mod index;
mod manage;
mod mime;
mod store;
mod thumbnail;
mod upload;

use std::sync::Arc;
use std::time::Duration;

use authz_resolver_sdk::PolicyEnforcer;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use toolkit_db::DBProvider;
use uuid::Uuid;

pub use self::upload::UploadStream;
use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::domain::messages::{AttachmentKind, AttachmentStatus, ThumbnailView};
use crate::domain::ports::{AttachmentCleanupPort, ModelPolicyPort, StoragePort};

/// `attachments.status` values.
pub(crate) mod status {
    /// Upload started.
    pub const PENDING: &str = "pending";
    /// Provider upload done, indexing running.
    pub const UPLOADED: &str = "uploaded";
    /// Usable.
    pub const READY: &str = "ready";
    /// Failed.
    pub const FAILED: &str = "failed";
}

/// `attachments.cleanup_status` values.
pub(crate) mod cleanup_status {
    /// Provider cleanup outstanding.
    pub const PENDING: &str = "pending";
    /// Provider cleanup finished.
    pub const DONE: &str = "done";
    /// Retry budget spent.
    pub const FAILED: &str = "failed";
}

/// `attachments.error_code` values the lifecycle writes.
pub(crate) mod error_code {
    /// The provider upload failed.
    pub const UPLOAD_FAILED: &str = "upload_failed";
    /// The vector store could not be created.
    pub const VECTOR_STORE_FAILED: &str = "vector_store_failed";
    /// Adding to the vector store or the indexing failed.
    pub const INDEXING_FAILED: &str = "indexing_failed";
    /// The upload reaper took an abandoned row.
    pub const UPLOAD_ABANDONED: &str = "upload_abandoned";
}

/// Waits of the indexing lifecycle. The documented values are
/// [`IndexingTimings::default`]; tests shrink them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexingTimings {
    /// The upload stops polling this long after it started.
    pub request_deadline: Duration,
    /// First wait of the request phase.
    pub first_poll_delay: Duration,
    /// The wait doubles up to this.
    pub max_poll_delay: Duration,
    /// Length of one round of the background task.
    pub background_round: Duration,
    /// The background task gives up after this long.
    pub background_max: Duration,
    /// Longest wait between two reads of a background round.
    pub background_max_poll_delay: Duration,
}

impl Default for IndexingTimings {
    fn default() -> Self {
        Self {
            request_deadline: Duration::from_secs(25),
            first_poll_delay: Duration::from_millis(250),
            max_poll_delay: Duration::from_secs(2),
            background_round: Duration::from_secs(20),
            background_max: Duration::from_secs(600),
            background_max_poll_delay: Duration::from_secs(5),
        }
    }
}

/// An attachment as stored (every column the lifecycle reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRow {
    /// Attachment id.
    pub id: Uuid,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The uploader.
    pub uploaded_by_user_id: Uuid,
    /// File name.
    pub filename: String,
    /// Stored content type.
    pub content_type: Option<String>,
    /// Size in bytes.
    pub size_bytes: Option<i64>,
    /// Storage label of the provider backend (internal).
    pub storage_backend: String,
    /// Provider file id (internal).
    pub provider_file_id: Option<String>,
    /// Lifecycle status.
    pub status: String,
    /// Stable internal failure code.
    pub error_code: Option<String>,
    /// `document` or `image`.
    pub kind: String,
    /// Indexed for `file_search`.
    pub for_file_search: bool,
    /// Offered to the code interpreter.
    pub for_code_interpreter: bool,
    /// WebP thumbnail bytes.
    pub thumbnail: Option<Vec<u8>>,
    /// Thumbnail width.
    pub thumbnail_width: Option<i32>,
    /// Thumbnail height.
    pub thumbnail_height: Option<i32>,
    /// Anthropic secondary copy (internal).
    pub secondary_file_id: Option<String>,
    /// `pending`, `done` or `failed`; `None` while no cleanup is owed.
    pub cleanup_status: Option<String>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last change.
    pub updated_at: OffsetDateTime,
    /// Soft-delete time.
    pub deleted_at: Option<OffsetDateTime>,
}

/// An attachment to insert (`pending`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAttachment {
    /// Attachment id.
    pub id: Uuid,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The uploader.
    pub user_id: Uuid,
    /// File name (at most 255 characters).
    pub filename: String,
    /// Stored content type.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Storage label of the provider backend.
    pub storage_backend: String,
    /// `document` or `image`.
    pub kind: AttachmentKind,
    /// Indexed for `file_search`.
    pub for_file_search: bool,
    /// Offered to the code interpreter.
    pub for_code_interpreter: bool,
    /// Creation time.
    pub now: OffsetDateTime,
}

/// A generated thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewThumbnail {
    /// WebP bytes.
    pub bytes: Vec<u8>,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
}

/// What a chat's live attachments already use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AttachmentUsage {
    /// Live (non-failed, non-deleted) documents, spreadsheets included.
    pub documents: u64,
    /// Bytes of every live attachment of every kind.
    pub total_bytes: u64,
}

/// A `chat_vector_stores` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorStoreRow {
    /// Row id.
    pub id: Uuid,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// Provider vector store id; `None` while the creator is still working.
    pub vector_store_id: Option<String>,
    /// Storage label of the provider backend.
    pub provider: String,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// What caused a provider file cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachmentCleanupKind {
    /// The user deleted the attachment.
    #[serde(rename = "attachment_deleted")]
    Deleted,
    /// The upload reaper failed an abandoned upload.
    #[serde(rename = "attachment_upload_abandoned")]
    UploadAbandoned,
    /// The background indexing failed or timed out.
    #[serde(rename = "attachment_indexing_failed")]
    IndexingFailed,
}

/// The Anthropic secondary copy of an image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRef {
    /// Secondary provider file id.
    pub file_id: String,
    /// Always `anthropic` in P1.
    pub provider_kind: String,
    /// Upstream the secondary file lives behind.
    pub upstream_alias: String,
}

/// The attachment-cleanup message (DESIGN "Attachment Deletion", Phase 1);
/// serialized as the outbox payload and deserialized by the handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentCleanupEvent {
    /// Why the cleanup is owed.
    pub event_type: AttachmentCleanupKind,
    /// Tenant of the chat (the partition key).
    pub tenant_id: Uuid,
    /// The chat.
    pub chat_id: Uuid,
    /// The attachment.
    pub attachment_id: Uuid,
    /// Primary provider file; `None` when the upload never reached the provider.
    #[serde(default)]
    pub provider_file_id: Option<String>,
    /// Always `None` in P1; the handler does not read it.
    #[serde(default)]
    pub vector_store_id: Option<String>,
    /// Storage label of the backend the file lives in.
    pub storage_backend: String,
    /// `document` or `image`.
    pub attachment_kind: String,
    /// Enqueue time (the deletion time of a user deletion).
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    /// The secondary copy, when one exists and must be deleted.
    #[serde(default)]
    pub secondary_ref: Option<SecondaryRef>,
}

impl AttachmentCleanupEvent {
    /// The message of `row` (the secondary copy is attached by the caller that
    /// owns it).
    #[must_use]
    pub fn for_row(
        event_type: AttachmentCleanupKind,
        row: &AttachmentRow,
        at: OffsetDateTime,
    ) -> Self {
        Self {
            event_type,
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            attachment_id: row.id,
            provider_file_id: row.provider_file_id.clone(),
            vector_store_id: None,
            storage_backend: row.storage_backend.clone(),
            attachment_kind: row.kind.clone(),
            deleted_at: at,
            secondary_ref: None,
        }
    }
}

/// `AttachmentDetail` of the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentDetail {
    /// Attachment id.
    pub id: Uuid,
    /// File name.
    pub filename: String,
    /// Stored content type.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Lifecycle status.
    pub status: AttachmentStatus,
    /// Kind.
    pub kind: AttachmentKind,
    /// Failure code; only of a failed row.
    pub error_code: Option<String>,
    /// Always `None` (document summaries are not implemented).
    pub doc_summary: Option<String>,
    /// Preview of a ready image.
    pub thumbnail: Option<ThumbnailView>,
    /// Always `None`.
    pub summary_updated_at: Option<OffsetDateTime>,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

impl AttachmentDetail {
    /// The public projection of a stored row.
    ///
    /// # Errors
    /// `Internal` for a status or kind outside the schema contract.
    pub fn from_row(row: &AttachmentRow) -> Result<Self, DomainError> {
        let status = AttachmentStatus::parse(&row.status)?;
        let kind = AttachmentKind::parse(&row.kind)?;
        let thumbnail = match (
            kind,
            status,
            &row.thumbnail,
            row.thumbnail_width,
            row.thumbnail_height,
        ) {
            (
                AttachmentKind::Image,
                AttachmentStatus::Ready,
                Some(bytes),
                Some(width),
                Some(height),
            ) => Some(ThumbnailView {
                bytes: bytes.clone(),
                width,
                height,
            }),
            _ => None,
        };
        Ok(Self {
            id: row.id,
            filename: row.filename.clone(),
            content_type: row.content_type.clone().unwrap_or_default(),
            size_bytes: row.size_bytes.unwrap_or_default(),
            status,
            kind,
            error_code: (status == AttachmentStatus::Failed)
                .then(|| row.error_code.clone())
                .flatten(),
            doc_summary: None,
            thumbnail,
            summary_updated_at: None,
            created_at: row.created_at,
        })
    }
}

/// The handles the background tasks need; cheap to clone.
#[derive(Clone)]
pub(crate) struct Shared {
    pub(crate) db: Arc<DBProvider<DomainError>>,
    pub(crate) storage: Arc<dyn StoragePort>,
    pub(crate) cleanup: Arc<dyn AttachmentCleanupPort>,
    pub(crate) timings: IndexingTimings,
}

/// Uploads, reads and deletes attachments and finishes document indexing in
/// the background.
pub struct AttachmentService {
    shared: Shared,
    policy: Arc<dyn ModelPolicyPort>,
    config: Arc<MiniChatConfig>,
    enforcer: PolicyEnforcer,
    /// `rag.max_concurrent_uploads` slots, per process.
    uploads: Arc<Semaphore>,
    /// The background indexing tasks.
    tasks: TaskTracker,
    /// Cancelled by [`Self::stop`].
    stop: CancellationToken,
}

impl AttachmentService {
    /// Creates the service with the documented indexing timings.
    #[must_use]
    pub fn new<
        P: ModelPolicyPort + 'static,
        S: StoragePort + 'static,
        C: AttachmentCleanupPort + 'static,
    >(
        db: Arc<DBProvider<DomainError>>,
        policy: Arc<P>,
        storage: Arc<S>,
        cleanup: Arc<C>,
        config: Arc<MiniChatConfig>,
        enforcer: PolicyEnforcer,
    ) -> Self {
        let slots = usize::from(config.rag.max_concurrent_uploads);
        Self {
            shared: Shared {
                db,
                storage,
                cleanup,
                timings: IndexingTimings::default(),
            },
            policy,
            config,
            enforcer,
            uploads: Arc::new(Semaphore::new(slots)),
            tasks: TaskTracker::new(),
            stop: CancellationToken::new(),
        }
    }

    /// Replaces the indexing timings (tests shrink them).
    #[must_use]
    pub fn with_timings(mut self, timings: IndexingTimings) -> Self {
        self.shared.timings = timings;
        self
    }

    /// Gear stop: cancels every background indexing task and waits for it. A
    /// row still `uploaded` stays `uploaded` (no failure is recorded, nothing is
    /// enqueued); the upload reaper finishes it.
    pub async fn stop(&self) {
        self.stop.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }
}
