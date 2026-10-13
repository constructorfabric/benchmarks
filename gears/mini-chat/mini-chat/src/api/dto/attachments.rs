//! Attachment API DTOs (`AttachmentDetailDto`). The kind, status and thumbnail schemas are shared
//! with the message list (`crate::api::dto::messages`).

use time::OffsetDateTime;
use uuid::Uuid;

use crate::api::dto::messages::{AttachmentKindDto, AttachmentStatusDto, ImgThumbnailDto};
use crate::domain::attachment::AttachmentView;

// Doc comments of these DTOs become the published schema descriptions (`docs/api/api.json`).

/// Full attachment details returned by the GET attachment endpoint.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentDetailDto {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub status: AttachmentStatusDto,
    pub kind: AttachmentKindDto,
    // Omitted unless the attachment failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    // Document summaries are not implemented: always omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_summary: Option<String>,
    // Omitted unless the attachment is a ready image with a preview.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
    // Document summaries are not implemented: always omitted.
    #[serde(
        with = "crate::api::dto::timestamp::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub summary_updated_at: Option<OffsetDateTime>,
    #[serde(with = "crate::api::dto::timestamp")]
    pub created_at: OffsetDateTime,
}

impl From<AttachmentView> for AttachmentDetailDto {
    fn from(a: AttachmentView) -> Self {
        Self {
            id: a.id,
            filename: a.filename,
            content_type: a.content_type,
            size_bytes: a.size_bytes,
            status: a.status.into(),
            kind: a.kind.into(),
            error_code: a.error_code,
            doc_summary: None,
            img_thumbnail: a.thumbnail.map(Into::into),
            summary_updated_at: None,
            created_at: a.created_at,
        }
    }
}
