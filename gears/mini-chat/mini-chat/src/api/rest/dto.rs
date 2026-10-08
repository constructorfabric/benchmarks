//! REST DTOs (wire contract, see the gear `OpenAPI`).

use base64::Engine;
use mini_chat_sdk::{ModelCatalogEntry, ModelTier};
use crate::domain::clock::Timestamp;
use uuid::Uuid;

use crate::domain::billing::TurnState;
use crate::domain::chats::ChatView;
use crate::domain::messages::{AttachmentSummary, MessageView};
use crate::domain::models::QuotaStatus;
use crate::infra::storage::entity::{attachment, chat_turn, message_reaction};

/// Chat details.
#[toolkit_macros::api_dto(response)]
pub struct ChatDetailDto {
    pub id: Uuid,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: Timestamp,
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: Timestamp,
}

impl From<ChatView> for ChatDetailDto {
    fn from(v: ChatView) -> Self {
        Self {
            id: v.chat.id,
            model: v.chat.model,
            title: v.chat.title,
            is_temporary: v.chat.is_temporary,
            message_count: v.message_count,
            created_at: v.chat.created_at,
            updated_at: v.chat.updated_at,
        }
    }
}

/// `POST /v1/chats` body.
#[toolkit_macros::api_dto(request)]
#[derive(Default)]
pub struct CreateChatReq {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// `PATCH /v1/chats/{id}` body.
#[toolkit_macros::api_dto(request)]
pub struct UpdateChatReq {
    pub title: String,
}

/// Web search toggle.
#[toolkit_macros::api_dto(request)]
pub struct WebSearchConfig {
    pub enabled: bool,
}

/// `POST /v1/chats/{id}/messages:stream` body.
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

/// `PATCH /v1/chats/{id}/turns/{request_id}` body.
#[toolkit_macros::api_dto(request)]
pub struct EditTurnRequest {
    pub content: String,
}

/// `PUT .../reaction` body.
#[toolkit_macros::api_dto(request)]
pub struct SetReactionReq {
    pub reaction: String,
}

/// Image thumbnail.
#[toolkit_macros::api_dto(response)]
pub struct ImgThumbnailDto {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data_base64: String,
}

fn thumb(bytes: &[u8], w: i32, h: i32) -> ImgThumbnailDto {
    ImgThumbnailDto {
        content_type: "image/webp".to_owned(),
        width: w,
        height: h,
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    }
}

/// Attachment summary embedded in messages.
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
        let img = if a.kind == "image" && a.status == "ready" {
            a.thumbnail.as_ref().map(|(b, w, h)| thumb(b, *w, *h))
        } else {
            None
        };
        Self { attachment_id: a.attachment_id, kind: a.kind, filename: a.filename, status: a.status, img_thumbnail: img }
    }
}

/// A message.
#[toolkit_macros::api_dto(response)]
#[schema(as = MiniChatMessageDto)]
pub struct MiniChatMessageDto {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: String,
    pub content: String,
    pub attachments: Vec<AttachmentSummaryDto>,
    pub my_reaction: Option<String>,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: Timestamp,
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
            request_id: m.request_id.unwrap_or(m.id),
            role: m.role,
            content: m.content,
            attachments: v.attachments.into_iter().map(Into::into).collect(),
            my_reaction: v.my_reaction,
            created_at: m.created_at,
            model: m.model,
            input_tokens: (m.input_tokens != 0).then_some(m.input_tokens),
            output_tokens: (m.output_tokens != 0).then_some(m.output_tokens),
        }
    }
}

/// Reaction.
#[toolkit_macros::api_dto(response)]
pub struct MiniChatReactionDto {
    pub message_id: Uuid,
    pub reaction: String,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: Timestamp,
}

impl From<message_reaction::Model> for MiniChatReactionDto {
    fn from(r: message_reaction::Model) -> Self {
        Self { message_id: r.message_id, reaction: r.reaction, created_at: r.created_at }
    }
}

/// Attachment details.
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
    #[schema(value_type = Option<String>, format = DateTime)]
    pub summary_updated_at: Option<Timestamp>,
    #[schema(value_type = String, format = DateTime)]
    pub created_at: Timestamp,
}

impl From<attachment::Model> for AttachmentDetailDto {
    fn from(a: attachment::Model) -> Self {
        let img = match (&a.img_thumbnail, a.img_thumbnail_width, a.img_thumbnail_height) {
            (Some(b), Some(w), Some(h)) if a.attachment_kind == "image" && a.status == "ready" => Some(thumb(b, w, h)),
            _ => None,
        };
        let error_code = if a.status == "failed" { a.error_code } else { None };
        Self {
            id: a.id,
            filename: a.filename,
            content_type: a.content_type,
            size_bytes: a.size_bytes,
            status: a.status,
            kind: a.attachment_kind,
            error_code,
            doc_summary: None,
            img_thumbnail: img,
            summary_updated_at: None,
            created_at: a.created_at,
        }
    }
}

/// Turn status.
#[toolkit_macros::api_dto(response)]
pub struct TurnStatusResponse {
    pub request_id: Uuid,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: Timestamp,
}

impl From<chat_turn::Model> for TurnStatusResponse {
    fn from(t: chat_turn::Model) -> Self {
        let state = TurnState::parse(&t.state).unwrap_or(TurnState::Failed);
        Self {
            request_id: t.request_id,
            state: state.api_state().to_owned(),
            error_code: if state == TurnState::Failed { t.error_code } else { None },
            assistant_message_id: match state {
                TurnState::Completed | TurnState::Cancelled => t.assistant_message_id,
                _ => None,
            },
            updated_at: t.updated_at,
        }
    }
}

/// A model.
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
            model_id: m.id,
            display_name: m.display_name,
            tier: match m.tier {
                ModelTier::Premium => "premium",
                ModelTier::Standard => "standard",
            }
            .to_owned(),
            multiplier_display: m.multiplier_display,
            description: (!m.description.trim().is_empty()).then_some(m.description),
            multimodal_capabilities: m.multimodal_capabilities,
            context_window: m.context_window,
        }
    }
}

/// Model list.
#[toolkit_macros::api_dto(response)]
pub struct ModelListDto {
    pub items: Vec<ModelDto>,
}

/// One quota period.
#[toolkit_macros::api_dto(response)]
pub struct QuotaPeriodStatus {
    pub period: String,
    pub limit_credits_micro: i64,
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u32,
    pub next_reset: String,
    pub warning: bool,
    pub exhausted: bool,
}

/// One quota tier.
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatus {
    pub tier: String,
    pub periods: Vec<QuotaPeriodStatus>,
}

/// Quota status.
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u32,
}

impl From<QuotaStatus> for QuotaStatusResponse {
    fn from(s: QuotaStatus) -> Self {
        let mut tiers: Vec<QuotaTierStatus> = Vec::new();
        for p in s.periods {
            let entry = QuotaPeriodStatus {
                period: p.period.as_str().to_owned(),
                limit_credits_micro: p.limit,
                used_credits_micro: p.used,
                remaining_credits_micro: p.remaining,
                remaining_percentage: p.remaining_percentage,
                next_reset: crate::domain::clock::format_rfc3339(p.next_reset),
                warning: p.warning,
                exhausted: p.exhausted,
            };
            match tiers.iter_mut().find(|t| t.tier == p.tier) {
                Some(t) => t.periods.push(entry),
                None => tiers.push(QuotaTierStatus { tier: p.tier.to_owned(), periods: vec![entry] }),
            }
        }
        Self { tiers, warning_threshold_pct: u32::from(s.warning_threshold_pct) }
    }
}

/// One SSE event (documentation type: `event: <name>`, `data: <json>`).
#[toolkit_macros::api_dto(response)]
pub struct MiniChatSseEvent {
    pub event: String,
    pub data: serde_json::Value,
}
