//! REST and SSE DTOs (see `docs/api/api.json`).

use base64::Engine as _;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::chats::{AttachmentSummary, ChatView, MessageView, Thumbnail, thumbnail_of};
use crate::domain::quota::PeriodStatus;
use crate::domain::stream::{CitationOut, DoneOut, StreamEvent, StreamHeader};
use crate::infra::db::entity::{attachments, chat_turns, message_reactions};

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
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
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
            content_type: "image/webp".into(),
            width: t.width,
            height: t.height,
            data_base64: base64::engine::general_purpose::STANDARD.encode(t.data),
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
            img_thumbnail: a.thumbnail.map(Into::into),
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
    pub attachments: Vec<AttachmentSummaryDto>,
    pub my_reaction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<MessageView> for MiniChatMessageDto {
    fn from(v: MessageView) -> Self {
        let m = v.message;
        Self {
            id: m.id,
            request_id: v.request_id,
            role: m.role,
            content: m.content,
            attachments: v.attachments.into_iter().map(Into::into).collect(),
            my_reaction: v.my_reaction,
            model: m.model,
            input_tokens: (m.input_tokens != 0).then_some(m.input_tokens),
            output_tokens: (m.output_tokens != 0).then_some(m.output_tokens),
            created_at: m.created_at,
        }
    }
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
    pub created_at: OffsetDateTime,
}

impl From<message_reactions::Model> for MiniChatReactionDto {
    fn from(r: message_reactions::Model) -> Self {
        Self {
            message_id: r.message_id,
            reaction: r.reaction,
            created_at: r.created_at,
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
    pub summary_updated_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<attachments::Model> for AttachmentDetailDto {
    fn from(a: attachments::Model) -> Self {
        Self {
            img_thumbnail: thumbnail_of(&a).map(Into::into),
            error_code: if a.status == "failed" { a.error_code } else { None },
            id: a.id,
            filename: a.filename,
            content_type: a.content_type,
            size_bytes: a.size_bytes,
            status: a.status,
            kind: a.attachment_kind,
            doc_summary: None,
            summary_updated_at: None,
            created_at: a.created_at,
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
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct EditTurnRequest {
    pub content: String,
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
    pub updated_at: OffsetDateTime,
}

impl From<chat_turns::Model> for TurnStatusResponse {
    fn from(t: chat_turns::Model) -> Self {
        let state = match t.state.as_str() {
            "completed" => "done",
            "failed" => "error",
            other => other,
        }
        .to_owned();
        Self {
            error_code: if t.state == "failed" { t.error_code } else { None },
            assistant_message_id: if t.state == "completed" || t.state == "cancelled" {
                t.assistant_message_id
            } else {
                None
            },
            request_id: t.request_id,
            state,
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

impl From<mini_chat_sdk::ModelCatalogEntry> for ModelDto {
    fn from(m: mini_chat_sdk::ModelCatalogEntry) -> Self {
        Self {
            model_id: m.id,
            display_name: m.display_name,
            tier: m.tier.as_str().into(),
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

/// One per-period quota status entry.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaPeriodStatus {
    pub period: String,
    pub limit_credits_micro: i64,
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// Quota status of one tier.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatus {
    pub tier: String,
    pub periods: Vec<QuotaPeriodStatus>,
}

/// Response of `GET /v1/quota/status`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u8,
}

impl QuotaStatusResponse {
    #[must_use]
    pub fn from_status(list: Vec<PeriodStatus>, warning_threshold_pct: u8) -> Self {
        let mut tiers: Vec<QuotaTierStatus> = Vec::new();
        for p in list {
            let entry = QuotaPeriodStatus {
                period: p.period.into(),
                limit_credits_micro: p.limit,
                used_credits_micro: p.used,
                remaining_credits_micro: p.remaining,
                remaining_percentage: p.remaining_percentage,
                next_reset: p.next_reset,
                warning: p.warning,
                exhausted: p.exhausted,
            };
            if let Some(t) = tiers.iter_mut().find(|t| t.tier == p.tier) {
                t.periods.push(entry);
            } else {
                tiers.push(QuotaTierStatus { tier: p.tier.into(), periods: vec![entry] });
            }
        }
        Self { tiers, warning_threshold_pct }
    }
}

// ── SSE payloads ────────────────────────────────────────────────────────────

/// Thread summary metadata of `stream_started`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ThreadSummaryInfo {
    pub token_estimate: i64,
}

/// `stream_started` payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

impl From<&StreamHeader> for StreamStartedData {
    fn from(h: &StreamHeader) -> Self {
        Self {
            request_id: h.request_id,
            message_id: h.message_id,
            is_new_turn: h.is_new_turn,
            thread_summary_applied: h.summary_token_estimate.map(|t| ThreadSummaryInfo { token_estimate: t }),
        }
    }
}

/// Token usage counters.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Quota warning entry of `done`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaWarning {
    pub tier: String,
    pub period: String,
    pub remaining_percentage: i64,
    pub warning: bool,
    pub exhausted: bool,
    #[serde(skip_serializing_if = "Option::is_none", with = "time::serde::rfc3339::option")]
    pub next_reset: Option<OffsetDateTime>,
}

/// `done` payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct DoneData {
    pub usage: Usage,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

impl From<DoneOut> for DoneData {
    fn from(d: DoneOut) -> Self {
        Self {
            usage: Usage { input_tokens: d.input_tokens, output_tokens: d.output_tokens },
            quota_decision: if d.downgrade_from.is_some() { "downgrade" } else { "allow" }.into(),
            effective_model: d.effective_model,
            selected_model: d.selected_model,
            downgrade_from: d.downgrade_from,
            downgrade_reason: d.downgrade_reason,
            quota_warnings: d.quota_warnings.map(|w| {
                w.into_iter()
                    .map(|w| QuotaWarning {
                        tier: w.tier.into(),
                        period: w.period.into(),
                        remaining_percentage: w.remaining_percentage,
                        warning: w.warning,
                        exhausted: w.exhausted,
                        next_reset: w.next_reset,
                    })
                    .collect()
            }),
        }
    }
}

/// A character span within response text.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct TextSpan {
    pub start: usize,
    pub end: usize,
}

/// A citation.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct Citation {
    pub source: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TextSpan>,
}

impl From<CitationOut> for Citation {
    fn from(c: CitationOut) -> Self {
        Self {
            source: c.source.into(),
            title: c.title,
            url: c.url,
            attachment_id: c.attachment_id,
            snippet: c.snippet,
            span: c.span.map(|(start, end)| TextSpan { start, end }),
        }
    }
}

/// `citations` payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CitationsData {
    pub items: Vec<Citation>,
}

/// `delta` payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct DeltaData {
    #[serde(rename = "type")]
    pub kind: String,
    pub content: String,
}

/// `tool` payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ToolData {
    pub phase: String,
    pub name: String,
    pub details: serde_json::Value,
}

/// `error` payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
}

/// OpenAPI description of one SSE event (`event` name + `data` payload).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatSseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

/// Serializes a stream event into `(event name, JSON data)`.
#[must_use]
pub fn sse_parts(ev: StreamEvent) -> (&'static str, String) {
    let to = |v: serde_json::Result<String>| v.unwrap_or_else(|_| "{}".to_owned());
    match ev {
        StreamEvent::Delta { kind, content } => ("delta", to(serde_json::to_string(&DeltaData { kind: kind.into(), content }))),
        StreamEvent::Tool { phase, name, details } => (
            "tool",
            to(serde_json::to_string(&ToolData { phase: phase.into(), name, details })),
        ),
        StreamEvent::Citations(items) => (
            "citations",
            to(serde_json::to_string(&CitationsData { items: items.into_iter().map(Into::into).collect() })),
        ),
        StreamEvent::Done(d) => ("done", to(serde_json::to_string(&DoneData::from(d)))),
        StreamEvent::Error { code, message } => ("error", to(serde_json::to_string(&ErrorData { code, message }))),
    }
}
