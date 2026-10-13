//! Attachment handlers (DESIGN section 3.3, "Get / Upload Attachment",
//! section 4 "Attachment Deletion").

use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use bytes::BytesMut;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::rest::extract::{Json, Path};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::AttachmentDetailDto;
use crate::domain::error::DomainError;
use crate::domain::services::attachment_service::UploadedFile;
use crate::gear::AppState;

/// The multipart field carrying the file.
const FILE_FIELD: &str = "file";

/// `POST {prefix}/v1/chats/{id}/attachments` (`multipart/form-data`, field
/// `file`) — 201 with the attachment.
///
/// The chat, its model and the upload limits are resolved before the body is
/// read; the `file` part is then read with a byte counter against the limit
/// for its content type.
///
/// # Errors
/// Canonical 400/403/404/409/429/500/503.
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path(chat_id): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<(StatusCode, Json<AttachmentDetailDto>)> {
    let plan = st.attachments.begin_upload(&ctx, chat_id).await?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());
    let file = read_file_part(content_type, body, |name, ct| plan.size_limit(name, ct)).await?;
    let view = st.attachments.upload(&ctx, plan, file).await?;
    Ok((StatusCode::CREATED, Json(view.into())))
}

/// `GET {prefix}/v1/chats/{id}/attachments/{attachment_id}`
///
/// # Errors
/// Canonical 400 (path), 403, 404, 500.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path((chat_id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    Ok(Json(
        st.attachments
            .get(&ctx, chat_id, attachment_id)
            .await?
            .into(),
    ))
}

/// `DELETE {prefix}/v1/chats/{id}/attachments/{attachment_id}` — 204.
///
/// # Errors
/// Canonical 400 (path), 403, 404, 409 (`attachment_locked`), 500.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(st): Extension<Arc<AppState>>,
    Path((chat_id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    st.attachments.delete(&ctx, chat_id, attachment_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Reads the `file` part of a multipart body (other fields are skipped). The
/// byte limit comes from `limit(filename, content_type)` of the part; the
/// read stops with `FileTooLarge` as soon as it is exceeded.
///
/// # Errors
/// `Multipart` (`BOUNDARY_REQUIRED`, `MULTIPART_ERROR`, `MISSING_FILE`,
/// `MISSING_CONTENT_TYPE`), `FileTooLarge`.
pub(crate) async fn read_file_part(
    content_type: Option<&str>,
    body: Body,
    limit: impl Fn(Option<&str>, &str) -> u64 + Send,
) -> Result<UploadedFile, DomainError> {
    let boundary = content_type
        .and_then(|ct| multer::parse_boundary(ct).ok())
        .ok_or(DomainError::Multipart {
            reason: "BOUNDARY_REQUIRED",
            field: "content_type",
        })?;
    let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
    loop {
        let Some(mut field) = multipart
            .next_field()
            .await
            .map_err(|e| multipart_error(&e))?
        else {
            return Err(DomainError::Multipart {
                reason: "MISSING_FILE",
                field: "file",
            });
        };
        if field.name() != Some(FILE_FIELD) {
            continue;
        }
        let filename = field.file_name().map(str::to_owned);
        let content_type =
            field
                .content_type()
                .map(ToString::to_string)
                .ok_or(DomainError::Multipart {
                    reason: "MISSING_CONTENT_TYPE",
                    field: "content_type",
                })?;
        let max = limit(filename.as_deref(), &content_type);
        let mut buf = BytesMut::new();
        while let Some(chunk) = field.chunk().await.map_err(|e| multipart_error(&e))? {
            let total = u64::try_from(buf.len() + chunk.len()).unwrap_or(u64::MAX);
            if total > max {
                return Err(DomainError::FileTooLarge);
            }
            buf.extend_from_slice(&chunk);
        }
        return Ok(UploadedFile {
            filename,
            content_type,
            bytes: buf.freeze(),
        });
    }
}

fn multipart_error(e: &multer::Error) -> DomainError {
    tracing::debug!(error = %e, "unreadable multipart body");
    DomainError::Multipart {
        reason: "MULTIPART_ERROR",
        field: "multipart",
    }
}

#[cfg(test)]
#[path = "attachments_tests.rs"]
mod attachments_tests;
