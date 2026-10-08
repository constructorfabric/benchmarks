//! Attachment handlers (multipart upload, status, delete).

use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::BytesMut;
use time::OffsetDateTime;
use toolkit::api::canonical_prelude::*;
use toolkit::api::rest::extract;
use toolkit_macros::api_dto;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{AttachmentKindDto, AttachmentStatusDto, ImgThumbnailDto};
use crate::domain::error::{DomainError, Resource};
use crate::domain::service::Core;
use crate::domain::service::attachments::{UploadInput, normalize_filename, resolve_mime};
use crate::domain::service::messages::thumbnail_of;
use crate::infra::db::entities::attachment;

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct AttachmentDetailDto {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub status: AttachmentStatusDto,
    pub kind: AttachmentKindDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub summary_updated_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<attachment::Model> for AttachmentDetailDto {
    fn from(a: attachment::Model) -> Self {
        let thumb = thumbnail_of(&a).map(Into::into);
        Self {
            id: a.id,
            filename: a.filename,
            content_type: a.content_type,
            size_bytes: a.size_bytes,
            status: match a.status.as_str() {
                "uploaded" => AttachmentStatusDto::Uploaded,
                "ready" => AttachmentStatusDto::Ready,
                "failed" => AttachmentStatusDto::Failed,
                _ => AttachmentStatusDto::Pending,
            },
            kind: if a.attachment_kind == "image" {
                AttachmentKindDto::Image
            } else {
                AttachmentKindDto::Document
            },
            error_code: if a.status == "failed" {
                a.error_code
            } else {
                None
            },
            doc_summary: a.doc_summary,
            img_thumbnail: thumb,
            summary_updated_at: a.summary_updated_at,
            created_at: a.created_at,
        }
    }
}

fn mp_err(field: &'static str, reason: &'static str, desc: impl Into<String>) -> CanonicalError {
    DomainError::invalid(Resource::Attachment, field, reason, desc).into()
}

/// Uploads a multipart file attachment to a chat.
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): Extension<Arc<Core>>,
    extract::Path(id): extract::Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let uc = core.prepare_upload(&ctx, id).await?;
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let boundary = multer::parse_boundary(&ct).map_err(|_| {
        mp_err(
            "content_type",
            "BOUNDARY_REQUIRED",
            "multipart boundary is required",
        )
    })?;
    let _permit = Arc::clone(&core.upload_slots)
        .try_acquire_owned()
        .map_err(|_| {
            CanonicalError::from(DomainError::ServiceUnavailable {
                retry_after: 5,
                detail: "Too many concurrent uploads".to_owned(),
            })
        })?;
    let mut mp = multer::Multipart::new(body.into_data_stream(), boundary);
    let mut input: Option<UploadInput> = None;
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(mp_err("multipart", "MULTIPART_ERROR", e.to_string())),
        };
        if field.name() != Some("file") {
            continue;
        }
        let filename = normalize_filename(field.file_name());
        let part_ct = field
            .content_type()
            .map(ToString::to_string)
            .ok_or_else(|| {
                mp_err(
                    "content_type",
                    "MISSING_CONTENT_TYPE",
                    "the file part has no content type",
                )
            })?;
        let mime = resolve_mime(&part_ct, &filename, core.cfg.rag.allow_csv_upload);
        uc.validate_upload_type(&mime)?;
        let limit = uc.limit_for(&mime);
        let mut buf = BytesMut::new();
        let mut field = field;
        loop {
            match field.chunk().await {
                Ok(Some(c)) => {
                    if (buf.len() + c.len()) as u64 > limit {
                        return Err(DomainError::out_of_range(
                            Resource::Attachment,
                            "content_length",
                            "FILE_TOO_LARGE",
                            format!("file exceeds the {limit} byte limit"),
                        )
                        .into());
                    }
                    buf.extend_from_slice(&c);
                }
                Ok(None) => break,
                Err(e) => return Err(mp_err("multipart", "MULTIPART_ERROR", e.to_string())),
            }
        }
        input = Some(UploadInput {
            filename,
            content_type: mime,
            data: buf.freeze(),
        });
        break;
    }
    let input =
        input.ok_or_else(|| mp_err("file", "MISSING_FILE", "the 'file' field is required"))?;
    let row = match core.upload_attachment(&ctx, uc, input).await {
        Ok(r) => {
            core.metrics.upload(&r.attachment_kind, &r.status);
            r
        }
        Err(e) => {
            core.metrics.upload("unknown", "rejected");
            return Err(e.into());
        }
    };
    Ok((StatusCode::CREATED, Json(AttachmentDetailDto::from(row))).into_response())
}

/// Returns attachment metadata.
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): Extension<Arc<Core>>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    Ok(Json(
        core.get_attachment(&ctx, id, attachment_id).await?.into(),
    ))
}

/// Deletes an attachment.
///
/// # Errors
/// Returns the mapped canonical error when the domain operation fails.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(core): Extension<Arc<Core>>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    core.delete_attachment(&ctx, id, attachment_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
