//! REST DTO of the attachment endpoints (`OpenAPI` name `AttachmentDetailDto`).
//!
//! The wire form is the public projection only: the provider file id, the
//! vector store id and the storage backend never appear (DESIGN 3.3 "Provider
//! identifier non-exposure").

use time::OffsetDateTime;
use uuid::Uuid;

use super::dto::{AttachmentKindDto, AttachmentStatusDto, ImgThumbnailDto};
use crate::domain::attachments::AttachmentDetail;

/// Full attachment details returned by the upload and the GET attachment
/// endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentDetailDto {
    /// Attachment id.
    pub id: Uuid,
    /// File name.
    pub filename: String,
    /// Stored content type.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Lifecycle status.
    pub status: AttachmentStatusDto,
    /// Kind.
    pub kind: AttachmentKindDto,
    /// Stable internal failure code; present only when the status is `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Always absent (document summaries are not implemented).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_summary: Option<String>,
    /// Preview of a ready image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
    /// Always absent.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub summary_updated_at: Option<OffsetDateTime>,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<AttachmentDetail> for AttachmentDetailDto {
    fn from(view: AttachmentDetail) -> Self {
        Self {
            id: view.id,
            filename: view.filename,
            content_type: view.content_type,
            size_bytes: view.size_bytes,
            status: view.status.into(),
            kind: view.kind.into(),
            error_code: view.error_code,
            doc_summary: view.doc_summary,
            img_thumbnail: view.thumbnail.map(Into::into),
            summary_updated_at: view.summary_updated_at,
            created_at: view.created_at,
        }
    }
}
