//! Attachment vocabulary: kind, status, secondary-copy status, thumbnails and the row read
//! model (`AttachmentRecord`).
//!
//! The wire strings are the vocabulary of DESIGN 3.7.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// `attachments.attachment_kind`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Document,
    Image,
}

impl AttachmentKind {
    /// The stored and wire value: `document` or `image`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::Image => "image",
        }
    }
}

/// `attachments.status`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentStatus {
    Pending,
    Uploaded,
    Ready,
    Failed,
}

impl AttachmentStatus {
    /// The stored and wire value: `pending`, `uploaded`, `ready` or `failed`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Uploaded => "uploaded",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

/// `attachments.secondary_status`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecondaryStatus {
    NotAttempted,
    Pending,
    Uploaded,
    Failed,
}

impl SecondaryStatus {
    /// The stored value: `not_attempted`, `pending`, `uploaded` or `failed`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotAttempted => "not_attempted",
            Self::Pending => "pending",
            Self::Uploaded => "uploaded",
            Self::Failed => "failed",
        }
    }
}

/// A generated thumbnail: WebP bytes and the dimensions of the encoded image.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The thumbnail columns of a stored row.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredThumbnail {
    pub bytes: Vec<u8>,
    pub width: i32,
    pub height: i32,
}

/// An `attachments` row as the domain reads it. The projection to the wire object
/// (`AttachmentDetailDto`) drops every internal column.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRecord {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub uploaded_by_user_id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub storage_backend: String,
    pub provider_file_id: Option<String>,
    pub status: AttachmentStatus,
    pub error_code: Option<String>,
    pub kind: AttachmentKind,
    pub for_file_search: bool,
    pub for_code_interpreter: bool,
    pub thumbnail: Option<StoredThumbnail>,
    pub secondary_file_id: Option<String>,
    pub secondary_status: SecondaryStatus,
    pub secondary_provider_kind: Option<String>,
    pub cleanup_status: Option<String>,
    pub cleanup_attempts: i32,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}
