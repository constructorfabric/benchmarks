//! Attachment handlers (multipart upload with streaming size check).

use std::sync::Arc;
use std::time::Instant;

use axum::Extension;
use axum::body::Body;
use axum::http::HeaderMap;
use axum::response::Response;
use bytes::BytesMut;
use toolkit::api::canonical_prelude::*;
use toolkit::api::rest::extract;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::AttachmentDetailDto;
use crate::domain::attachment_service::{normalize_filename, resolve_mime};
use crate::domain::error::DomainError;
use crate::domain::service::MiniChat;

fn invalid(field: &'static str, reason: &'static str, detail: impl Into<String>) -> DomainError {
    DomainError::invalid(field, reason, detail)
}

/// Upload an attachment to a chat (multipart `file` field).
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<MiniChat>>,
    extract::Path(id): extract::Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let started = Instant::now();
    let up = svc.prepare_upload(&ctx, id).await?;
    let _permit = Arc::clone(&svc.upload_slots)
        .try_acquire_owned()
        .map_err(|_| DomainError::UploadConcurrency)?;
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let boundary = multer::parse_boundary(ct)
        .map_err(|_| invalid("content_type", "BOUNDARY_REQUIRED", "multipart boundary is required"))?;
    let mut mp = multer::Multipart::new(body.into_data_stream(), boundary);
    let mut field = loop {
        match mp.next_field().await {
            Err(e) => return Err(invalid("multipart", "MULTIPART_ERROR", format!("invalid multipart body: {e}")).into()),
            Ok(None) => return Err(invalid("file", "MISSING_FILE", "the 'file' field is required").into()),
            Ok(Some(f)) if f.name() == Some("file") => break f,
            Ok(Some(_)) => {}
        }
    };
    let raw_ct = field
        .content_type()
        .map(ToString::to_string)
        .ok_or_else(|| invalid("content_type", "MISSING_CONTENT_TYPE", "the file part has no content type"))?;
    let filename = normalize_filename(field.file_name());
    let mime = resolve_mime(&raw_ct, &filename, svc.cfg.rag.allow_csv_upload)?;
    let (image, _, _) = MiniChat::check_kind(&up, &mime)?;
    let limit = if image { up.limits.image_max_bytes } else { up.limits.document_max_bytes };
    let mut buf = BytesMut::new();
    loop {
        match field.chunk().await {
            Ok(Some(chunk)) => {
                buf.extend_from_slice(&chunk);
                if buf.len() as u64 > limit {
                    return Err(DomainError::FileTooLarge(format!("the file exceeds the limit of {limit} bytes")).into());
                }
            }
            Ok(None) => break,
            Err(e) => return Err(invalid("multipart", "MULTIPART_ERROR", format!("invalid multipart body: {e}")).into()),
        }
    }
    let view = svc
        .complete_upload(&ctx, up, filename, mime, buf.freeze(), started)
        .await?;
    Ok((StatusCode::CREATED, Json(AttachmentDetailDto::from(view))).into_response())
}

/// Get an attachment by id.
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<MiniChat>>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    Ok(Json(svc.get_attachment(&ctx, id, attachment_id).await?.into()))
}

/// Delete an attachment.
///
/// # Errors
///
/// Returns an API error when the request is invalid, the caller is not
/// authorized, the target does not exist, or the operation fails.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<MiniChat>>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    svc.delete_attachment(&ctx, id, attachment_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
