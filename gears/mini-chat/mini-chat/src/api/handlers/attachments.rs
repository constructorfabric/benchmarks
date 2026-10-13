//! Attachment routes: upload (multipart), get, delete (DESIGN §3.3 "Get Attachment" /
//! "Upload Attachment", §4 "Attachment Deletion").

use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::response::IntoResponse;
use base64::Engine as _;
use bytes::{Bytes, BytesMut};
use http::{HeaderMap, StatusCode};
use time::OffsetDateTime;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::ApiResult;
use toolkit::api::operation_builder::{OperationBuilder, ResponseHeaderSpec, ResponseHeaderType};
use toolkit::api::rest::extract::Path;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::handlers::{License, V1};
use crate::domain::attachments::{self, CONCURRENCY_RETRY_AFTER_SECS, STATUS_FAILED, STATUS_READY, thumbnail};
use crate::domain::error::{DomainError, Resource};
use crate::domain::services::AppServices;
use crate::infra::db::entities::attachment;

const TAG: &str = "Mini Chat Attachments";

/// Attachment kind.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKindDto {
    Document,
    Image,
}

/// Attachment lifecycle status.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentStatusDto {
    Pending,
    Uploaded,
    Ready,
    Failed,
}

/// Server-generated preview thumbnail for an image attachment.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImgThumbnailDto {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data_base64: String,
}

/// Full attachment details returned by the GET attachment endpoint.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, PartialEq)]
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
    #[serde(skip_serializing_if = "Option::is_none", with = "time::serde::rfc3339::option")]
    pub summary_updated_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl AttachmentKindDto {
    #[must_use]
    pub fn from_db(kind: &str) -> Self {
        if kind == "image" { Self::Image } else { Self::Document }
    }
}

impl AttachmentStatusDto {
    #[must_use]
    pub fn from_db(status: &str) -> Self {
        match status {
            "pending" => Self::Pending,
            "uploaded" => Self::Uploaded,
            "ready" => Self::Ready,
            _ => Self::Failed,
        }
    }
}

/// Thumbnail of a row: only for `ready` images that have one.
#[must_use]
pub fn thumbnail_dto(row: &attachment::Model) -> Option<ImgThumbnailDto> {
    if row.status != STATUS_READY || row.attachment_kind != "image" {
        return None;
    }
    let bytes = row.img_thumbnail.as_ref()?;
    Some(ImgThumbnailDto {
        content_type: thumbnail::THUMBNAIL_CONTENT_TYPE.to_owned(),
        width: row.img_thumbnail_width.unwrap_or(0),
        height: row.img_thumbnail_height.unwrap_or(0),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

impl From<&attachment::Model> for AttachmentDetailDto {
    fn from(row: &attachment::Model) -> Self {
        Self {
            id: row.id,
            filename: row.filename.clone(),
            content_type: row.content_type.clone(),
            size_bytes: row.size_bytes,
            status: AttachmentStatusDto::from_db(&row.status),
            kind: AttachmentKindDto::from_db(&row.attachment_kind),
            error_code: if row.status == STATUS_FAILED { row.error_code.clone() } else { None },
            doc_summary: None,
            img_thumbnail: thumbnail_dto(row),
            summary_updated_at: None,
            created_at: row.created_at,
        }
    }
}

/// Registers this area's routes.
pub fn register(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    let retry_after = || ResponseHeaderSpec::new("Retry-After", "Seconds to wait before retrying", ResponseHeaderType::Integer);
    let router = OperationBuilder::post(format!("{V1}/chats/{{id}}/attachments"))
        .operation_id("mini_chat.upload_attachment")
        .summary("Upload an attachment to a chat")
        .tag(TAG)
        .path_param("id", "Chat UUID")
        .multipart_file_request("file", Some("File to upload"))
        .authenticated()
        .require_license_features::<License>([License])
        .handler(upload_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::CREATED, "Attachment uploaded and processed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);
    let router = OperationBuilder::get(format!("{V1}/chats/{{id}}/attachments/{{attachment_id}}"))
        .operation_id("mini_chat.get_attachment")
        .summary("Get attachment metadata")
        .tag(TAG)
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(get_attachment)
        .json_response_with_schema::<AttachmentDetailDto>(openapi, StatusCode::OK, "Attachment metadata")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);
    OperationBuilder::delete(format!("{V1}/chats/{{id}}/attachments/{{attachment_id}}"))
        .operation_id("mini_chat.delete_attachment")
        .summary("Delete an attachment")
        .tag(TAG)
        .path_param("id", "Chat UUID")
        .path_param("attachment_id", "Attachment UUID")
        .authenticated()
        .require_license_features::<License>([License])
        .handler(delete_attachment)
        .no_content_response(StatusCode::NO_CONTENT, "Attachment deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi)
}

fn invalid(field: &str, reason: &str, description: impl Into<String>) -> DomainError {
    DomainError::invalid(Resource::Attachment, field, reason, description)
}

fn multipart_error(e: &multer::Error) -> DomainError {
    invalid("multipart", "MULTIPART_ERROR", format!("invalid multipart body: {e}"))
}

/// `POST /chats/{id}/attachments`.
///
/// # Errors
/// Canonical errors of DESIGN §3.3 "Upload Attachment".
pub async fn upload_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path(chat_id): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<impl IntoResponse> {
    // Chat and model are resolved before the body is read.
    let target = attachments::prepare_upload(&app, &ctx, chat_id).await?;
    let content_type = headers.get(http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let boundary = multer::parse_boundary(content_type)
        .map_err(|_| invalid("content_type", "BOUNDARY_REQUIRED", "multipart boundary is required"))?;
    let mut multipart = multer::Multipart::new(body.into_data_stream(), boundary);
    let (meta, data, _permit) = loop {
        let Some(mut field) = multipart.next_field().await.map_err(|e| multipart_error(&e))? else {
            return Err(invalid("file", "MISSING_FILE", "multipart field 'file' is required").into());
        };
        if field.name() != Some("file") {
            continue;
        }
        let Some(part_type) = field.content_type().map(ToString::to_string) else {
            return Err(invalid("content_type", "MISSING_CONTENT_TYPE", "the file part has no content type").into());
        };
        let meta = target.validate_part(&app, field.file_name(), &part_type)?;
        let permit = Arc::clone(&app.upload_permits)
            .try_acquire_owned()
            .map_err(|_| DomainError::unavailable(CONCURRENCY_RETRY_AFTER_SECS, "upload concurrency limit reached"))?;
        let mut buf = BytesMut::new();
        while let Some(chunk) = field.chunk().await.map_err(|e| multipart_error(&e))? {
            if (buf.len() + chunk.len()) as u64 > meta.max_bytes {
                return Err(attachments::file_too_large(meta.max_bytes).into());
            }
            buf.extend_from_slice(&chunk);
        }
        break (meta, Bytes::from(buf), permit);
    };
    let row = attachments::store_upload(&app, &ctx, &target, meta, data).await?;
    Ok((StatusCode::CREATED, axum::Json(AttachmentDetailDto::from(&row))))
}

/// `GET /chats/{id}/attachments/{attachment_id}`.
///
/// # Errors
/// 404 chat / attachment, authz errors.
pub async fn get_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path((chat_id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<axum::Json<AttachmentDetailDto>> {
    let row = attachments::get(&app, &ctx, chat_id, attachment_id).await?;
    Ok(axum::Json(AttachmentDetailDto::from(&row)))
}

/// `DELETE /chats/{id}/attachments/{attachment_id}`.
///
/// # Errors
/// 404 chat / attachment, 409 `attachment_locked`, authz errors.
pub async fn delete_attachment(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path((chat_id, attachment_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    attachments::delete(&app, &ctx, chat_id, attachment_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
#[path = "attachments_tests.rs"]
mod tests;
