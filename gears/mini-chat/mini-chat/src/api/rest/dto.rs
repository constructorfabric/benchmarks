//! REST DTOs (DESIGN section 3.3, `docs/api/api.json`).
//!
//! The Models API projection exposes only user-facing catalog fields. It never
//! carries provider routing (`provider_id`, `provider_model_id`), credit
//! multipliers, the policy version, `max_output_tokens` or `is_default`
//! (DESIGN "Non-Exposure Rules").
//!
//! Chat, message, reaction and turn DTOs never carry `user_id`, `tenant_id`
//! or `chat_id`: identity comes from the security context and the chat id
//! from the path. Optional fields are omitted from the JSON when absent
//! (`title`, `model`, token counts, `img_thumbnail`, `error_code`,
//! `assistant_message_id`, `doc_summary`, `summary_updated_at`);
//! `attachments` and `my_reaction` are always present (`my_reaction` may be
//! `null`). `AttachmentDetailDto` never carries provider file or vector store
//! identifiers.

use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;
use mini_chat_sdk::{ModelCatalogEntry, ModelTier};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::attachment_service::AttachmentDetail;
use crate::domain::chat_service::ChatDetail;
use crate::domain::message_service::{
    self, AttachmentSummary, ImgThumbnail, MessageView, ReactionView, TurnStatus,
};
use crate::domain::ports::{AttachmentKind, AttachmentStatus, MessageRole, Reaction};
use crate::domain::quota::{self, QuotaBucket};
use crate::domain::quota_service;
use crate::domain::stream::SendRequest;
use crate::domain::stream::events::{self, StreamEvent};

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
    // Stable internal model id (`ModelCatalogEntry.id`).
    pub model_id: String,
    pub display_name: String,
    pub tier: ModelTierDto,
    // Informational multiplier text such as `1x`.
    pub multiplier_display: String,
    // Omitted when the catalog entry has no description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub multimodal_capabilities: Vec<String>,
    pub context_window: u32,
}

impl From<&ModelCatalogEntry> for ModelDto {
    fn from(entry: &ModelCatalogEntry) -> Self {
        Self {
            model_id: entry.id.clone(),
            display_name: entry.display_name.clone(),
            tier: entry.tier.into(),
            multiplier_display: entry.multiplier_display.clone(),
            description: entry.description.clone(),
            multimodal_capabilities: entry.multimodal_capabilities.clone(),
            context_window: entry.context_window,
        }
    }
}

/// Response DTO for the model list endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ModelListDto {
    pub items: Vec<ModelDto>,
}

impl<'a> FromIterator<&'a ModelCatalogEntry> for ModelListDto {
    fn from_iter<I: IntoIterator<Item = &'a ModelCatalogEntry>>(entries: I) -> Self {
        Self {
            items: entries.into_iter().map(ModelDto::from).collect(),
        }
    }
}

// ── Chats ────────────────────────────────────────────────────────────────

/// Request DTO for creating a new chat.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct CreateChatReq {
    // Absent or `null` creates an untitled chat.
    pub title: Option<String>,
    // Absent or `null` selects the default model.
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
    // Omitted (never `null`) for an untitled chat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<ChatDetail> for ChatDetailDto {
    fn from(chat: ChatDetail) -> Self {
        Self {
            id: chat.id,
            model: chat.model,
            title: chat.title,
            is_temporary: chat.is_temporary,
            message_count: i64::try_from(chat.message_count).unwrap_or(i64::MAX),
            created_at: chat.created_at,
            updated_at: chat.updated_at,
        }
    }
}

// ── Messages ─────────────────────────────────────────────────────────────

/// Message author role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum MessageRoleDto {
    User,
    Assistant,
    System,
}

impl From<MessageRole> for MessageRoleDto {
    fn from(role: MessageRole) -> Self {
        match role {
            MessageRole::User => Self::User,
            MessageRole::Assistant => Self::Assistant,
            MessageRole::System => Self::System,
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
    fn from(kind: AttachmentKind) -> Self {
        match kind {
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
    fn from(status: AttachmentStatus) -> Self {
        match status {
            AttachmentStatus::Pending => Self::Pending,
            AttachmentStatus::Uploaded => Self::Uploaded,
            AttachmentStatus::Ready => Self::Ready,
            AttachmentStatus::Failed => Self::Failed,
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

impl From<Reaction> for ReactionKindDto {
    fn from(reaction: Reaction) -> Self {
        match reaction {
            Reaction::Like => Self::Like,
            Reaction::Dislike => Self::Dislike,
        }
    }
}

/// Server-generated preview thumbnail for an image attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ImgThumbnailDto {
    // Always `image/webp`.
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    // Standard base64 of the thumbnail bytes.
    pub data_base64: String,
}

impl From<ImgThumbnail> for ImgThumbnailDto {
    fn from(thumbnail: ImgThumbnail) -> Self {
        Self {
            content_type: thumbnail.content_type.to_owned(),
            width: thumbnail.width,
            height: thumbnail.height,
            data_base64: BASE64_STANDARD.encode(&thumbnail.data),
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
    // Present only for a ready image with a stored thumbnail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

impl From<AttachmentSummary> for AttachmentSummaryDto {
    fn from(summary: AttachmentSummary) -> Self {
        Self {
            attachment_id: summary.attachment_id,
            kind: summary.kind.into(),
            filename: summary.filename,
            status: summary.status.into(),
            img_thumbnail: summary.img_thumbnail.map(ImgThumbnailDto::from),
        }
    }
}

/// Full attachment details returned by the upload and GET attachment
/// endpoints. Provider identifiers (file, vector store) are never exposed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentDetailDto {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub status: AttachmentStatusDto,
    pub kind: AttachmentKindDto,
    // Present only for `status = failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    // Document summaries are not implemented (ADR-0007): always omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_summary: Option<String>,
    // Present only for a ready image with a stored thumbnail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
    // Always omitted, see `doc_summary`.
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub summary_updated_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<AttachmentDetail> for AttachmentDetailDto {
    fn from(detail: AttachmentDetail) -> Self {
        Self {
            id: detail.id,
            filename: detail.filename,
            // Every row the API writes has both; a row without them reports
            // the empty type and size 0 rather than breaking the schema.
            content_type: detail.content_type.unwrap_or_default(),
            size_bytes: detail.size_bytes.unwrap_or_default(),
            status: detail.status.into(),
            kind: detail.kind.into(),
            error_code: detail.error_code,
            doc_summary: None,
            img_thumbnail: detail.img_thumbnail.map(ImgThumbnailDto::from),
            summary_updated_at: None,
            created_at: detail.created_at,
        }
    }
}

/// Response DTO for a message in the list endpoint.
///
/// Aliased to `MiniChatMessageDto` in the `OpenAPI`` schema: `chat-engine` also
/// exposes a `MessageDto`, and both gears register into the same api-gateway
/// `OpenAPI`` registry, so the bare ident would collide in `components.schemas`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
#[schema(as = MiniChatMessageDto)]
pub struct MessageDto {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: MessageRoleDto,
    pub content: String,
    // Model of an assistant message; omitted for user messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    // Provider-reported counts; omitted for user messages and when 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    // Always present; empty when the message has no live attachment.
    pub attachments: Vec<AttachmentSummaryDto>,
    /// The caller's reaction to this message; `null` when there is none.
    #[schema(required = true)]
    pub my_reaction: Option<ReactionKindDto>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<MessageView> for MessageDto {
    fn from(message: MessageView) -> Self {
        let is_user = message.role == MessageRole::User;
        let tokens = |count: i64| (!is_user && count != 0).then_some(count);
        Self {
            id: message.id,
            request_id: message.request_id,
            role: message.role.into(),
            content: message.content,
            model: message.model.filter(|_| !is_user),
            input_tokens: tokens(message.input_tokens),
            output_tokens: tokens(message.output_tokens),
            attachments: message
                .attachments
                .into_iter()
                .map(AttachmentSummaryDto::from)
                .collect(),
            my_reaction: message.my_reaction.map(ReactionKindDto::from),
            created_at: message.created_at,
        }
    }
}

// ── Reactions ────────────────────────────────────────────────────────────

/// Request DTO for setting a reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct SetReactionReq {
    // `like` or `dislike`; any other value is `INVALID_REACTION`.
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
    fn from(view: ReactionView) -> Self {
        Self {
            message_id: view.message_id,
            reaction: view.reaction.into(),
            created_at: view.created_at,
        }
    }
}

// ── Turns ────────────────────────────────────────────────────────────────

/// Turn state as reported by the turn status endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum TurnStatusState {
    Running,
    Done,
    Error,
    Cancelled,
}

impl From<message_service::TurnStatusState> for TurnStatusState {
    fn from(state: message_service::TurnStatusState) -> Self {
        match state {
            message_service::TurnStatusState::Running => Self::Running,
            message_service::TurnStatusState::Done => Self::Done,
            message_service::TurnStatusState::Error => Self::Error,
            message_service::TurnStatusState::Cancelled => Self::Cancelled,
        }
    }
}

/// Response DTO for `GET /chats/{id}/turns/{request_id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct TurnStatusResponse {
    pub request_id: Uuid,
    pub state: TurnStatusState,
    // Terminal error code; omitted when none is stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    // Persisted assistant message; omitted when none is stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<TurnStatus> for TurnStatusResponse {
    fn from(status: TurnStatus) -> Self {
        Self {
            request_id: status.request_id,
            state: status.state.into(),
            error_code: status.error_code,
            assistant_message_id: status.assistant_message_id,
            updated_at: status.updated_at,
        }
    }
}

// ── Streaming ────────────────────────────────────────────────────────────

/// Web search toggle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct WebSearchConfig {
    pub enabled: bool,
}

/// Request body for `POST /v1/chats/{id}/messages:stream`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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

impl From<StreamMessageRequest> for SendRequest {
    fn from(req: StreamMessageRequest) -> Self {
        Self {
            content: req.content,
            request_id: req.request_id,
            attachment_ids: req.attachment_ids,
            web_search_enabled: req.web_search.is_some_and(|config| config.enabled),
        }
    }
}

/// Request DTO for `PATCH /chats/{id}/turns/{request_id}` (edit).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct EditTurnRequest {
    /// Replacement user message content (must be non-empty after trim).
    pub content: String,
}

/// Metadata about a thread summary applied to the current turn's context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ThreadSummaryInfo {
    /// Estimated token cost of the summary in the context window.
    pub token_estimate: u32,
}

/// Stream header event carrying the stream request ID and the assistant
/// message ID; `is_new_turn` is `false` on an idempotent replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    // Present only when a thread summary was included in the context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

impl From<events::StreamStarted> for StreamStartedData {
    fn from(started: events::StreamStarted) -> Self {
        Self {
            request_id: started.request_id,
            message_id: started.message_id,
            is_new_turn: started.is_new_turn,
            thread_summary_applied: started
                .thread_summary_applied
                .map(|token_estimate| ThreadSummaryInfo { token_estimate }),
        }
    }
}

/// Payload of the `ping` event: an empty JSON object (a unit struct would
/// be serialized and documented as `null`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
#[allow(
    clippy::empty_structs_with_brackets,
    reason = "the braces make serde write `{}` instead of `null`"
)]
pub struct PingData {}

/// Kind of a `delta` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

impl From<events::DeltaKind> for DeltaKind {
    fn from(kind: events::DeltaKind) -> Self {
        match kind {
            events::DeltaKind::Text => Self::Text,
            events::DeltaKind::Reasoning => Self::Reasoning,
        }
    }
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

impl From<events::ToolPhase> for ToolPhase {
    fn from(phase: events::ToolPhase) -> Self {
        match phase {
            events::ToolPhase::Start => Self::Start,
            events::ToolPhase::Done => Self::Done,
        }
    }
}

/// Tool lifecycle event.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct ToolData {
    pub phase: ToolPhase,
    pub name: String,
    // Non-sensitive tool metadata (an object).
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

impl From<events::CitationSource> for CitationSource {
    fn from(source: events::CitationSource) -> Self {
        match source {
            events::CitationSource::File => Self::File,
            events::CitationSource::Web => Self::Web,
        }
    }
}

/// A character span within response text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct TextSpan {
    pub start: usize,
    pub end: usize,
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
    // Not populated in P1 (never serialized).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

impl From<events::Citation> for Citation {
    fn from(citation: events::Citation) -> Self {
        Self {
            source: citation.source.into(),
            title: citation.title,
            url: citation.url,
            attachment_id: citation.attachment_id,
            snippet: citation.snippet,
            span: citation.span.map(|span| TextSpan {
                start: span.start,
                end: span.end,
            }),
            score: None,
        }
    }
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

/// Quota decision reported in `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaDecisionKind {
    Allow,
    Downgrade,
}

impl From<quota_service::QuotaDecisionKind> for QuotaDecisionKind {
    fn from(kind: quota_service::QuotaDecisionKind) -> Self {
        match kind {
            quota_service::QuotaDecisionKind::Allow => Self::Allow,
            quota_service::QuotaDecisionKind::Downgrade => Self::Downgrade,
        }
    }
}

/// Quota tier classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaTier {
    Premium,
    Total,
}

impl From<QuotaBucket> for QuotaTier {
    fn from(bucket: QuotaBucket) -> Self {
        match bucket {
            QuotaBucket::Premium => Self::Premium,
            QuotaBucket::Total => Self::Total,
        }
    }
}

/// Quota period classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaPeriod {
    Daily,
    Monthly,
}

impl From<quota::QuotaPeriod> for QuotaPeriod {
    fn from(period: quota::QuotaPeriod) -> Self {
        match period {
            quota::QuotaPeriod::Daily => Self::Daily,
            quota::QuotaPeriod::Monthly => Self::Monthly,
        }
    }
}

/// Per-tier, per-period quota warning entry in the SSE `done` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaWarning {
    pub tier: QuotaTier,
    pub period: QuotaPeriod,
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

impl From<quota_service::QuotaWarning> for QuotaWarning {
    fn from(warning: quota_service::QuotaWarning) -> Self {
        Self {
            tier: warning.tier.into(),
            period: warning.period.into(),
            remaining_percentage: u32::from(warning.remaining_percentage),
            warning: warning.warning,
            exhausted: warning.exhausted,
            next_reset: warning.next_reset,
        }
    }
}

/// Successful stream completion.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct DoneData {
    pub usage: Usage,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: QuotaDecisionKind,
    // Present only on a downgrade.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    // Present only on a live downgrade (omitted on replay).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    // Present only on CAS-winning completed turns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

impl From<events::Done> for DoneData {
    fn from(done: events::Done) -> Self {
        Self {
            usage: Usage {
                input_tokens: done.usage.input_tokens,
                output_tokens: done.usage.output_tokens,
            },
            effective_model: done.effective_model,
            selected_model: done.selected_model,
            quota_decision: done.quota_decision.into(),
            downgrade_from: done.downgrade_from,
            downgrade_reason: done
                .downgrade_reason
                .map(|reason| reason.as_str().to_owned()),
            quota_warnings: done
                .quota_warnings
                .map(|warnings| warnings.into_iter().map(QuotaWarning::from).collect()),
        }
    }
}

/// Stream error (terminal).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
}

impl From<events::StreamError> for ErrorData {
    fn from(error: events::StreamError) -> Self {
        Self {
            code: error.code,
            message: error.message,
        }
    }
}

/// `OpenAPI` description of one SSE event of the `messages:stream`, retry and
/// edit responses. Each event is sent as `event: <name>` and `data: <payload>`
/// (JSON); `event` below is the SSE event name and `data` its payload. The
/// SSE relay writes it through [`MiniChatSseEvent::into_parts`].
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

impl MiniChatSseEvent {
    /// SSE event name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::StreamStarted(_) => "stream_started",
            Self::Ping(_) => "ping",
            Self::Delta(_) => "delta",
            Self::Tool(_) => "tool",
            Self::Citations(_) => "citations",
            Self::Done(_) => "done",
            Self::Error(_) => "error",
        }
    }

    /// The SSE event name and its JSON `data` payload.
    ///
    /// # Errors
    ///
    /// A [`serde_json::Error`] when the payload cannot be serialized.
    pub fn into_parts(self) -> Result<(&'static str, String), serde_json::Error> {
        let name = self.name();
        let data = match self {
            Self::StreamStarted(data) => serde_json::to_string(&data),
            Self::Ping(data) => serde_json::to_string(&data),
            Self::Delta(data) => serde_json::to_string(&data),
            Self::Tool(data) => serde_json::to_string(&data),
            Self::Citations(data) => serde_json::to_string(&data),
            Self::Done(data) => serde_json::to_string(&data),
            Self::Error(data) => serde_json::to_string(&data),
        }?;
        Ok((name, data))
    }
}

impl From<StreamEvent> for MiniChatSseEvent {
    fn from(event: StreamEvent) -> Self {
        match event {
            StreamEvent::StreamStarted(started) => Self::StreamStarted(started.into()),
            StreamEvent::Delta(delta) => Self::Delta(DeltaData {
                kind: delta.kind.into(),
                content: delta.content,
            }),
            StreamEvent::Tool(tool) => Self::Tool(ToolData {
                phase: tool.phase.into(),
                name: tool.name,
                details: tool.details,
            }),
            StreamEvent::Citations(items) => Self::Citations(CitationsData {
                items: items.into_iter().map(Citation::from).collect(),
            }),
            StreamEvent::Done(done) => Self::Done((*done).into()),
            StreamEvent::Error(error) => Self::Error(error.into()),
        }
    }
}

// ── Quota status ─────────────────────────────────────────────────────────

/// One period of a quota tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl From<quota_service::PeriodStatus> for QuotaPeriodStatus {
    fn from(status: quota_service::PeriodStatus) -> Self {
        Self {
            period: status.period.into(),
            limit_credits_micro: status.limit_credits_micro,
            used_credits_micro: status.used_credits_micro,
            remaining_credits_micro: status.remaining_credits_micro,
            remaining_percentage: u32::from(status.remaining_percentage),
            next_reset: status.next_reset,
            warning: status.warning,
            exhausted: status.exhausted,
        }
    }
}

/// One quota tier with its periods.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatus {
    pub tier: QuotaTier,
    pub periods: Vec<QuotaPeriodStatus>,
}

/// Response DTO for `GET /v1/quota/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u32,
}

impl From<quota_service::QuotaStatus> for QuotaStatusResponse {
    fn from(status: quota_service::QuotaStatus) -> Self {
        Self {
            tiers: status
                .tiers
                .into_iter()
                .map(|tier| QuotaTierStatus {
                    tier: tier.tier.into(),
                    periods: tier
                        .periods
                        .into_iter()
                        .map(QuotaPeriodStatus::from)
                        .collect(),
                })
                .collect(),
            warning_threshold_pct: u32::from(status.warning_threshold_pct),
        }
    }
}
