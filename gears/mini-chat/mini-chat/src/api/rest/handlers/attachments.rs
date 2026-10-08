//! Attachments: `POST /v1/chats/{id}/attachments` (multipart upload),
//! `GET` / `DELETE /v1/chats/{id}/attachments/{attachment_id}`.

use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use toolkit::api::canonical_prelude::{ApiResult, no_content};
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::AttachmentDetailDto;
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::domain::services::attachment_service::UploadPart;

/// Name of the multipart field carrying the file.
const FILE_FIELD: &str = "file";

fn multipart(field: &'static str, reason: &'static str) -> DomainError {
    DomainError::Multipart { field, reason }
}

/// Upload one file. The chat, its model, the limits and a concurrency slot
/// are resolved before the body is read; the body is streamed through
/// `multer` (fields before `file` are skipped).
///
/// # Errors
/// 400 (multipart, MIME, size, code interpreter, images disabled, invalid
/// model), 403, 404 (`chat`), 409 (`provider_mismatch`), 429
/// (`document_limit` / `storage_limit`), 500, 503 (storage, concurrency).
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<impl IntoResponse> {
    let prepared = svc.attachments.prepare_upload(&ctx, id).await?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    let boundary = multer::parse_boundary(content_type)
        .map_err(|_| multipart("content_type", "BOUNDARY_REQUIRED"))?;
    let mut form = multer::Multipart::new(body.into_data_stream(), boundary);
    let field = loop {
        match form.next_field().await {
            Ok(Some(f)) if f.name() == Some(FILE_FIELD) => break f,
            Ok(Some(_)) => {}
            Ok(None) => return Err(multipart(FILE_FIELD, "MISSING_FILE").into()),
            Err(_) => return Err(multipart("multipart", "MULTIPART_ERROR").into()),
        }
    };
    let part_type = field
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| multipart("content_type", "MISSING_CONTENT_TYPE"))?;
    let part = UploadPart {
        filename: field.file_name().map(str::to_owned),
        content_type: part_type,
        stream: field,
    };
    let detail = svc.attachments.upload(&ctx, prepared, part).await?;
    Ok((StatusCode::CREATED, Json(AttachmentDetailDto::from(detail))))
}

/// Attachment metadata (polled after an `uploaded` upload).
///
/// # Errors
/// 400 (non-UUID path), 403, 404 (`chat` / `attachment`), 500, 503.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    let detail = svc.attachments.get(&ctx, id, attachment_id).await?;
    Ok(Json(AttachmentDetailDto::from(detail)))
}

/// Delete an unreferenced attachment (204; idempotent).
///
/// # Errors
/// 400 (non-UUID path), 403, 404 (`chat` / `attachment`), 409
/// (`attachment_locked`), 500, 503.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    Path((id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.attachments.delete(&ctx, id, attachment_id).await?;
    Ok(no_content())
}
