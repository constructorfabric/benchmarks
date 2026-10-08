//! REST DTOs (wire format per `docs/api/api.json`).

use mini_chat_sdk::ModelCatalogEntry;
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::attachments::AttachmentDetail;
use crate::domain::chats::ChatView;
use crate::domain::messages::{AttachmentSummary, MessageView, ReactionView, Thumbnail};
use crate::domain::quota::PeriodStatus;
use crate::domain::turns::TurnStatusView;

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// Request DTO for creating a new chat.
#[derive(Debug, Clone, Default)]
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
    fn from(c: ChatView) -> Self {
        Self {
            id: c.id,
            model: c.model,
            title: c.title,
            is_temporary: c.is_temporary,
            message_count: c.message_count,
            created_at: c.created_at,
            updated_at: c.updated_at,
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

impl From<Thumbnail> for ImgThumbnailDto {
    fn from(t: Thumbnail) -> Self {
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

impl From<AttachmentSummary> for AttachmentSummaryDto {
    fn from(a: AttachmentSummary) -> Self {
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
#[schema(as = MiniChatMessageDto)]
pub struct MessageDto {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: String,
    pub content: String,
    pub attachments: Vec<AttachmentSummaryDto>,
    pub my_reaction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    pub input_tokens: i64,
    #[serde(skip_serializing_if = "is_zero")]
    pub output_tokens: i64,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<MessageView> for MessageDto {
    fn from(m: MessageView) -> Self {
        Self {
            id: m.id,
            request_id: m.request_id,
            role: m.role,
            content: m.content,
            attachments: m.attachments.into_iter().map(Into::into).collect(),
            my_reaction: m.my_reaction,
            model: m.model,
            input_tokens: m.input_tokens,
            output_tokens: m.output_tokens,
            created_at: m.created_at,
        }
    }
}

/// Web search toggle.
#[derive(Debug, Clone, Default)]
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
#[schema(as = MiniChatReactionDto)]
pub struct ReactionDto {
    pub message_id: Uuid,
    pub reaction: String,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<ReactionView> for ReactionDto {
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
            state: t.state.to_owned(),
            error_code: t.error_code,
            assistant_message_id: t.assistant_message_id,
            updated_at: t.updated_at,
        }
    }
}

/// Full attachment details returned by the GET attachment endpoint.
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_updated_at: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<AttachmentDetail> for AttachmentDetailDto {
    fn from(a: AttachmentDetail) -> Self {
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

impl From<ModelCatalogEntry> for ModelDto {
    fn from(m: ModelCatalogEntry) -> Self {
        Self {
            display_name: if m.display_name.is_empty() { m.id.clone() } else { m.display_name },
            model_id: m.id,
            tier: m.tier.as_str().to_owned(),
            multiplier_display: m.multiplier_display,
            description: (!m.description.is_empty()).then_some(m.description),
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
    pub remaining_percentage: i64,
    pub next_reset: String,
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
    pub warning_threshold_pct: u8,
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
}

impl QuotaStatusResponse {
    #[must_use]
    pub fn build(statuses: &[PeriodStatus], threshold: u8) -> Self {
        let mut tiers: Vec<QuotaTierStatus> = Vec::new();
        for s in statuses {
            let p = QuotaPeriodStatus {
                period: s.period.to_owned(),
                limit_credits_micro: s.limit,
                used_credits_micro: s.used,
                remaining_credits_micro: s.remaining,
                remaining_percentage: s.remaining_percentage,
                next_reset: rfc3339(s.next_reset),
                warning: s.warning,
                exhausted: s.exhausted,
            };
            if let Some(t) = tiers.iter_mut().find(|t| t.tier == s.tier) {
                t.periods.push(p);
            } else {
                tiers.push(QuotaTierStatus {
                    tier: s.tier.to_owned(),
                    periods: vec![p],
                });
            }
        }
        Self {
            tiers,
            warning_threshold_pct: threshold,
        }
    }
}

/// OpenAPI description of one SSE event (`event` name and `data` payload).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatSseEvent {
    pub event: String,
    #[schema(value_type = Object)]
    pub data: Value,
}
