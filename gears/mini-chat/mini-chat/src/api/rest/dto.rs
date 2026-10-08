//! REST DTOs (shapes of `docs/api/api.json`).

use base64::Engine;
use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::domain::service::attachments::AttachmentView;
use crate::domain::service::chats::ChatView;
use crate::domain::service::messages::{AttachmentSummaryView, MessageView, ReactionView, ThumbnailView};
use crate::domain::service::models::ModelView;
use crate::domain::service::quota::PeriodStatus;
use crate::domain::service::stream::{CitationView, DoneView, WarningView};
use crate::domain::service::turns::TurnStatusView;

/// Request DTO for creating a new chat.
#[toolkit_macros::api_dto(request)]
pub struct CreateChatReq {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// Request DTO for updating a chat title.
#[toolkit_macros::api_dto(request)]
pub struct UpdateChatReq {
    pub title: String,
}

/// Response DTO for chat details.
#[toolkit_macros::api_dto(response)]
pub struct ChatDetailDto {
    pub id: Uuid,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
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
            data_base64: base64::engine::general_purpose::STANDARD.encode(t.data),
        }
    }
}

/// Lightweight attachment metadata embedded in Message responses.
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
#[toolkit_macros::api_dto(response)]
pub struct MiniChatMessageDto {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: String,
    pub content: String,
    pub attachments: Vec<AttachmentSummaryDto>,
    /// The caller's reaction to this message; `null` when there is none.
    pub my_reaction: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
}

impl From<MessageView> for MiniChatMessageDto {
    fn from(m: MessageView) -> Self {
        Self {
            id: m.id,
            request_id: m.request_id,
            role: m.role,
            content: m.content,
            attachments: m.attachments.into_iter().map(Into::into).collect(),
            my_reaction: m.my_reaction,
            created_at: m.created_at,
            model: m.model,
            input_tokens: m.input_tokens,
            output_tokens: m.output_tokens,
        }
    }
}

/// Full attachment details returned by the GET attachment endpoint.
#[toolkit_macros::api_dto(response)]
pub struct AttachmentDetailDto {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub status: String,
    pub kind: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_updated_at: Option<DateTime<Utc>>,
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
            created_at: a.created_at,
            error_code: a.error_code,
            doc_summary: None,
            img_thumbnail: a.img_thumbnail.map(Into::into),
            summary_updated_at: None,
        }
    }
}

/// Request DTO for setting a reaction.
#[toolkit_macros::api_dto(request)]
pub struct SetReactionReq {
    pub reaction: String,
}

/// Response DTO for a reaction.
#[toolkit_macros::api_dto(response)]
pub struct MiniChatReactionDto {
    pub message_id: Uuid,
    pub reaction: String,
    pub created_at: DateTime<Utc>,
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

/// Web search toggle.
#[toolkit_macros::api_dto(request)]
pub struct WebSearchConfig {
    pub enabled: bool,
}

/// Request body for `POST /v1/chats/{id}/messages:stream`.
#[toolkit_macros::api_dto(request)]
pub struct StreamMessageRequest {
    /// Message content (must be non-empty).
    pub content: String,
    /// Idempotency key: any UUID; generated by the server when omitted.
    #[serde(default)]
    pub request_id: Option<Uuid>,
    /// Attachment IDs to include.
    #[serde(default)]
    pub attachment_ids: Option<Vec<Uuid>>,
    /// Web search configuration.
    #[serde(default)]
    pub web_search: Option<WebSearchConfig>,
}

/// Request DTO for `PATCH /chats/{id}/turns/{request_id}` (edit).
#[toolkit_macros::api_dto(request)]
pub struct EditTurnRequest {
    pub content: String,
}

/// Response DTO for `GET /chats/{id}/turns/{request_id}`.
#[toolkit_macros::api_dto(response)]
pub struct TurnStatusResponse {
    pub request_id: Uuid,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: DateTime<Utc>,
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

/// Response DTO for a single model.
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
            tier: m.tier.as_str().to_owned(),
            multiplier_display: m.multiplier_display,
            description: m.description,
            multimodal_capabilities: m.multimodal_capabilities,
            context_window: m.context_window,
        }
    }
}

/// Response DTO for the model list endpoint.
#[toolkit_macros::api_dto(response)]
pub struct ModelListDto {
    pub items: Vec<ModelDto>,
}

#[toolkit_macros::api_dto(response)]
pub struct QuotaPeriodStatus {
    pub period: String,
    pub limit_credits_micro: i64,
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u8,
    pub next_reset: DateTime<Utc>,
    pub warning: bool,
    pub exhausted: bool,
}

#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatus {
    pub tier: String,
    pub periods: Vec<QuotaPeriodStatus>,
}

#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u8,
}

impl QuotaStatusResponse {
    #[must_use]
    pub fn from_status(entries: Vec<PeriodStatus>, warning_threshold_pct: u8) -> Self {
        let mut tiers: Vec<QuotaTierStatus> = Vec::new();
        for e in entries {
            let p = QuotaPeriodStatus {
                period: e.period.as_str().to_owned(),
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
            warning_threshold_pct,
        }
    }
}

// ── SSE payloads ─────────────────────────────────────────────────────────

/// Metadata about a thread summary applied to the current turn's context.
#[derive(Debug, Clone, Serialize)]
pub struct ThreadSummaryInfo {
    pub token_estimate: i64,
}

/// `stream_started` payload.
#[derive(Debug, Clone, Serialize)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

/// `delta` payload.
#[derive(Debug, Clone, Serialize)]
pub struct DeltaData {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub content: String,
}

/// `tool` payload.
#[derive(Debug, Clone, Serialize)]
pub struct ToolData {
    pub phase: &'static str,
    pub name: String,
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextSpan {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Citation {
    pub source: &'static str,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TextSpan>,
}

impl From<CitationView> for Citation {
    fn from(c: CitationView) -> Self {
        Self {
            source: c.source,
            title: c.title,
            url: c.url,
            attachment_id: c.attachment_id,
            snippet: c.snippet,
            span: c.span.map(|(start, end)| TextSpan { start, end }),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CitationsData {
    pub items: Vec<Citation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaWarning {
    pub tier: &'static str,
    pub period: &'static str,
    pub remaining_percentage: u8,
    pub warning: bool,
    pub exhausted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_reset: Option<DateTime<Utc>>,
}

impl From<WarningView> for QuotaWarning {
    fn from(w: WarningView) -> Self {
        Self {
            tier: w.tier,
            period: w.period,
            remaining_percentage: w.remaining_percentage,
            warning: w.warning,
            exhausted: w.exhausted,
            next_reset: w.next_reset,
        }
    }
}

/// `done` payload.
#[derive(Debug, Clone, Serialize)]
pub struct DoneData {
    pub usage: Usage,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

impl From<DoneView> for DoneData {
    fn from(d: DoneView) -> Self {
        Self {
            usage: Usage {
                input_tokens: d.input_tokens,
                output_tokens: d.output_tokens,
            },
            effective_model: d.effective_model,
            selected_model: d.selected_model,
            quota_decision: d.quota_decision,
            downgrade_from: d.downgrade_from,
            downgrade_reason: d.downgrade_reason,
            quota_warnings: d.quota_warnings.map(|w| w.into_iter().map(Into::into).collect()),
        }
    }
}

/// `error` payload.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
}

/// `OpenAPI` description of one SSE event of the `messages:stream`, retry and
/// edit responses (`event: <name>`, `data: <JSON payload>`).
#[toolkit_macros::api_dto(response)]
pub struct MiniChatSseEvent {
    /// SSE event name: `stream_started`, `ping`, `delta`, `tool`, `citations`, `done`, `error`.
    pub event: String,
    /// Event payload.
    pub data: serde_json::Value,
}
