//! REST / SSE DTOs (wire contract, see `docs/api/api.json`).

use base64::Engine as _;
use mini_chat_sdk::{ModelCatalogEntry, ModelTier};
use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::quota::TierStatus;
use crate::infra::db::entity::{attachment, chat_turn};
use crate::service::chats::ChatView;
use crate::service::messages::MessageView;
use crate::service::reactions::ReactionView;
use crate::service::stream::{CitationOut, DoneOut, QuotaWarningOut, StreamEvent};

// ---------------------------------------------------------------------------
// Chats
// ---------------------------------------------------------------------------

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
            message_count: i64::try_from(v.message_count).unwrap_or(i64::MAX),
            created_at: v.chat.created_at,
            updated_at: v.chat.updated_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

/// Attachment kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum AttachmentKindDto {
    Document,
    Image,
}

/// Attachment lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum AttachmentStatusDto {
    Pending,
    Uploaded,
    Ready,
    Failed,
}

fn kind_of(a: &attachment::Model) -> AttachmentKindDto {
    if a.attachment_kind == "image" {
        AttachmentKindDto::Image
    } else {
        AttachmentKindDto::Document
    }
}

fn status_of(a: &attachment::Model) -> AttachmentStatusDto {
    match a.status.as_str() {
        "pending" => AttachmentStatusDto::Pending,
        "uploaded" => AttachmentStatusDto::Uploaded,
        "ready" => AttachmentStatusDto::Ready,
        _ => AttachmentStatusDto::Failed,
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

fn thumbnail_of(a: &attachment::Model) -> Option<ImgThumbnailDto> {
    if a.attachment_kind != "image" || a.status != "ready" {
        return None;
    }
    let bytes = a.img_thumbnail.as_ref()?;
    Some(ImgThumbnailDto {
        content_type: "image/webp".to_owned(),
        width: a.img_thumbnail_width.unwrap_or(0),
        height: a.img_thumbnail_height.unwrap_or(0),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

/// Full attachment details returned by the GET attachment endpoint.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
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
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub summary_updated_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<&attachment::Model> for AttachmentDetailDto {
    fn from(a: &attachment::Model) -> Self {
        Self {
            id: a.id,
            filename: a.filename.clone(),
            content_type: a.content_type.clone(),
            size_bytes: a.size_bytes,
            status: status_of(a),
            kind: kind_of(a),
            error_code: if a.status == "failed" {
                a.error_code.clone()
            } else {
                None
            },
            doc_summary: None,
            img_thumbnail: thumbnail_of(a),
            summary_updated_at: None,
            created_at: a.created_at,
        }
    }
}

/// Lightweight attachment metadata embedded in Message responses.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentSummaryDto {
    pub attachment_id: Uuid,
    pub kind: AttachmentKindDto,
    pub filename: String,
    pub status: AttachmentStatusDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

impl From<&attachment::Model> for AttachmentSummaryDto {
    fn from(a: &attachment::Model) -> Self {
        Self {
            attachment_id: a.id,
            kind: kind_of(a),
            filename: a.filename.clone(),
            status: status_of(a),
            img_thumbnail: thumbnail_of(a),
        }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Message author role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum MessageRoleDto {
    User,
    Assistant,
    System,
}

/// Reaction value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ReactionKindDto {
    Like,
    Dislike,
}

impl ReactionKindDto {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "like" => Some(Self::Like),
            "dislike" => Some(Self::Dislike),
            _ => None,
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
    pub role: MessageRoleDto,
    pub content: String,
    pub attachments: Vec<AttachmentSummaryDto>,
    pub my_reaction: Option<ReactionKindDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<MessageView> for MessageDto {
    fn from(v: MessageView) -> Self {
        let m = v.message;
        let role = match m.role.as_str() {
            "assistant" => MessageRoleDto::Assistant,
            "system" => MessageRoleDto::System,
            _ => MessageRoleDto::User,
        };
        Self {
            id: m.id,
            request_id: v.request_id,
            role,
            content: m.content,
            attachments: v
                .attachments
                .iter()
                .map(AttachmentSummaryDto::from)
                .collect(),
            my_reaction: v.my_reaction.as_deref().and_then(ReactionKindDto::parse),
            model: m.model,
            input_tokens: (m.input_tokens != 0).then_some(m.input_tokens),
            output_tokens: (m.output_tokens != 0).then_some(m.output_tokens),
            created_at: m.created_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Reactions
// ---------------------------------------------------------------------------

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
    pub reaction: ReactionKindDto,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<ReactionView> for ReactionDto {
    fn from(v: ReactionView) -> Self {
        Self {
            message_id: v.message_id,
            reaction: ReactionKindDto::parse(&v.reaction).unwrap_or(ReactionKindDto::Like),
            created_at: v.created_at,
        }
    }
}

// ---------------------------------------------------------------------------
// Turns
// ---------------------------------------------------------------------------

/// Turn state as reported by the turn status endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum TurnStatusState {
    Running,
    Done,
    Error,
    Cancelled,
}

/// Response DTO for `GET /chats/{id}/turns/{request_id}`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct TurnStatusResponse {
    pub request_id: Uuid,
    pub state: TurnStatusState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<&chat_turn::Model> for TurnStatusResponse {
    fn from(t: &chat_turn::Model) -> Self {
        let state = match t.state.as_str() {
            "running" => TurnStatusState::Running,
            "completed" => TurnStatusState::Done,
            "cancelled" => TurnStatusState::Cancelled,
            _ => TurnStatusState::Error,
        };
        Self {
            request_id: t.request_id,
            state,
            error_code: (state == TurnStatusState::Error)
                .then(|| t.error_code.clone())
                .flatten(),
            assistant_message_id: match state {
                TurnStatusState::Done | TurnStatusState::Cancelled => t.assistant_message_id,
                _ => None,
            },
            updated_at: t.updated_at,
        }
    }
}

/// Web search toggle.
#[derive(Debug, Clone, Copy, Default)]
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
    pub attachment_ids: Vec<Uuid>,
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

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

/// Model tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ModelTierDto {
    Standard,
    Premium,
}

/// Response DTO for a single model.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ModelDto {
    pub model_id: String,
    pub display_name: String,
    pub tier: ModelTierDto,
    pub multiplier_display: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub multimodal_capabilities: Vec<String>,
    pub context_window: u32,
}

impl From<&ModelCatalogEntry> for ModelDto {
    fn from(m: &ModelCatalogEntry) -> Self {
        Self {
            model_id: m.id.clone(),
            display_name: m.display_name.clone(),
            tier: match m.tier {
                ModelTier::Premium => ModelTierDto::Premium,
                ModelTier::Standard => ModelTierDto::Standard,
            },
            multiplier_display: m.multiplier_display.clone(),
            description: (!m.description.is_empty()).then(|| m.description.clone()),
            multimodal_capabilities: m.multimodal_capabilities.clone(),
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

// ---------------------------------------------------------------------------
// Quota status
// ---------------------------------------------------------------------------

/// Quota tier classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaTier {
    Premium,
    Total,
}

/// Quota period classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaPeriod {
    Daily,
    Monthly,
}

fn tier_dto(t: &str) -> QuotaTier {
    if t == "premium" {
        QuotaTier::Premium
    } else {
        QuotaTier::Total
    }
}

fn period_dto(p: &str) -> QuotaPeriod {
    if p == "monthly" {
        QuotaPeriod::Monthly
    } else {
        QuotaPeriod::Daily
    }
}

#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaPeriodStatus {
    pub period: QuotaPeriod,
    pub limit_credits_micro: i64,
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatus {
    pub tier: QuotaTier,
    pub periods: Vec<QuotaPeriodStatus>,
}

#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u32,
}

impl QuotaStatusResponse {
    #[must_use]
    pub fn new(tiers: Vec<TierStatus>, warning_threshold_pct: u8) -> Self {
        Self {
            tiers: tiers
                .into_iter()
                .map(|t| QuotaTierStatus {
                    tier: tier_dto(t.tier),
                    periods: t
                        .periods
                        .into_iter()
                        .map(|p| QuotaPeriodStatus {
                            period: period_dto(p.period.as_str()),
                            limit_credits_micro: p.limit,
                            used_credits_micro: p.used,
                            remaining_credits_micro: p.remaining,
                            remaining_percentage: p.remaining_percentage,
                            next_reset: p.next_reset,
                            warning: p.warning,
                            exhausted: p.exhausted,
                        })
                        .collect(),
                })
                .collect(),
            warning_threshold_pct: u32::from(warning_threshold_pct),
        }
    }
}

// ---------------------------------------------------------------------------
// SSE payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ThreadSummaryInfo {
    pub token_estimate: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeltaData {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub content: String,
}

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
pub struct CitationDto {
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

impl From<CitationOut> for CitationDto {
    fn from(c: CitationOut) -> Self {
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
    pub items: Vec<CitationDto>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageDto {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaWarningDto {
    pub tier: &'static str,
    pub period: &'static str,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub next_reset: Option<OffsetDateTime>,
}

impl From<QuotaWarningOut> for QuotaWarningDto {
    fn from(w: QuotaWarningOut) -> Self {
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

#[derive(Debug, Clone, Serialize)]
pub struct DoneData {
    pub usage: UsageDto,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarningDto>>,
}

impl From<DoneOut> for DoneData {
    fn from(d: DoneOut) -> Self {
        Self {
            usage: UsageDto {
                input_tokens: d.input_tokens,
                output_tokens: d.output_tokens,
            },
            effective_model: d.effective_model,
            selected_model: d.selected_model,
            quota_decision: if d.downgrade { "downgrade" } else { "allow" },
            downgrade_from: d.downgrade_from,
            downgrade_reason: d.downgrade_reason,
            quota_warnings: d
                .quota_warnings
                .map(|ws| ws.into_iter().map(QuotaWarningDto::from).collect()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
}

/// SSE event name and JSON payload of a stream event.
#[must_use]
pub fn sse_parts(ev: StreamEvent) -> (&'static str, serde_json::Value) {
    let to = |v: serde_json::Result<serde_json::Value>| v.unwrap_or(serde_json::Value::Null);
    match ev {
        StreamEvent::StreamStarted {
            request_id,
            message_id,
            is_new_turn,
            thread_summary_applied,
        } => (
            "stream_started",
            to(serde_json::to_value(StreamStartedData {
                request_id,
                message_id,
                is_new_turn,
                thread_summary_applied: thread_summary_applied
                    .map(|t| ThreadSummaryInfo { token_estimate: t }),
            })),
        ),
        StreamEvent::Delta { kind, content } => (
            "delta",
            to(serde_json::to_value(DeltaData { kind, content })),
        ),
        StreamEvent::Tool {
            phase,
            name,
            details,
        } => (
            "tool",
            to(serde_json::to_value(ToolData {
                phase,
                name,
                details,
            })),
        ),
        StreamEvent::Citations(items) => (
            "citations",
            to(serde_json::to_value(CitationsData {
                items: items.into_iter().map(CitationDto::from).collect(),
            })),
        ),
        StreamEvent::Done(d) => ("done", to(serde_json::to_value(DoneData::from(d)))),
        StreamEvent::Error { code, message } => (
            "error",
            to(serde_json::to_value(ErrorData { code, message })),
        ),
    }
}

/// `OpenAPI` description of one SSE event (documentation only).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatSseEvent {
    pub event: String,
    pub data: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_payload_shape() {
        let (name, v) = sse_parts(StreamEvent::Done(DoneOut {
            input_tokens: 5,
            output_tokens: 3,
            effective_model: "m".into(),
            selected_model: "m".into(),
            downgrade: false,
            downgrade_from: None,
            downgrade_reason: None,
            quota_warnings: None,
        }));
        assert_eq!(name, "done");
        assert_eq!(v["quota_decision"], "allow");
        assert_eq!(v["usage"]["input_tokens"], 5);
        assert!(v.get("downgrade_from").is_none());
        assert!(v.get("quota_warnings").is_none());
    }

    #[test]
    fn delta_uses_type_key() {
        let (_, v) = sse_parts(StreamEvent::Delta {
            kind: "text",
            content: "hi".into(),
        });
        assert_eq!(v["type"], "text");
        assert_eq!(v["content"], "hi");
    }
}
