//! REST DTOs (`docs/api/api.json`, operations `mini_chat.*`).

use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::views::{
    AttachmentSummaryView, AttachmentView, ChatView, MessageView, ModelView, ReactionView,
    ThumbnailView, TurnStatusView,
};

/// Request DTO for creating a new chat.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct CreateChatReq {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// Request DTO for updating a chat title.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct UpdateChatReq {
    pub title: String,
}

/// Response DTO for chat details.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ChatDetailDto {
    pub id: Uuid,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: OffsetDateTime,
}

impl From<ChatView> for ChatDetailDto {
    fn from(v: ChatView) -> Self {
        Self {
            id: v.id,
            model: v.model,
            title: v.title,
            is_temporary: v.is_temporary,
            message_count: v.message_count,
            created_at: v.created_at,
            updated_at: v.updated_at,
        }
    }
}

/// Server-generated preview thumbnail for an image attachment.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ImgThumbnailDto {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data_base64: String,
}

impl From<ThumbnailView> for ImgThumbnailDto {
    fn from(t: ThumbnailView) -> Self {
        Self {
            content_type: t.content_type,
            width: t.width,
            height: t.height,
            data_base64: t.data_base64,
        }
    }
}

/// Lightweight attachment metadata embedded in Message responses.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentSummaryDto {
    pub attachment_id: Uuid,
    pub kind: String,
    pub filename: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

impl From<AttachmentSummaryView> for AttachmentSummaryDto {
    fn from(a: AttachmentSummaryView) -> Self {
        Self {
            attachment_id: a.attachment_id,
            kind: a.kind,
            filename: a.filename,
            status: a.status,
            img_thumbnail: a.img_thumbnail.map(Into::into),
        }
    }
}

/// Response DTO for a message in the list endpoint.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatMessageDto {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    pub attachments: Vec<AttachmentSummaryDto>,
    pub my_reaction: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<MessageView> for MiniChatMessageDto {
    fn from(m: MessageView) -> Self {
        Self {
            id: m.id,
            request_id: m.request_id,
            role: m.role,
            content: m.content,
            model: m.model,
            input_tokens: m.input_tokens,
            output_tokens: m.output_tokens,
            attachments: m.attachments.into_iter().map(Into::into).collect(),
            my_reaction: m.my_reaction,
            created_at: m.created_at,
        }
    }
}

/// Full attachment details.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentDetailDto {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub status: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
    #[serde(skip_serializing_if = "Option::is_none", with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub summary_updated_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<AttachmentView> for AttachmentDetailDto {
    fn from(a: AttachmentView) -> Self {
        Self {
            id: a.id,
            filename: a.filename,
            content_type: a.content_type,
            size_bytes: a.size_bytes,
            status: a.status,
            kind: a.kind,
            error_code: a.error_code,
            doc_summary: None,
            img_thumbnail: a.img_thumbnail.map(Into::into),
            summary_updated_at: None,
            created_at: a.created_at,
        }
    }
}

/// Web search toggle.
#[derive(Debug, Clone, Copy)]
#[toolkit_macros::api_dto(request)]
pub struct WebSearchConfig {
    pub enabled: bool,
}

/// Request body for `POST /v1/chats/{id}/messages:stream`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct StreamMessageRequest {
    pub content: String,
    #[serde(default)]
    pub request_id: Option<Uuid>,
    #[serde(default)]
    pub attachment_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    pub web_search: Option<WebSearchConfig>,
}

/// Request DTO for `PATCH /chats/{id}/turns/{request_id}` (edit).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct EditTurnRequest {
    pub content: String,
}

/// Request DTO for setting a reaction.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct SetReactionReq {
    pub reaction: String,
}

/// Response DTO for a reaction.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatReactionDto {
    pub message_id: Uuid,
    pub reaction: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<ReactionView> for MiniChatReactionDto {
    fn from(r: ReactionView) -> Self {
        Self {
            message_id: r.message_id,
            reaction: r.reaction,
            created_at: r.created_at,
        }
    }
}

/// Response DTO for `GET /chats/{id}/turns/{request_id}`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct TurnStatusResponse {
    pub request_id: Uuid,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: OffsetDateTime,
}

impl From<TurnStatusView> for TurnStatusResponse {
    fn from(t: TurnStatusView) -> Self {
        Self {
            request_id: t.request_id,
            state: t.state,
            error_code: t.error_code,
            assistant_message_id: t.assistant_message_id,
            updated_at: t.updated_at,
        }
    }
}

/// Response DTO for a single model.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ModelDto {
    pub model_id: String,
    pub display_name: String,
    pub tier: String,
    pub multiplier_display: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub multimodal_capabilities: Vec<String>,
    pub context_window: u32,
}

impl From<ModelView> for ModelDto {
    fn from(m: ModelView) -> Self {
        Self {
            model_id: m.model_id,
            display_name: m.display_name,
            tier: m.tier,
            multiplier_display: m.multiplier_display,
            description: m.description,
            multimodal_capabilities: m.multimodal_capabilities,
            context_window: m.context_window,
        }
    }
}

/// Response DTO for the model list endpoint.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ModelListDto {
    pub items: Vec<ModelDto>,
}

#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaPeriodStatus {
    pub period: String,
    pub limit_credits_micro: i64,
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u32,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatus {
    pub tier: String,
    pub periods: Vec<QuotaPeriodStatus>,
}

#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u32,
}

impl From<crate::domain::chat_service::QuotaStatusView> for QuotaStatusResponse {
    fn from(v: crate::domain::chat_service::QuotaStatusView) -> Self {
        let mut tiers: Vec<QuotaTierStatus> = Vec::new();
        for e in v.entries {
            let p = QuotaPeriodStatus {
                period: e.period.to_owned(),
                limit_credits_micro: e.limit,
                used_credits_micro: e.used,
                remaining_credits_micro: e.remaining,
                remaining_percentage: e.remaining_percentage,
                next_reset: e.next_reset,
                warning: e.warning,
                exhausted: e.exhausted,
            };
            match tiers.iter_mut().find(|t| t.tier == e.tier) {
                Some(t) => t.periods.push(p),
                None => tiers.push(QuotaTierStatus {
                    tier: e.tier.to_owned(),
                    periods: vec![p],
                }),
            }
        }
        Self {
            tiers,
            warning_threshold_pct: u32::from(v.warning_threshold_pct),
        }
    }
}

/// `OpenAPI` description of one SSE event of the `messages:stream`, retry and
/// edit responses (`event: <name>`, `data: <payload>`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatSseEvent {
    pub event: String,
    pub data: serde_json::Value,
}
