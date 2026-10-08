//! Message list and reaction handlers (DESIGN §3.3 "List Messages", "Message Reaction API").

use std::sync::Arc;

use axum::Extension;
use base64::Engine as _;
use time::OffsetDateTime;
use toolkit::api::OpenApiRegistry;
use toolkit::api::canonical_prelude::{ApiResult, StatusCode};
use toolkit::api::odata::OData;
use toolkit::api::operation_builder::{OperationBuilder, OperationBuilderODataExt, ResponseHeaderSpec, ResponseHeaderType};
use toolkit::api::rest::extract::{Json, Path};
use toolkit_odata::Page;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::{License, V1};
use crate::domain::messages::{self, AttachmentSummary, MessageField, MessageView};
use crate::domain::reactions::{self, ReactionView};
use crate::domain::services::AppServices;

/// Attachment kind.
#[toolkit_macros::api_dto(request, response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKindDto {
    Document,
    Image,
}

impl AttachmentKindDto {
    #[must_use]
    pub fn from_db(kind: &str) -> Self {
        if kind == "image" { Self::Image } else { Self::Document }
    }
}

/// Attachment lifecycle status.
#[toolkit_macros::api_dto(request, response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentStatusDto {
    Pending,
    Uploaded,
    Ready,
    Failed,
}

impl AttachmentStatusDto {
    #[must_use]
    pub fn from_db(status: &str) -> Self {
        match status {
            "uploaded" => Self::Uploaded,
            "ready" => Self::Ready,
            "failed" => Self::Failed,
            _ => Self::Pending,
        }
    }
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

impl ImgThumbnailDto {
    /// WebP thumbnail bytes as the wire object.
    #[must_use]
    pub fn webp(data: &[u8], width: i32, height: i32) -> Self {
        Self {
            content_type: "image/webp".to_owned(),
            width,
            height,
            data_base64: base64::engine::general_purpose::STANDARD.encode(data),
        }
    }
}

/// Lightweight attachment metadata embedded in Message responses.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentSummaryDto {
    pub attachment_id: Uuid,
    pub kind: AttachmentKindDto,
    pub filename: String,
    pub status: AttachmentStatusDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

impl From<AttachmentSummary> for AttachmentSummaryDto {
    fn from(a: AttachmentSummary) -> Self {
        Self {
            attachment_id: a.attachment_id,
            kind: AttachmentKindDto::from_db(&a.kind),
            filename: a.filename,
            status: AttachmentStatusDto::from_db(&a.status),
            img_thumbnail: a.thumbnail.map(|t| ImgThumbnailDto::webp(&t.data, t.width, t.height)),
        }
    }
}

/// Message author role.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRoleDto {
    User,
    Assistant,
    System,
}

impl MessageRoleDto {
    #[must_use]
    pub fn from_db(role: &str) -> Self {
        match role {
            "assistant" => Self::Assistant,
            "system" => Self::System,
            _ => Self::User,
        }
    }
}

/// Reaction value.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionKindDto {
    Like,
    Dislike,
}

impl ReactionKindDto {
    #[must_use]
    pub fn from_db(reaction: &str) -> Option<Self> {
        match reaction {
            "like" => Some(Self::Like),
            "dislike" => Some(Self::Dislike),
            _ => None,
        }
    }
}

/// Response DTO for a message in the list endpoint.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct MiniChatMessageDto {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: MessageRoleDto,
    pub content: String,
    pub attachments: Vec<AttachmentSummaryDto>,
    /// The caller's reaction to this message; `null` when there is none.
    pub my_reaction: Option<ReactionKindDto>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
}

impl From<MessageView> for MiniChatMessageDto {
    fn from(v: MessageView) -> Self {
        let m = v.message;
        Self {
            id: m.id,
            request_id: v.request_id,
            role: MessageRoleDto::from_db(&m.role),
            content: m.content,
            attachments: v.attachments.into_iter().map(AttachmentSummaryDto::from).collect(),
            my_reaction: v.my_reaction.as_deref().and_then(ReactionKindDto::from_db),
            created_at: m.created_at,
            model: m.model,
            input_tokens: (m.input_tokens != 0).then_some(m.input_tokens),
            output_tokens: (m.output_tokens != 0).then_some(m.output_tokens),
        }
    }
}

/// Request DTO for setting a reaction.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct SetReactionReq {
    pub reaction: String,
}

/// Response DTO for a reaction.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct MiniChatReactionDto {
    pub message_id: Uuid,
    pub reaction: ReactionKindDto,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<ReactionView> for MiniChatReactionDto {
    fn from(v: ReactionView) -> Self {
        Self {
            message_id: v.message_id,
            reaction: ReactionKindDto::from_db(&v.reaction).unwrap_or(ReactionKindDto::Like),
            created_at: v.created_at,
        }
    }
}

/// `GET /chats/{id}/messages`.
///
/// # Errors
/// Canonical problem responses mapped from the domain errors (ADR-0004).
pub async fn list_messages(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path(chat_id): Path<Uuid>,
    OData(query): OData,
) -> ApiResult<axum::Json<Page<MiniChatMessageDto>>> {
    let page = messages::list_messages(&app, &ctx, chat_id, &query).await?;
    Ok(axum::Json(page.map_items(MiniChatMessageDto::from)))
}

/// `PUT /chats/{id}/messages/{msg_id}/reaction`.
///
/// # Errors
/// Canonical problem responses mapped from the domain errors (ADR-0004).
pub async fn put_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path((chat_id, msg_id)): Path<(Uuid, Uuid)>,
    Json(req): Json<SetReactionReq>,
) -> ApiResult<axum::Json<MiniChatReactionDto>> {
    let view = reactions::set_reaction(&app, &ctx, chat_id, msg_id, &req.reaction).await?;
    Ok(axum::Json(MiniChatReactionDto::from(view)))
}

/// `DELETE /chats/{id}/messages/{msg_id}/reaction`.
///
/// # Errors
/// Canonical problem responses mapped from the domain errors (ADR-0004).
pub async fn delete_reaction(
    Extension(ctx): Extension<SecurityContext>,
    Extension(app): Extension<Arc<AppServices>>,
    Path((chat_id, msg_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    reactions::delete_reaction(&app, &ctx, chat_id, msg_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn retry_after() -> ResponseHeaderSpec {
    ResponseHeaderSpec::new("Retry-After", "Seconds to wait before retrying", ResponseHeaderType::Integer)
}

/// Registers this area's routes.
pub fn register(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    let reaction_path = format!("{V1}/chats/{{id}}/messages/{{msg_id}}/reaction");

    let router = OperationBuilder::get(format!("{V1}/chats/{{id}}/messages"))
        .operation_id("mini_chat.list_messages")
        .summary("List messages in a chat")
        .tag("Mini Chat Messages")
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat UUID")
        .query_param_typed("limit", false, "Maximum number of messages to return", "integer")
        .query_param("cursor", false, "Cursor for pagination")
        .with_odata_filter::<MessageField>()
        .with_odata_orderby::<MessageField>()
        .handler(list_messages)
        .json_response_with_schema::<Page<MiniChatMessageDto>>(openapi, StatusCode::OK, "Paginated list of messages")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    let router = OperationBuilder::put(reaction_path.clone())
        .operation_id("mini_chat.put_reaction")
        .summary("Set or update a reaction on a message")
        .tag("Mini Chat Reactions")
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .json_request::<SetReactionReq>(openapi, "Reaction data")
        .handler(put_reaction)
        .json_response_with_schema::<MiniChatReactionDto>(openapi, StatusCode::OK, "Reaction set")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_422(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi);

    OperationBuilder::delete(reaction_path)
        .operation_id("mini_chat.delete_reaction")
        .summary("Remove a reaction from a message")
        .tag("Mini Chat Reactions")
        .authenticated()
        .require_license_features::<License>([License])
        .path_param("id", "Chat UUID")
        .path_param("msg_id", "Message UUID")
        .handler(delete_reaction)
        .no_content_response(StatusCode::NO_CONTENT, "Reaction removed")
        .error_400(openapi)
        .error_401(openapi)
        .error_403(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .error_503(openapi)
        .response_header(retry_after())
        .register(router, openapi)
}

#[cfg(test)]
#[path = "messages_tests.rs"]
mod tests;
