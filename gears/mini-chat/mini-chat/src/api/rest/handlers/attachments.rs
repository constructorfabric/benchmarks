//! Attachment handlers (DESIGN section 3.3 "Upload Attachment", "Get
//! Attachment"; section 3.6 "File Upload"; "Attachment Deletion"; B.9
//! "Streaming upload").
//!
//! The upload reads the raw request body as a stream: mini-chat sets no
//! route-level body limit (the api-gateway `defaults.body_limit_bytes` is
//! the outer cap) and parses the `multipart/form-data` body incrementally,
//! so an oversized file is rejected as soon as the received bytes exceed the
//! effective limit, without buffering the rest of the body. Every check that
//! does not need the file runs before the body is read: the boundary of the
//! `Content-Type` header, then [`AttachmentService::begin_upload`] (PEP,
//! `404` chat, `INVALID_MODEL`, upload concurrency slot).

use axum::body::Body;
use axum::extract::Extension;
use axum::http::{HeaderMap, header};
use bytes::{Bytes, BytesMut};
use toolkit::api::canonical_prelude::{ApiResult, IntoResponse, JsonBody, StatusCode};
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::RestState;
use crate::api::rest::dto::AttachmentDetailDto;
use crate::domain::attachment_service::{AttachmentService, FileSpec, UploadSession};
use crate::domain::error::{DomainError, InvalidArgumentReason};

/// Name of the multipart field carrying the file.
pub const FILE_FIELD: &str = "file";

/// `POST {prefix}/v1/chats/{id}/attachments` — `multipart/form-data` with a
/// `file` field; 201 with the `AttachmentDetail` (`ready`, or `uploaded`
/// while indexing continues in the background).
///
/// # Errors
///
/// 400 (`BOUNDARY_REQUIRED`, `MULTIPART_ERROR`, `MISSING_FILE`,
/// `MISSING_CONTENT_TYPE`, `FILE_TOO_LARGE`, `UNSUPPORTED_CONTENT_TYPE`,
/// `CODE_INTERPRETER_UNAVAILABLE`, `FEATURE_DISABLED`, `INVALID_MODEL`,
/// path), 403, 404 (chat), 409 (`provider_mismatch`), 429
/// (`document_limit`, `storage_limit`), 500, 503 (`Retry-After`).
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path(chat_id): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<impl IntoResponse> {
    let boundary = multipart_boundary(&headers)?;
    let session = state.attachments.begin_upload(&ctx, chat_id).await?;
    let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
    let (spec, data) = read_file_part(&state.attachments, &session, &mut multipart).await?;
    let detail = state.attachments.upload(session, spec, data).await?;
    Ok((
        StatusCode::CREATED,
        JsonBody::from(AttachmentDetailDto::from(detail)),
    ))
}

/// `GET {prefix}/v1/chats/{id}/attachments/{attachment_id}`.
///
/// # Errors
///
/// 400 (path), 403, 404 (chat, attachment), 500, 503.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path((chat_id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<JsonBody<AttachmentDetailDto>> {
    let detail = state.attachments.get(&ctx, chat_id, attachment_id).await?;
    Ok(JsonBody::from(AttachmentDetailDto::from(detail)))
}

/// `DELETE {prefix}/v1/chats/{id}/attachments/{attachment_id}` — soft
/// delete, 204; repeating it is a no-op.
///
/// # Errors
///
/// 400 (path), 403, 404 (chat, attachment), 409 (`attachment_locked`), 500,
/// 503.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<RestState>,
    Path((chat_id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    state
        .attachments
        .delete(&ctx, chat_id, attachment_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Boundary of a `multipart/form-data` `Content-Type`.
///
/// # Errors
///
/// `BOUNDARY_REQUIRED` when the header is missing, is not
/// `multipart/form-data` or has no boundary.
pub fn multipart_boundary(headers: &HeaderMap) -> Result<String, DomainError> {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| multer::parse_boundary(value).ok())
        .filter(|boundary| !boundary.is_empty())
        .ok_or_else(|| DomainError::invalid_argument(InvalidArgumentReason::BoundaryRequired))
}

fn multipart_error(err: &multer::Error) -> DomainError {
    tracing::debug!(error = %err, "mini-chat upload: unreadable multipart body");
    DomainError::invalid_argument(InvalidArgumentReason::MultipartError)
}

/// Reads the multipart body up to the end of the first `file` part; other
/// parts are skipped. The `file` part headers are classified before its
/// data is read, and the data is counted chunk by chunk against the
/// effective size limit.
///
/// # Errors
///
/// `MULTIPART_ERROR` (unreadable body), `MISSING_FILE`,
/// `MISSING_CONTENT_TYPE`, `FILE_TOO_LARGE` and the classification errors of
/// [`AttachmentService::classify_file`].
async fn read_file_part(
    service: &AttachmentService,
    session: &UploadSession,
    multipart: &mut multer::Multipart<'_>,
) -> Result<(FileSpec, Bytes), DomainError> {
    loop {
        let Some(mut field) = multipart
            .next_field()
            .await
            .map_err(|err| multipart_error(&err))?
        else {
            return Err(DomainError::invalid_argument(
                InvalidArgumentReason::MissingFile,
            ));
        };
        if field.name() != Some(FILE_FIELD) {
            // The unread data of a skipped part is consumed by `next_field`.
            continue;
        }
        let content_type = field
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).trim().to_owned())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                DomainError::invalid_argument(InvalidArgumentReason::MissingContentType)
            })?;
        let spec = service.classify_file(session, field.file_name(), &content_type)?;
        let mut data = BytesMut::new();
        let mut received: u64 = 0;
        while let Some(chunk) = field.chunk().await.map_err(|err| multipart_error(&err))? {
            received = received.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
            spec.check_size(received)?;
            data.extend_from_slice(&chunk);
        }
        return Ok((spec, data.freeze()));
    }
}
