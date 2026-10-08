//! REST / SSE DTOs (wire contract of DESIGN §3.3; shapes follow `docs/api/api.json`).

use time::OffsetDateTime;
use toolkit_odata_macros::ODataFilterable;
use uuid::Uuid;

// ── Chats ──────────────────────────────────────────────────────────────────

/// Request DTO for creating a new chat.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone, Default)]
pub struct CreateChatReq {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

/// Request DTO for updating a chat title.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct UpdateChatReq {
    pub title: String,
}

/// Response DTO for chat details.
#[derive(Debug, Clone, ODataFilterable)]
#[toolkit_macros::api_dto(response)]
pub struct ChatDetailDto {
    #[odata(filter(kind = "String"))]
    pub id: Uuid,
    pub model: String,
    #[odata(filter(kind = "String"))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[odata(filter(kind = "DateTimeUtc"))]
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

// ── Messages ───────────────────────────────────────────────────────────────

/// Message author role.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRoleDto {
    User,
    Assistant,
    System,
}

/// Attachment kind.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKindDto {
    Document,
    Image,
}

/// Attachment lifecycle status.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentStatusDto {
    Pending,
    Uploaded,
    Ready,
    Failed,
}

/// Server-generated preview thumbnail for an image attachment.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ImgThumbnailDto {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data_base64: String,
}

/// Lightweight attachment metadata embedded in Message responses.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct AttachmentSummaryDto {
    pub attachment_id: Uuid,
    pub kind: AttachmentKindDto,
    pub filename: String,
    pub status: AttachmentStatusDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

/// Reaction value.
#[toolkit_macros::api_dto(request, response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionKindDto {
    Like,
    Dislike,
}

/// Response DTO for a message in the list endpoint.
#[derive(Debug, Clone, ODataFilterable)]
#[toolkit_macros::api_dto(response)]
#[schema(as = MiniChatMessageDto)]
pub struct MiniChatMessageDto {
    #[odata(filter(kind = "String"))]
    pub id: Uuid,
    pub request_id: Uuid,
    #[odata(filter(kind = "String"))]
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
    #[odata(filter(kind = "DateTimeUtc"))]
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

// ── Reactions ──────────────────────────────────────────────────────────────

/// Request DTO for setting a reaction (validated by the handler: `like` | `dislike`).
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

// ── Models ─────────────────────────────────────────────────────────────────

/// Model tier.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTierDto {
    Standard,
    Premium,
}

/// Response DTO for a single model.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
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

/// Response DTO for the model list endpoint.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ModelListDto {
    pub items: Vec<ModelDto>,
}

// ── Turns ──────────────────────────────────────────────────────────────────

/// Turn state as reported by the turn status endpoint.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatusState {
    Running,
    Done,
    Error,
    Cancelled,
}

/// Response DTO for `GET /chats/{id}/turns/{request_id}`.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
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

/// Request DTO for `PATCH /chats/{id}/turns/{request_id}` (edit).
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct EditTurnRequest {
    pub content: String,
}

// ── Streaming ──────────────────────────────────────────────────────────────

/// Web search toggle.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone, Default)]
pub struct WebSearchConfig {
    pub enabled: bool,
}

/// Request body for `POST /v1/chats/{id}/messages:stream`.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
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

/// Metadata about a thread summary applied to the current turn's context.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ThreadSummaryInfo {
    pub token_estimate: u32,
}

/// `stream_started` payload.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

/// `ping` payload (empty object).
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Default)]
pub struct PingData {}

/// Kind of a `delta` chunk.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// `delta` payload.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct DeltaData {
    #[serde(rename = "type")]
    pub kind: DeltaKind,
    pub content: String,
}

/// Lifecycle phase of a tool invocation.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPhase {
    Start,
    Done,
}

/// `tool` payload.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ToolData {
    pub phase: ToolPhase,
    pub name: String,
    pub details: serde_json::Value,
}

/// Citation source.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CitationSource {
    File,
    Web,
}

/// Character range of a citation.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSpan {
    pub start: u64,
    pub end: u64,
}

/// A citation extracted from provider annotations.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct Citation {
    pub source: CitationSource,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TextSpan>,
}

/// `citations` payload.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct CitationsData {
    pub items: Vec<Citation>,
}

/// Token usage counters.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Quota decision reported in `done`.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDecisionKind {
    Allow,
    Downgrade,
}

/// Quota tier classification.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaTier {
    Premium,
    Total,
}

/// Quota period classification.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaPeriod {
    Daily,
    Monthly,
}

/// Per-tier, per-period quota warning entry in the SSE `done` event.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaWarning {
    pub tier: QuotaTier,
    pub period: QuotaPeriod,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option",
        default
    )]
    pub next_reset: Option<OffsetDateTime>,
}

/// `done` payload.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct DoneData {
    pub usage: Usage,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: QuotaDecisionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

/// `error` payload (terminal).
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
}

/// `OpenAPI` description of one SSE event (`event: <name>`, `data: <payload>`).
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
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

    /// JSON payload (the `data:` line).
    #[must_use]
    pub fn data_json(&self) -> serde_json::Value {
        let r = match self {
            Self::StreamStarted(d) => serde_json::to_value(d),
            Self::Ping(d) => serde_json::to_value(d),
            Self::Delta(d) => serde_json::to_value(d),
            Self::Tool(d) => serde_json::to_value(d),
            Self::Citations(d) => serde_json::to_value(d),
            Self::Done(d) => serde_json::to_value(d),
            Self::Error(d) => serde_json::to_value(d),
        };
        r.unwrap_or_else(|_| serde_json::json!({}))
    }

    /// Whether this is a terminal event (`done` / `error`).
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error(_))
    }
}

// ── Attachments ────────────────────────────────────────────────────────────

/// Full attachment details.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
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
        with = "time::serde::rfc3339::option",
        default
    )]
    pub summary_updated_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

// ── Quota status ───────────────────────────────────────────────────────────

/// One period of a tier in the quota status response.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
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

/// One tier of the quota status response.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaTierStatus {
    pub tier: QuotaTier,
    pub periods: Vec<QuotaPeriodStatus>,
}

/// `GET /v1/quota/status` response.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u32,
}
