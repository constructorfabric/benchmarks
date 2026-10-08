//! Attachment handlers (DESIGN §3.3 "Upload/Get Attachment", §4 "Attachment
//! Deletion").
//!
//! The upload handler reads the raw body itself: the chat, the model and the
//! limits are checked before any body byte is read, and the `file` part is
//! streamed with a byte counter that aborts as soon as the limit is exceeded.

use axum::Extension;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use bytes::BytesMut;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::AttachmentDetailDto;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::services::{Services, UploadContext, UploadPart};

/// Name of the multipart field carrying the file.
const FILE_FIELD: &str = "file";

/// `POST /v1/chats/{id}/attachments` (`multipart/form-data`) → 201.
///
/// # Errors
/// Canonical problem for authorization, model, multipart, validation, limit
/// and storage failures.
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path(id): extract::Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<impl IntoResponse> {
    let prepared = svc.attachments.prepare_upload(&ctx, id).await?;
    let part = read_file_part(&headers, body, &prepared).await?;
    let row = svc.attachments.upload(prepared, part).await?;
    Ok((
        StatusCode::CREATED,
        Json(AttachmentDetailDto::try_from(row)?),
    ))
}

/// `GET /v1/chats/{id}/attachments/{attachment_id}`.
///
/// # Errors
/// Canonical problem for authorization and not-found failures.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    let row = svc.attachments.get(&ctx, id, attachment_id).await?;
    Ok(Json(AttachmentDetailDto::try_from(row)?))
}

/// `DELETE /v1/chats/{id}/attachments/{attachment_id}` → 204.
///
/// # Errors
/// Canonical problem for authorization, not-found and locked attachments.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Services>,
    extract::Path((id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.attachments.delete(&ctx, id, attachment_id).await?;
    Ok(no_content())
}

fn multipart_error(field: &'static str, reason: &'static str, detail: &str) -> DomainError {
    DomainError::Multipart {
        field,
        reason,
        detail: detail.to_owned(),
    }
}

fn unreadable(e: &multer::Error) -> DomainError {
    tracing::debug!(error = %e, "unreadable multipart body");
    multipart_error(
        "multipart",
        "MULTIPART_ERROR",
        "The multipart body cannot be read",
    )
}

/// The `file` part: validated from its headers, then read under its size limit.
async fn read_file_part(
    headers: &HeaderMap,
    body: Body,
    prepared: &UploadContext,
) -> DomainResult<UploadPart> {
    let boundary = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| multer::parse_boundary(ct).ok())
        .ok_or_else(|| {
            multipart_error(
                "content_type",
                "BOUNDARY_REQUIRED",
                "Content-Type must be multipart/form-data with a boundary",
            )
        })?;
    let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
    loop {
        let Some(mut field) = multipart.next_field().await.map_err(|e| unreadable(&e))? else {
            return Err(multipart_error(
                "file",
                "MISSING_FILE",
                "The request has no `file` part",
            ));
        };
        if field.name() != Some(FILE_FIELD) {
            continue;
        }
        let content_type = field.content_type().map(ToString::to_string);
        let meta = prepared.validate_part(field.file_name(), content_type.as_deref())?;
        let limit = usize::try_from(meta.limit_bytes).unwrap_or(usize::MAX);
        let mut buf = BytesMut::new();
        while let Some(chunk) = field.chunk().await.map_err(|e| unreadable(&e))? {
            if buf.len().saturating_add(chunk.len()) > limit {
                return Err(DomainError::FileTooLarge {
                    limit_bytes: meta.limit_bytes,
                });
            }
            buf.extend_from_slice(&chunk);
        }
        return Ok(UploadPart {
            meta,
            bytes: buf.freeze(),
        });
    }
}
