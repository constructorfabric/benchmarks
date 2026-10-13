//! Attachment handlers: `mini_chat.{upload,get,delete}_attachment`.

use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use http::HeaderMap;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::dto::attachments::AttachmentDetailDto;
use crate::api::state::AppServices;

/// `mini_chat.upload_attachment`: reads the raw multipart body (no route-level body limit) and
/// answers 201 with the attachment (`ready`, or `uploaded` while indexing goes on).
///
/// # Errors
/// See DESIGN "Upload Attachment": 400 / 404 / 409 / 429 / 500 / 503 problems.
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path(chat_id): extract::Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<impl IntoResponse> {
    let view = svc
        .attachments
        .upload(&ctx, chat_id, &headers, body)
        .await?;
    Ok((StatusCode::CREATED, Json(AttachmentDetailDto::from(view))))
}

/// `mini_chat.get_attachment`.
///
/// # Errors
/// 404 for a missing chat or attachment (also deleted, foreign or uploaded by someone else),
/// 403 / 503 from the PDP.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path((chat_id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<Json<AttachmentDetailDto>> {
    let view = svc.attachments.get(&ctx, chat_id, attachment_id).await?;
    Ok(Json(view.into()))
}

/// `mini_chat.delete_attachment`: 204, also when already deleted.
///
/// # Errors
/// 404 for a missing chat or attachment, 409 `attachment_locked` when a message references it,
/// 403 / 503 from the PDP.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<AppServices>>,
    extract::Path((chat_id, attachment_id)): extract::Path<(Uuid, Uuid)>,
) -> ApiResult<impl IntoResponse> {
    svc.attachments.delete(&ctx, chat_id, attachment_id).await?;
    Ok(no_content())
}
