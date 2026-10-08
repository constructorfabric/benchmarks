//! Attachment routes: upload (multipart, streamed with a byte counter), get, delete.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Extension;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use bytes::BytesMut;
use futures::TryStreamExt;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;
use toolkit::api::rest::extract::Path;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::License;
use super::dto::AttachmentDetailDto;
use crate::domain::error::DomainError;
use crate::domain::service::Svc;

const TAG: &str = "Mini Chat Attachments";

fn multipart_err(field: &'static str, reason: &'static str, description: impl Into<String>) -> DomainError {
    DomainError::Multipart { field, reason, description: description.into() }
}

/// `POST /chats/{id}/attachments`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn upload_attachment(
    uri: Uri,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path(chat_id): Path<Uuid>,
    body: Body,
) -> Result<Response, CanonicalError> {
    // Chat, model and upload slot are resolved before the body is read.
    let gate = svc.begin_upload(&ctx, chat_id).await?;
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let boundary = multer::parse_boundary(ct)
        .map_err(|_| multipart_err("content_type", "BOUNDARY_REQUIRED", "multipart boundary is required"))?;
    let mut mp = multer::Multipart::new(body.into_data_stream(), boundary);
    loop {
        let field = mp
            .next_field()
            .await
            .map_err(|e| multipart_err("multipart", "MULTIPART_ERROR", format!("unreadable multipart body: {e}")))?;
        let Some(mut field) = field else {
            return Err(multipart_err("file", "MISSING_FILE", "the 'file' field is required").into());
        };
        if field.name() != Some("file") {
            continue;
        }
        let part_ct = field
            .content_type()
            .map(ToString::to_string)
            .ok_or_else(|| multipart_err("content_type", "MISSING_CONTENT_TYPE", "the 'file' part has no content type"))?;
        let plan = svc.plan_part(&gate, &part_ct, field.file_name())?;
        let mut buf = BytesMut::new();
        while let Some(chunk) = field
            .try_next()
            .await
            .map_err(|e| multipart_err("multipart", "MULTIPART_ERROR", format!("unreadable multipart body: {e}")))?
        {
            if (buf.len() + chunk.len()) as u64 > plan.limit_bytes {
                return Err(DomainError::FileTooLarge { limit_bytes: plan.limit_bytes }.into());
            }
            buf.extend_from_slice(&chunk);
        }
        let row = svc.complete_upload(&ctx, gate, plan, buf.freeze()).await?;
        let location = format!("{}/{}", uri.path().trim_end_matches('/'), row.id);
        return Ok((
            StatusCode::CREATED,
            [(header::LOCATION, location)],
            axum::Json(AttachmentDetailDto::from(&row)),
        )
            .into_response());
    }
}

/// `GET /chats/{id}/attachments/{attachment_id}`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path((chat_id, id)): Path<(Uuid, Uuid)>,
) -> Result<axum::Json<AttachmentDetailDto>, CanonicalError> {
    let a = svc.get_attachment(&ctx, chat_id, id).await?;
    Ok(axum::Json(AttachmentDetailDto::from(&a)))
}

/// `DELETE /chats/{id}/attachments/{attachment_id}`.
///
/// # Errors
///
/// Returns a [`CanonicalError`] when the request is rejected or the domain operation fails.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Svc>>,
    Path((chat_id, id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, CanonicalError> {
    svc.delete_attachment(&ctx, chat_id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Registers attachment routes.
pub fn register(router: Router, openapi: &dyn OpenApiRegistry, path: &dyn Fn(&str) -> String) -> Router {
    let router = OperationBuilder::post(path("/chats/{id}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment")
        .tag(TAG)
        .path_param("id", "Chat id")
        .authenticated()
        .require_license_features([&License])
        .multipart_file_request("file", Some("Document or image file"))
        .handler(upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::CREATED, "Attachment uploaded")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_409(openapi)
        .error_429(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(path("/chats/{id}/attachments/{attachment_id}"))
        .operation_id("mini_chat.get_attachment")
        .summary("Get an attachment")
        .tag(TAG)
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .authenticated()
        .require_license_features([&License])
        .handler(get_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::OK, "Attachment")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi);

    OperationBuilder::delete(path("/chats/{id}/attachments/{attachment_id}"))
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag(TAG)
        .path_param("id", "Chat id")
        .path_param("attachment_id", "Attachment id")
        .authenticated()
        .require_license_features([&License])
        .handler(delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Attachment deleted")
        .error_400(openapi).error_401(openapi).error_403(openapi).error_404(openapi).error_409(openapi).error_500(openapi).error_503(openapi)
        .register(router, openapi)
}
