//! The attachment cleanup message as the domain sees it
//! (`cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload`). The JSON
//! encoding and decoding live in `infra::outbox::attachment_cleanup`.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use super::model::{AttachmentKind, StoredAttachment};

/// `event_type` of a cleanup message.
#[allow(clippy::enum_variant_names)] // the variants are the wire event types
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupEvent {
    /// `attachment_deleted`: user delete.
    AttachmentDeleted,
    /// `attachment_upload_abandoned`: the reaper.
    AttachmentUploadAbandoned,
    /// `attachment_indexing_failed`: indexing failure in the request or the
    /// background task.
    AttachmentIndexingFailed,
    /// `attachment_upload_failed`: an upload request failed a row that has a
    /// provider file with `upload_failed` or `vector_store_failed`.
    AttachmentUploadFailed,
    /// `attachment_orphan_file`: row-less variant, a provider file id no row
    /// can carry.
    AttachmentOrphanFile,
}

impl CleanupEvent {
    /// The `event_type` wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AttachmentDeleted => "attachment_deleted",
            Self::AttachmentUploadAbandoned => "attachment_upload_abandoned",
            Self::AttachmentIndexingFailed => "attachment_indexing_failed",
            Self::AttachmentUploadFailed => "attachment_upload_failed",
            Self::AttachmentOrphanFile => "attachment_orphan_file",
        }
    }

    /// Parses an `event_type`.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "attachment_deleted" => Some(Self::AttachmentDeleted),
            "attachment_upload_abandoned" => Some(Self::AttachmentUploadAbandoned),
            "attachment_indexing_failed" => Some(Self::AttachmentIndexingFailed),
            "attachment_upload_failed" => Some(Self::AttachmentUploadFailed),
            "attachment_orphan_file" => Some(Self::AttachmentOrphanFile),
            _ => None,
        }
    }
}

/// The secondary (Anthropic) copy to delete with the primary file.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecondaryRef {
    /// Anthropic file id.
    pub file_id: String,
    /// OAGW alias of the Anthropic upstream.
    pub upstream_alias: String,
}

/// One message of the attachment cleanup queue.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupMessage {
    /// Event type.
    pub event: CleanupEvent,
    /// Tenant of the row (of the upload, for an orphan file).
    pub tenant_id: Uuid,
    /// Chat of the row.
    pub chat_id: Uuid,
    /// Attachment id.
    pub attachment_id: Uuid,
    /// Provider file id; `None` when the upload never reached the provider;
    /// never `None` for `AttachmentUploadFailed` and `AttachmentOrphanFile`.
    pub provider_file_id: Option<String>,
    /// Storage backend label of the row.
    pub storage_backend: String,
    /// Attachment kind.
    pub kind: AttachmentKind,
    /// Enqueue time.
    pub deleted_at: OffsetDateTime,
    /// Secondary copy, only for `AttachmentDeleted` with a resolved reference
    /// and `AttachmentUploadFailed` of a row whose secondary copy is uploaded.
    pub secondary_ref: Option<SecondaryRef>,
}

impl CleanupMessage {
    /// The message of `event` for `row` at `now`. `provider_file_id` is the
    /// file to delete (it can differ from the row's: the one held in memory
    /// after a failed `uploaded` write). `secondary_ref` is applied only for
    /// the events that may carry it and dropped for every other event.
    #[must_use]
    pub fn for_row(
        event: CleanupEvent,
        row: &StoredAttachment,
        provider_file_id: Option<String>,
        secondary_ref: Option<SecondaryRef>,
        now: OffsetDateTime,
    ) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload:p1:inst-secondary-ref-rule
        let secondary_ref = secondary_ref.filter(|_| {
            matches!(
                event,
                CleanupEvent::AttachmentDeleted | CleanupEvent::AttachmentUploadFailed
            )
        });
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload:p1:inst-secondary-ref-rule
        Self {
            event,
            tenant_id: row.tenant_id,
            chat_id: row.chat_id,
            attachment_id: row.id,
            provider_file_id,
            storage_backend: row.storage_backend.clone(),
            kind: row.kind,
            deleted_at: now,
            secondary_ref,
        }
    }

    /// The row-less `AttachmentOrphanFile` message of `provider_file_id`
    /// (`secondary_ref` is always `None`).
    #[must_use]
    pub fn orphan(row: &StoredAttachment, provider_file_id: &str, now: OffsetDateTime) -> Self {
        Self::for_row(
            CleanupEvent::AttachmentOrphanFile,
            row,
            Some(provider_file_id.to_owned()),
            None,
            now,
        )
    }
}
