//! REST DTOs (`OpenAPI` component names per `docs/api/api.json`).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use mini_chat_sdk::{ModelCatalogEntry, ModelTier};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::enums::{AttachmentKind, AttachmentStatus, MessageRole, ReactionKind};
use crate::domain::services::attachment_service::AttachmentView;
use crate::domain::services::chat_service::ChatView;
use crate::domain::services::message_service::{AttachmentSummaryView, MessageView, ThumbnailView};
use crate::domain::services::quota_service::{
    QuotaDecisionKind, QuotaPeriodKind, QuotaPeriodStatusView, QuotaStatusView, QuotaTierKind,
    QuotaTierStatusView, QuotaWarningView,
};
use crate::domain::services::reaction_service::ReactionView;
use crate::domain::services::turn_service::{TurnStatusState, TurnStatusView};

// ── Models ───────────────────────────────────────────────────────────────────

/// Model tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ModelTierDto {
    Standard,
    Premium,
}

impl From<ModelTier> for ModelTierDto {
    fn from(tier: ModelTier) -> Self {
        match tier {
            ModelTier::Standard => Self::Standard,
            ModelTier::Premium => Self::Premium,
        }
    }
}

/// Response DTO for a single model.
#[derive(Debug, Clone, PartialEq, Eq)]
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
            tier: m.tier.into(),
            multiplier_display: m.multiplier_display.clone(),
            description: (!m.description.is_empty()).then(|| m.description.clone()),
            multimodal_capabilities: m.multimodal_capabilities.clone(),
            context_window: m.context_window,
        }
    }
}

/// Response DTO for the model list endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ModelListDto {
    pub items: Vec<ModelDto>,
}

// ── Chats ────────────────────────────────────────────────────────────────────

/// Request DTO for creating a new chat.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct CreateChatReq {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// Request DTO for updating a chat title.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct UpdateChatReq {
    pub title: String,
}

/// Response DTO for chat details.
#[derive(Debug, Clone, PartialEq, Eq)]
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

// ── Messages ─────────────────────────────────────────────────────────────────

/// Message author role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum MessageRoleDto {
    User,
    Assistant,
    System,
}

impl From<MessageRole> for MessageRoleDto {
    fn from(r: MessageRole) -> Self {
        match r {
            MessageRole::User => Self::User,
            MessageRole::Assistant => Self::Assistant,
            MessageRole::System => Self::System,
        }
    }
}

/// Reaction value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ReactionKindDto {
    Like,
    Dislike,
}

impl From<ReactionKind> for ReactionKindDto {
    fn from(r: ReactionKind) -> Self {
        match r {
            ReactionKind::Like => Self::Like,
            ReactionKind::Dislike => Self::Dislike,
        }
    }
}

/// Attachment kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum AttachmentKindDto {
    Document,
    Image,
}

impl From<AttachmentKind> for AttachmentKindDto {
    fn from(k: AttachmentKind) -> Self {
        match k {
            AttachmentKind::Document => Self::Document,
            AttachmentKind::Image => Self::Image,
        }
    }
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

impl From<AttachmentStatus> for AttachmentStatusDto {
    fn from(s: AttachmentStatus) -> Self {
        match s {
            AttachmentStatus::Pending => Self::Pending,
            AttachmentStatus::Uploaded => Self::Uploaded,
            AttachmentStatus::Ready => Self::Ready,
            AttachmentStatus::Failed => Self::Failed,
        }
    }
}

/// Server-generated preview thumbnail for an image attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
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
            content_type: t.content_type.to_owned(),
            width: t.width,
            height: t.height,
            data_base64: BASE64.encode(&t.data),
        }
    }
}

/// Lightweight attachment metadata embedded in Message responses.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentSummaryDto {
    pub attachment_id: Uuid,
    pub kind: AttachmentKindDto,
    pub filename: String,
    pub status: AttachmentStatusDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

impl From<AttachmentSummaryView> for AttachmentSummaryDto {
    fn from(a: AttachmentSummaryView) -> Self {
        Self {
            attachment_id: a.attachment_id,
            kind: a.kind.into(),
            filename: a.filename,
            status: a.status.into(),
            img_thumbnail: a.img_thumbnail.map(Into::into),
        }
    }
}

/// Full attachment details returned by the GET attachment endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Never populated (ADR-0007).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
    /// Never populated (ADR-0007).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub summary_updated_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<AttachmentView> for AttachmentDetailDto {
    fn from(a: AttachmentView) -> Self {
        Self {
            id: a.id,
            filename: a.filename,
            content_type: a.content_type,
            size_bytes: a.size_bytes,
            status: a.status.into(),
            kind: a.kind.into(),
            error_code: a.error_code,
            doc_summary: None,
            img_thumbnail: a.img_thumbnail.map(Into::into),
            summary_updated_at: None,
            created_at: a.created_at,
        }
    }
}

/// Response DTO for a message in the list endpoint.
///
/// Aliased to `MiniChatMessageDto` in the `OpenAPI` schema: `chat-engine` also
/// exposes a `MessageDto`, and both gears register into the same api-gateway
/// `OpenAPI` registry, so the bare ident would collide in `components.schemas`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
#[schema(as = MiniChatMessageDto)]
pub struct MessageDto {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: MessageRoleDto,
    pub content: String,
    pub attachments: Vec<AttachmentSummaryDto>,
    /// The caller's reaction to this message; `null` when there is none.
    #[schema(required = true)]
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
    fn from(m: MessageView) -> Self {
        Self {
            id: m.id,
            request_id: m.request_id,
            role: m.role.into(),
            content: m.content,
            attachments: m.attachments.into_iter().map(Into::into).collect(),
            my_reaction: m.my_reaction.map(Into::into),
            model: m.model,
            input_tokens: m.input_tokens,
            output_tokens: m.output_tokens,
            created_at: m.created_at,
        }
    }
}

// ── Reactions ────────────────────────────────────────────────────────────────

/// Request DTO for setting a reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct SetReactionReq {
    pub reaction: String,
}

/// Response DTO for a reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
#[schema(as = MiniChatReactionDto)]
pub struct ReactionDto {
    pub message_id: Uuid,
    pub reaction: ReactionKindDto,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<ReactionView> for ReactionDto {
    fn from(r: ReactionView) -> Self {
        Self {
            message_id: r.message_id,
            reaction: r.reaction.into(),
            created_at: r.created_at,
        }
    }
}

// ── Turns ────────────────────────────────────────────────────────────────────

/// Turn state as reported by the turn status endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
#[schema(as = TurnStatusState)]
pub enum TurnStatusStateDto {
    Running,
    Done,
    Error,
    Cancelled,
}

impl From<TurnStatusState> for TurnStatusStateDto {
    fn from(s: TurnStatusState) -> Self {
        match s {
            TurnStatusState::Running => Self::Running,
            TurnStatusState::Done => Self::Done,
            TurnStatusState::Error => Self::Error,
            TurnStatusState::Cancelled => Self::Cancelled,
        }
    }
}

/// Response DTO for `GET /chats/{id}/turns/{request_id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct TurnStatusResponse {
    pub request_id: Uuid,
    pub state: TurnStatusStateDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<TurnStatusView> for TurnStatusResponse {
    fn from(t: TurnStatusView) -> Self {
        Self {
            request_id: t.request_id,
            state: t.state.into(),
            error_code: t.error_code,
            assistant_message_id: t.assistant_message_id,
            updated_at: t.updated_at,
        }
    }
}

// ── Quota ────────────────────────────────────────────────────────────────────

/// Quota tier classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
#[schema(as = QuotaTier)]
pub enum QuotaTierDto {
    Premium,
    Total,
}

impl From<QuotaTierKind> for QuotaTierDto {
    fn from(t: QuotaTierKind) -> Self {
        match t {
            QuotaTierKind::Premium => Self::Premium,
            QuotaTierKind::Total => Self::Total,
        }
    }
}

/// Quota period classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
#[schema(as = QuotaPeriod)]
pub enum QuotaPeriodDto {
    Daily,
    Monthly,
}

impl From<QuotaPeriodKind> for QuotaPeriodDto {
    fn from(p: QuotaPeriodKind) -> Self {
        match p {
            QuotaPeriodKind::Daily => Self::Daily,
            QuotaPeriodKind::Monthly => Self::Monthly,
        }
    }
}

/// Quota decision reported in `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
#[schema(as = QuotaDecisionKind)]
pub enum QuotaDecisionKindDto {
    Allow,
    Downgrade,
}

impl From<QuotaDecisionKind> for QuotaDecisionKindDto {
    fn from(d: QuotaDecisionKind) -> Self {
        match d {
            QuotaDecisionKind::Allow => Self::Allow,
            QuotaDecisionKind::Downgrade => Self::Downgrade,
        }
    }
}

/// One period of a tier in `GET /quota/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaPeriodStatus {
    pub period: QuotaPeriodDto,
    pub limit_credits_micro: i64,
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

impl From<QuotaPeriodStatusView> for QuotaPeriodStatus {
    fn from(p: QuotaPeriodStatusView) -> Self {
        Self {
            period: p.period.into(),
            limit_credits_micro: p.limit_credits_micro,
            used_credits_micro: p.used_credits_micro,
            remaining_credits_micro: p.remaining_credits_micro,
            remaining_percentage: p.remaining_percentage,
            next_reset: p.next_reset,
            warning: p.warning,
            exhausted: p.exhausted,
        }
    }
}

/// One tier in `GET /quota/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatus {
    pub tier: QuotaTierDto,
    pub periods: Vec<QuotaPeriodStatus>,
}

impl From<QuotaTierStatusView> for QuotaTierStatus {
    fn from(t: QuotaTierStatusView) -> Self {
        Self {
            tier: t.tier.into(),
            periods: t.periods.into_iter().map(Into::into).collect(),
        }
    }
}

/// Response DTO for `GET /quota/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u32,
}

impl From<QuotaStatusView> for QuotaStatusResponse {
    fn from(v: QuotaStatusView) -> Self {
        Self {
            tiers: v.tiers.into_iter().map(Into::into).collect(),
            warning_threshold_pct: u32::from(v.warning_threshold_pct),
        }
    }
}

/// Per-tier, per-period quota warning entry in the SSE `done` event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaWarning {
    pub tier: QuotaTierDto,
    pub period: QuotaPeriodDto,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    /// RFC 3339 timestamp of the next quota-period reset.
    /// Present when `warning` or `exhausted` is true; absent otherwise.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub next_reset: Option<OffsetDateTime>,
}

impl From<QuotaWarningView> for QuotaWarning {
    fn from(w: QuotaWarningView) -> Self {
        Self {
            tier: w.tier.into(),
            period: w.period.into(),
            remaining_percentage: w.remaining_percentage,
            warning: w.warning,
            exhausted: w.exhausted,
            next_reset: w.next_reset,
        }
    }
}

// ── Streaming ────────────────────────────────────────────────────────────────

/// Web search toggle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct WebSearchConfig {
    pub enabled: bool,
}

/// Request body for `POST /v1/chats/{id}/messages:stream`.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct EditTurnRequest {
    pub content: String,
}

/// Metadata about a thread summary applied to the current turn's context.
///
/// Sent in the `stream_started` event when the conversation has an active
/// thread summary. The UI can use this to show an informational banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ThreadSummaryInfo {
    /// Estimated token cost of the summary in the context window.
    pub token_estimate: u32,
}

/// Stream header event carrying the stream request ID and server-generated
/// assistant message ID.
///
/// Emitted as the first event in every SSE stream (both new generations and
/// replays). `is_new_turn` distinguishes replayed completed turns from live
/// generations.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    /// `true` for a live generation (new turn); `false` when the stream
    /// replays an already-completed turn (idempotent replay).
    pub is_new_turn: bool,
    /// Present when a thread summary is included in the context window.
    /// UI can display an informational indicator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

/// Payload of the `ping` event: an empty JSON object (a unit struct would
/// be documented as `null`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
#[allow(clippy::empty_structs_with_brackets)] // `{}` on the wire, `type: object` in the schema
pub struct PingData {}

/// Kind of a `delta` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// Delta text chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct DeltaData {
    #[serde(rename = "type")]
    pub kind: DeltaKind,
    pub content: String,
}

/// Lifecycle phase of a tool invocation within a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ToolPhase {
    Start,
    Done,
}

/// Tool lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ToolData {
    pub phase: ToolPhase,
    pub name: String,
    #[schema(value_type = Object)]
    pub details: serde_json::Value,
}

/// Whether a citation came from a file or web search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum CitationSource {
    File,
    Web,
}

/// A character span within response text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct TextSpan {
    pub start: u64,
    pub end: u64,
}

/// A citation extracted from provider annotations.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct Citation {
    pub source: CitationSource,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The attachment UUID (the provider file id before mapping).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TextSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// Citations from provider annotations.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct CitationsData {
    pub items: Vec<Citation>,
}

/// Token usage counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Successful stream completion.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct DoneData {
    pub usage: Usage,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: QuotaDecisionKindDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

/// Stream error (terminal).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
}

/// `OpenAPI` description of one SSE event of the `messages:stream`, retry and
/// edit responses. Each event is sent as `event: <name>` and `data: <payload>`
/// (JSON); `event` below is the SSE event name and `data` its payload. This
/// type only documents the wire format; `StreamEvent::into_sse_event` writes it.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
#[serde(tag = "event", content = "data")]
pub enum MiniChatSseEvent {
    StreamStarted(StreamStartedData),
    Ping(PingData),
    Delta(DeltaData),
    Tool(ToolData),
    Citations(CitationsData),
    Done(DoneData),
    Error(ErrorData),
}

#[cfg(test)]
#[path = "dto_tests.rs"]
mod dto_tests;
