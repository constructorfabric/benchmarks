//! REST DTOs (schemas of `docs/api/api.json`).

use base64::Engine;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::chat_service::ChatView;
use crate::domain::message_service::MessageView;
use crate::domain::quota::PeriodStatus;
use crate::domain::turn_service::TurnStatus;
use crate::infra::db::entities::{attachments, message_reactions};

fn rfc3339(ts: &OffsetDateTime) -> String {
    ts.format(&time::format_description::well_known::Rfc3339).unwrap_or_default()
}

/// Request DTO for creating a new chat.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone, Default)]
pub struct CreateChatReq {
    /// Model id.
    #[serde(default)]
    pub model: Option<String>,
    /// Title.
    #[serde(default)]
    pub title: Option<String>,
}

/// Request DTO for updating a chat title.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct UpdateChatReq {
    /// Title.
    pub title: String,
}

/// Response DTO for chat details.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ChatDetailDto {
    /// Chat id.
    pub id: Uuid,
    /// Selected model.
    pub model: String,
    /// Title (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Temporary flag.
    pub is_temporary: bool,
    /// Non-deleted messages.
    pub message_count: i64,
    /// Creation time.
    pub created_at: String,
    /// Last activity.
    pub updated_at: String,
}

impl From<ChatView> for ChatDetailDto {
    fn from(v: ChatView) -> Self {
        Self {
            id: v.chat.id,
            model: v.chat.model,
            title: v.chat.title,
            is_temporary: v.chat.is_temporary,
            message_count: v.message_count,
            created_at: rfc3339(&v.chat.created_at),
            updated_at: rfc3339(&v.chat.updated_at),
        }
    }
}

/// Server-generated preview thumbnail.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ImgThumbnailDto {
    /// `image/webp`.
    pub content_type: String,
    /// Width.
    pub width: i32,
    /// Height.
    pub height: i32,
    /// Base64 bytes.
    pub data_base64: String,
}

fn thumbnail(a: &attachments::Model) -> Option<ImgThumbnailDto> {
    if a.attachment_kind != "image" || a.status != "ready" {
        return None;
    }
    let data = a.img_thumbnail.as_ref()?;
    Some(ImgThumbnailDto {
        content_type: "image/webp".to_owned(),
        width: a.img_thumbnail_width.unwrap_or(0),
        height: a.img_thumbnail_height.unwrap_or(0),
        data_base64: base64::engine::general_purpose::STANDARD.encode(data),
    })
}

/// Attachment metadata embedded in messages.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct AttachmentSummaryDto {
    /// Attachment id.
    pub attachment_id: Uuid,
    /// `document` / `image`.
    pub kind: String,
    /// Filename.
    pub filename: String,
    /// Status.
    pub status: String,
    /// Thumbnail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

/// Message in the list endpoint.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct MiniChatMessageDto {
    /// Message id.
    pub id: Uuid,
    /// Turn correlation key.
    pub request_id: Uuid,
    /// `user` / `assistant` / `system`.
    pub role: String,
    /// Content.
    pub content: String,
    /// Linked non-deleted attachments.
    pub attachments: Vec<AttachmentSummaryDto>,
    /// Caller's reaction.
    pub my_reaction: Option<String>,
    /// Effective model (assistant).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Input tokens (omitted when 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    /// Output tokens (omitted when 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    /// Creation time.
    pub created_at: String,
}

impl MiniChatMessageDto {
    /// Converts a view; a null `request_id` is an internal error.
    ///
    /// # Errors
    /// Internal when `request_id` is null.
    pub fn try_from_view(v: MessageView) -> Result<Self, crate::domain::error::DomainError> {
        let m = v.message;
        let request_id = m
            .request_id
            .ok_or_else(|| crate::domain::error::DomainError::internal("message without request_id"))?;
        Ok(Self {
            id: m.id,
            request_id,
            role: m.role,
            content: m.content,
            attachments: v
                .attachments
                .iter()
                .map(|a| AttachmentSummaryDto {
                    attachment_id: a.id,
                    kind: a.attachment_kind.clone(),
                    filename: a.filename.clone(),
                    status: a.status.clone(),
                    img_thumbnail: thumbnail(a),
                })
                .collect(),
            my_reaction: v.my_reaction,
            model: m.model,
            input_tokens: (m.input_tokens != 0).then_some(m.input_tokens),
            output_tokens: (m.output_tokens != 0).then_some(m.output_tokens),
            created_at: rfc3339(&m.created_at),
        })
    }
}

/// Full attachment details.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct AttachmentDetailDto {
    /// Attachment id.
    pub id: Uuid,
    /// Filename.
    pub filename: String,
    /// MIME type.
    pub content_type: String,
    /// Size.
    pub size_bytes: i64,
    /// Status.
    pub status: String,
    /// Kind.
    pub kind: String,
    /// Failure code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Always null in P1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_summary: Option<String>,
    /// Thumbnail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
    /// Always null in P1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_updated_at: Option<String>,
    /// Upload time.
    pub created_at: String,
}

impl From<&attachments::Model> for AttachmentDetailDto {
    fn from(a: &attachments::Model) -> Self {
        Self {
            id: a.id,
            filename: a.filename.clone(),
            content_type: a.content_type.clone(),
            size_bytes: a.size_bytes,
            status: a.status.clone(),
            kind: a.attachment_kind.clone(),
            error_code: if a.status == "failed" { a.error_code.clone() } else { None },
            doc_summary: None,
            img_thumbnail: thumbnail(a),
            summary_updated_at: None,
            created_at: rfc3339(&a.created_at),
        }
    }
}

/// Request for `messages:stream`.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct StreamMessageRequest {
    /// Content.
    pub content: String,
    /// Idempotency key.
    #[serde(default)]
    pub request_id: Option<Uuid>,
    /// Attachments.
    #[serde(default)]
    pub attachment_ids: Option<Vec<Uuid>>,
    /// Web search toggle.
    #[serde(default)]
    pub web_search: Option<WebSearchConfig>,
}

/// Web search toggle.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone, Copy)]
pub struct WebSearchConfig {
    /// Enabled.
    pub enabled: bool,
}

/// Edit request.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct EditTurnRequest {
    /// New content.
    pub content: String,
}

/// Turn status.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct TurnStatusResponse {
    /// Request id.
    pub request_id: Uuid,
    /// `running|done|error|cancelled`.
    pub state: String,
    /// Error code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Assistant message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    /// Update time.
    pub updated_at: String,
}

impl From<TurnStatus> for TurnStatusResponse {
    fn from(t: TurnStatus) -> Self {
        Self {
            request_id: t.request_id,
            state: t.state.to_owned(),
            error_code: t.error_code,
            assistant_message_id: t.assistant_message_id,
            updated_at: rfc3339(&t.updated_at),
        }
    }
}

/// Reaction request.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct SetReactionReq {
    /// `like` / `dislike`.
    pub reaction: String,
}

/// Reaction response.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct MiniChatReactionDto {
    /// Message.
    pub message_id: Uuid,
    /// Reaction.
    pub reaction: String,
    /// Time.
    pub created_at: String,
}

impl From<message_reactions::Model> for MiniChatReactionDto {
    fn from(r: message_reactions::Model) -> Self {
        Self { message_id: r.message_id, reaction: r.reaction, created_at: rfc3339(&r.created_at) }
    }
}

/// Model.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ModelDto {
    /// Model id.
    pub model_id: String,
    /// Name.
    pub display_name: String,
    /// `standard` / `premium`.
    pub tier: String,
    /// Multiplier.
    pub multiplier_display: String,
    /// Description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Capabilities.
    pub multimodal_capabilities: Vec<String>,
    /// Context window.
    pub context_window: u32,
}

impl From<&mini_chat_sdk::ModelCatalogEntry> for ModelDto {
    fn from(m: &mini_chat_sdk::ModelCatalogEntry) -> Self {
        Self {
            model_id: m.id.clone(),
            display_name: m.display_name.clone(),
            tier: m.tier.as_str().to_owned(),
            multiplier_display: m.multiplier_display.clone(),
            description: (!m.description.is_empty()).then(|| m.description.clone()),
            multimodal_capabilities: m.multimodal_capabilities.clone(),
            context_window: m.context_window,
        }
    }
}

/// Model list.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ModelListDto {
    /// Items.
    pub items: Vec<ModelDto>,
}

/// Period status.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaPeriodStatus {
    /// `daily` / `monthly`.
    pub period: String,
    /// Limit.
    pub limit_credits_micro: i64,
    /// Spent + reserved.
    pub used_credits_micro: i64,
    /// Remaining.
    pub remaining_credits_micro: i64,
    /// Remaining percentage.
    pub remaining_percentage: u32,
    /// Next reset.
    pub next_reset: String,
    /// Warning.
    pub warning: bool,
    /// Exhausted.
    pub exhausted: bool,
}

/// Tier status.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaTierStatus {
    /// `premium` / `total`.
    pub tier: String,
    /// Periods.
    pub periods: Vec<QuotaPeriodStatus>,
}

/// Quota status.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaStatusResponse {
    /// Tiers.
    pub tiers: Vec<QuotaTierStatus>,
    /// Threshold.
    pub warning_threshold_pct: u32,
}

impl QuotaStatusResponse {
    /// Builds the response from status entries.
    #[must_use]
    pub fn from_entries(entries: &[PeriodStatus], threshold: u8) -> Self {
        let mut tiers: Vec<QuotaTierStatus> = Vec::new();
        for e in entries {
            let p = QuotaPeriodStatus {
                period: e.period.as_str().to_owned(),
                limit_credits_micro: e.limit,
                used_credits_micro: e.used,
                remaining_credits_micro: e.remaining,
                remaining_percentage: e.remaining_percentage,
                next_reset: rfc3339(&e.next_reset),
                warning: e.warning,
                exhausted: e.exhausted,
            };
            if let Some(t) = tiers.iter_mut().find(|t| t.tier == e.tier) {
                t.periods.push(p);
            } else {
                tiers.push(QuotaTierStatus { tier: e.tier.to_owned(), periods: vec![p] });
            }
        }
        Self { tiers, warning_threshold_pct: u32::from(threshold) }
    }
}

/// `OpenAPI` description of one SSE event (`event: <name>`, `data: <json>`).
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct MiniChatSseEvent {
    /// Event name.
    pub event: String,
    /// JSON payload.
    pub data: serde_json::Value,
}
