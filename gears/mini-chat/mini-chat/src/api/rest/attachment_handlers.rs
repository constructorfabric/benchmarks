//! Handlers of the attachment endpoints. The caller comes from the
//! `SecurityContext` the api-gateway auth layer injects. The upload takes the
//! raw request body: the service resolves the chat and the model first and only
//! then parses the multipart document.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::Extension;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use futures::StreamExt;
use toolkit::api::canonical_prelude::*;
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::attachment_dto::AttachmentDetailDto;
use crate::domain::attachments::{AttachmentService, UploadStream};

/// `POST {prefix}/v1/chats/{id}/attachments`
///
/// # Errors
/// Canonical `Problem`: 400 (`BOUNDARY_REQUIRED`, `MULTIPART_ERROR`,
/// `MISSING_FILE`, `MISSING_CONTENT_TYPE`, `UNSUPPORTED_CONTENT_TYPE`,
/// `FILE_TOO_LARGE`, `CODE_INTERPRETER_UNAVAILABLE`, `VISION_NOT_SUPPORTED`,
/// `FEATURE_DISABLED`, `INVALID_MODEL`, `invalid_path_params`), 403, 404, 409
/// (`provider_mismatch`), 429 (`document_limit`, `storage_limit`), 503
/// (provider failure, concurrency limit, PDP), 500.
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<AttachmentService>>,
    Path(chat_id): Path<Uuid>,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<impl IntoResponse> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let stream: UploadStream = Box::pin(
        body.into_data_stream()
            .map(|chunk| chunk.map_err(std::io::Error::other)),
    );
    let attachment = service.upload(&ctx, chat_id, content_type, stream).await?;
    let id = attachment.id.to_string();
    Ok(created_json(
        AttachmentDetailDto::from(attachment),
        &uri,
        &id,
    ))
}

/// `GET {prefix}/v1/chats/{id}/attachments/{attachment_id}`
///
/// # Errors
/// Canonical `Problem`: 400 (`invalid_path_params`), 403, 404 (chat or
/// attachment), 503 when the PDP is unreachable, 500.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<AttachmentService>>,
    Path((chat_id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    Ok(Json(
        service.get(&ctx, chat_id, attachment_id).await?.into(),
    ))
}

/// `DELETE {prefix}/v1/chats/{id}/attachments/{attachment_id}`
///
/// # Errors
/// Canonical `Problem`: 400 (`invalid_path_params`), 403, 404, 409
/// (`attachment_locked`), 503 when the PDP is unreachable, 500.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<AttachmentService>>,
    Path((chat_id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    service.delete(&ctx, chat_id, attachment_id).await?;
    Ok(StatusCode::NO_CONTENT)
}
