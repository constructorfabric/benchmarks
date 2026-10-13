//! Handlers (OWNER: attachments work package).

use std::sync::Arc;
use std::time::Instant;

use axum::Extension;
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use bytes::{Bytes, BytesMut};
use http::{HeaderMap, StatusCode, header};
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{
    AttachmentDetailDto, AttachmentKindDto, AttachmentStatusDto, ImgThumbnailDto,
};
use crate::domain::error::{DomainError, reasons, resource_types};
use crate::domain::service::Services;
use crate::domain::service::attachments::{
    AttachmentService, FileSpec, UploadTarget, file_too_large, status,
};
use crate::domain::service::thumbnail::THUMBNAIL_CONTENT_TYPE;
use crate::infra::db::entity::attachment;

/// Retry-After of the upload concurrency limit.
const CONCURRENCY_RETRY_AFTER_SECS: u64 = 5;

fn multipart_invalid(field: &str, reason: &str, description: &str) -> DomainError {
    DomainError::invalid(resource_types::ATTACHMENT, field, reason, description)
}

fn multipart_error(e: &multer::Error) -> DomainError {
    tracing::debug!(error = %e, "multipart parse error");
    multipart_invalid(
        "multipart",
        reasons::MULTIPART_ERROR,
        "The multipart body could not be read",
    )
}

/// Builds the public attachment detail (optional fields omitted when null; the thumbnail
/// only for ready images; `error_code` only for failed rows).
#[must_use]
pub fn detail_dto(row: &attachment::Model) -> AttachmentDetailDto {
    let is_image = row.attachment_kind == "image";
    let img_thumbnail = if is_image && row.status == status::READY {
        match (&row.img_thumbnail, row.img_thumbnail_width, row.img_thumbnail_height) {
            (Some(data), Some(width), Some(height)) => Some(ImgThumbnailDto {
                content_type: THUMBNAIL_CONTENT_TYPE.to_owned(),
                width,
                height,
                data_base64: base64::engine::general_purpose::STANDARD.encode(data),
            }),
            _ => None,
        }
    } else {
        None
    };
    AttachmentDetailDto {
        id: row.id,
        filename: row.filename.clone(),
        content_type: row.content_type.clone(),
        size_bytes: row.size_bytes,
        status: match row.status.as_str() {
            status::PENDING => AttachmentStatusDto::Pending,
            status::UPLOADED => AttachmentStatusDto::Uploaded,
            status::READY => AttachmentStatusDto::Ready,
            _ => AttachmentStatusDto::Failed,
        },
        kind: if is_image {
            AttachmentKindDto::Image
        } else {
            AttachmentKindDto::Document
        },
        error_code: if row.status == status::FAILED {
            row.error_code.clone()
        } else {
            None
        },
        doc_summary: None,
        img_thumbnail,
        summary_updated_at: None,
        created_at: row.created_at,
    }
}

/// Reads the `file` part of a multipart body: validates its header through the service, then
/// streams its chunks under the effective size limit.
async fn read_file_part(
    svc: &AttachmentService,
    target: &UploadTarget,
    headers: &HeaderMap,
    body: Body,
) -> Result<(FileSpec, Bytes), DomainError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let boundary = multer::parse_boundary(content_type).map_err(|_| {
        multipart_invalid(
            "content_type",
            reasons::BOUNDARY_REQUIRED,
            "Content-Type must be multipart/form-data with a boundary",
        )
    })?;
    let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
    loop {
        let Some(mut field) = multipart.next_field().await.map_err(|e| multipart_error(&e))? else {
            return Err(multipart_invalid(
                "file",
                reasons::MISSING_FILE,
                "The multipart body has no `file` field",
            ));
        };
        if field.name() != Some("file") {
            continue;
        }
        let Some(part_type) = field.content_type().map(ToString::to_string) else {
            return Err(multipart_invalid(
                "content_type",
                reasons::MISSING_CONTENT_TYPE,
                "The `file` part has no Content-Type",
            ));
        };
        let filename = field.file_name().map(ToOwned::to_owned);
        let spec = svc.validate_file(target, &part_type, filename.as_deref())?;
        let mut buf = BytesMut::new();
        while let Some(chunk) = field.chunk().await.map_err(|e| multipart_error(&e))? {
            let total = u64::try_from(buf.len() + chunk.len()).unwrap_or(u64::MAX);
            if total > spec.max_bytes {
                return Err(file_too_large(spec.max_bytes));
            }
            buf.extend_from_slice(&chunk);
        }
        return Ok((spec, buf.freeze()));
    }
}

pub async fn upload_attachment(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(chat_id): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let started = Instant::now();
    let attachments = &svc.attachments;
    let target = attachments.prepare_upload(&ctx, chat_id).await?;
    let _permit = Arc::clone(&svc.deps.upload_slots)
        .try_acquire_owned()
        .map_err(|_| DomainError::ServiceUnavailable {
            retry_after_secs: CONCURRENCY_RETRY_AFTER_SECS,
            detail: "Too many concurrent uploads".to_owned(),
        })?;
    let (spec, data) = read_file_part(attachments, &target, &headers, body).await?;
    let row = attachments.upload(&ctx, &target, spec, data, started).await?;
    Ok((StatusCode::CREATED, axum::Json(detail_dto(&row))).into_response())
}

pub async fn get_attachment(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path((chat_id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    let row = svc.attachments.get(&ctx, chat_id, attachment_id).await?;
    Ok((StatusCode::OK, axum::Json(detail_dto(&row))).into_response())
}

pub async fn delete_attachment(
    Extension(svc): Extension<Arc<Services>>,
    Extension(ctx): Extension<SecurityContext>,
    Path((chat_id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Response> {
    svc.attachments.delete(&ctx, chat_id, attachment_id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
#[path = "attachments_tests.rs"]
mod attachments_tests;
