//! REST DTOs for the mini-chat API.
//!
//! Shapes follow the generated `OpenAPI` document (`docs/api/api.json`).

use time::OffsetDateTime;
use toolkit_odata_macros::ODataFilterable;
use uuid::Uuid;

/// Serialize an `OffsetDateTime` as RFC 3339 (`...Z`).
#[must_use]
pub fn fmt_ts(ts: &OffsetDateTime) -> String {
    ts.to_offset(time::UtcOffset::UTC)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

mod ts_serde {
    use serde::Serializer;
    use time::OffsetDateTime;

    pub fn serialize<S: Serializer>(ts: &OffsetDateTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::fmt_ts(ts))
    }
}

/// `POST /v1/chats` request.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct CreateChatReq {
    /// Optional chat title (trimmed, 1–255 chars).
    pub title: Option<String>,
    /// Optional model id (defaults via the default-model algorithm).
    pub model: Option<String>,
}

/// `PATCH /v1/chats/{id}` request.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct UpdateChatReq {
    /// New chat title (trimmed, 1–255 chars).
    pub title: String,
}

/// Chat detail response (also the list item shape).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ChatDetailDto {
    /// Chat id.
    pub id: Uuid,
    /// Selected model (locked).
    pub model: String,
    /// Chat title (omitted when unset).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Reserved (P2).
    pub is_temporary: bool,
    /// Number of messages.
    pub message_count: i64,
    /// Creation time.
    #[serde(serialize_with = "ts_serde::serialize")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
    /// Last activity time.
    #[serde(serialize_with = "ts_serde::serialize")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: OffsetDateTime,
}

/// Chat list `OData` filter fields.
#[derive(Debug, Clone, ODataFilterable)]
pub struct ChatListQuery {
    /// Last activity time.
    #[odata(filter(kind = "DateTimeUtc"))]
    pub updated_at: String,
    /// Chat id.
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    /// Chat title.
    #[odata(filter(kind = "String"))]
    pub title: String,
}

/// Message list `OData` filter fields.
#[derive(Debug, Clone, ODataFilterable)]
pub struct MessageListQuery {
    /// Creation time.
    #[odata(filter(kind = "DateTimeUtc"))]
    pub created_at: String,
    /// Message id.
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    /// Message role.
    #[odata(filter(kind = "String"))]
    pub role: String,
}

/// Attachment kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum AttachmentKindDto {
    /// Document.
    Document,
    /// Image.
    Image,
}

/// Attachment lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum AttachmentStatusDto {
    /// Upload started.
    Pending,
    /// Provider upload done, indexing not finished.
    Uploaded,
    /// Ready for use.
    Ready,
    /// Failed.
    Failed,
}

/// Server-generated preview thumbnail for an image attachment.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ImgThumbnailDto {
    /// Always `image/webp`.
    pub content_type: String,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// Base64-encoded WebP bytes.
    pub data_base64: String,
}

/// Full attachment details (`GET .../attachments/{attachment_id}`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentDetailDto {
    /// Attachment id.
    pub id: Uuid,
    /// Original filename.
    pub filename: String,
    /// MIME type.
    pub content_type: String,
    /// File size in bytes.
    pub size_bytes: i64,
    /// Lifecycle status.
    pub status: AttachmentStatusDto,
    /// Attachment kind.
    pub kind: AttachmentKindDto,
    /// Stable internal error code (only when `status=failed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Always null in P1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_summary: Option<String>,
    /// Server-generated thumbnail (only `ready` images).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
    /// Always null in P1.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub summary_updated_at: Option<OffsetDateTime>,
    /// Upload time.
    #[serde(serialize_with = "ts_serde::serialize")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

/// Lightweight attachment metadata embedded in Message responses.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentSummaryDto {
    /// Attachment id.
    pub attachment_id: Uuid,
    /// Attachment kind.
    pub kind: AttachmentKindDto,
    /// Original filename.
    pub filename: String,
    /// Lifecycle status.
    pub status: AttachmentStatusDto,
    /// Server-generated thumbnail (only `ready` images).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

/// Message author role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum MessageRoleDto {
    /// User message.
    User,
    /// Assistant message.
    Assistant,
    /// System message.
    System,
}

/// Reaction value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum ReactionKindDto {
    /// Like.
    Like,
    /// Dislike.
    Dislike,
}

/// Message response in the list endpoint.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatMessageDto {
    /// Message id.
    pub id: Uuid,
    /// Turn correlation key (always present).
    pub request_id: Uuid,
    /// Author role.
    pub role: MessageRoleDto,
    /// Message content.
    pub content: String,
    /// Associated attachments (always present, empty when none).
    pub attachments: Vec<AttachmentSummaryDto>,
    /// The caller's reaction (`null` when none or not an assistant message).
    pub my_reaction: Option<ReactionKindDto>,
    /// Effective model (assistant messages only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Input tokens (assistant messages; omitted when 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    /// Output tokens (assistant messages; omitted when 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    /// Creation time.
    #[serde(serialize_with = "ts_serde::serialize")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

/// `messages:stream` request body.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct StreamMessageRequest {
    /// Message content (must be non-empty).
    pub content: String,
    /// Idempotency key (any UUID; server-generated when omitted).
    pub request_id: Option<Uuid>,
    /// Attachment IDs to include.
    #[serde(default)]
    pub attachment_ids: Option<Vec<Uuid>>,
    /// Web search configuration.
    #[serde(default)]
    pub web_search: Option<WebSearchConfig>,
}

/// Web search toggle.
#[derive(Debug, Clone, Copy)]
#[toolkit_macros::api_dto(request)]
pub struct WebSearchConfig {
    /// Enable web search for this turn.
    pub enabled: bool,
}

/// `PATCH .../turns/{request_id}` (edit) request.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct EditTurnRequest {
    /// Replacement user message content.
    pub content: String,
}

/// `PUT .../messages/{msg_id}/reaction` request.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct SetReactionReq {
    /// Reaction value (`like` or `dislike`).
    pub reaction: String,
}

/// Reaction response.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatReactionDto {
    /// Message id.
    pub message_id: Uuid,
    /// Reaction value.
    pub reaction: ReactionKindDto,
    /// Creation time.
    #[serde(serialize_with = "ts_serde::serialize")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

/// Turn state as reported by the turn status endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum TurnStatusState {
    /// Generation in progress.
    Running,
    /// Completed.
    Done,
    /// Failed.
    Error,
    /// Cancelled.
    Cancelled,
}

/// `GET .../turns/{request_id}` response.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct TurnStatusResponse {
    /// Turn correlation key.
    pub request_id: Uuid,
    /// Turn state.
    pub state: TurnStatusState,
    /// Terminal error code (only when `state=error`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Persisted assistant message id (when present).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    /// Last update time.
    #[serde(serialize_with = "ts_serde::serialize")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: OffsetDateTime,
}

/// Model tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum ModelTierDto {
    /// Standard tier.
    Standard,
    /// Premium tier.
    Premium,
}

/// A single model in the Models API.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ModelDto {
    /// Stable internal model identifier.
    pub model_id: String,
    /// User-facing name.
    pub display_name: String,
    /// Rate-limit tier.
    pub tier: ModelTierDto,
    /// Human-readable credit multiplier.
    pub multiplier_display: String,
    /// User-facing help text (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Capability flags.
    pub multimodal_capabilities: Vec<String>,
    /// Context window in tokens.
    pub context_window: u32,
}

/// `GET /v1/models` response.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ModelListDto {
    /// Visible models.
    pub items: Vec<ModelDto>,
}

/// Quota tier classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum QuotaTier {
    /// Premium sub-cap.
    Premium,
    /// Global ceiling.
    Total,
}

/// Quota period classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum QuotaPeriod {
    /// Daily (UTC).
    Daily,
    /// Monthly (UTC).
    Monthly,
}

/// Per-period quota status.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaPeriodStatus {
    /// Period.
    pub period: QuotaPeriod,
    /// Limit in micro-credits.
    pub limit_credits_micro: i64,
    /// Used (spent + reserved) in micro-credits.
    pub used_credits_micro: i64,
    /// Remaining micro-credits.
    pub remaining_credits_micro: i64,
    /// Floored remaining percentage (0–100).
    pub remaining_percentage: u32,
    /// RFC 3339 timestamp of the next reset.
    #[serde(serialize_with = "ts_serde::serialize")]
    #[schema(value_type = String, format = DateTime)]
    pub next_reset: OffsetDateTime,
    /// Warning flag.
    pub warning: bool,
    /// Exhausted flag.
    pub exhausted: bool,
}

/// Per-tier quota status.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatus {
    /// Tier.
    pub tier: QuotaTier,
    /// Per-period breakdown.
    pub periods: Vec<QuotaPeriodStatus>,
}

/// `GET /v1/quota/status` response.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponse {
    /// Per-tier breakdown.
    pub tiers: Vec<QuotaTierStatus>,
    /// Configured warning threshold percentage.
    pub warning_threshold_pct: u32,
}

/// Per-tier, per-period quota warning entry in the SSE `done` event.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct QuotaWarning {
    /// Tier.
    pub tier: QuotaTier,
    /// Period.
    pub period: QuotaPeriod,
    /// Floored remaining percentage (0–100).
    pub remaining_percentage: u32,
    /// Warning flag.
    pub warning: bool,
    /// Exhausted flag.
    pub exhausted: bool,
    /// RFC 3339 timestamp of the next quota-period reset (present when
    /// `warning` or `exhausted` is true).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub next_reset: Option<OffsetDateTime>,
}

// ---- SSE event payloads ----

/// Kind of a `delta` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum DeltaKind {
    /// Text content.
    Text,
    /// Model reasoning (`vLLM` `<think>` blocks).
    Reasoning,
}

/// Metadata about a thread summary applied to the context.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ThreadSummaryInfo {
    /// Estimated token cost of the summary.
    pub token_estimate: u32,
}

/// `stream_started` event payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct StreamStartedData {
    /// Resolved turn correlation key.
    pub request_id: Uuid,
    /// Assistant message id (pre-allocated or persisted).
    pub message_id: Uuid,
    /// `false` for an idempotent replay.
    pub is_new_turn: bool,
    /// Present when a thread summary is part of the context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

/// `ping` event payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PingData;

/// `delta` event payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct DeltaData {
    /// Output type.
    #[serde(rename = "type")]
    pub kind: DeltaKind,
    /// Incremental text fragment.
    pub content: String,
}

/// Lifecycle phase of a tool invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum ToolPhase {
    /// Tool call started.
    Start,
    /// Tool call finished.
    Done,
}

/// `tool` event payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ToolData {
    /// Lifecycle phase.
    pub phase: ToolPhase,
    /// Tool identifier.
    pub name: String,
    /// Tool-specific metadata.
    pub details: serde_json::Value,
}

/// A character span within response text.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct TextSpan {
    /// Start offset.
    pub start: u32,
    /// End offset.
    pub end: u32,
}

/// Whether a citation came from a file or web search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum CitationSource {
    /// File citation.
    File,
    /// Web citation.
    Web,
}

/// A citation extracted from provider annotations.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct Citation {
    /// `Citation` source type.
    pub source: CitationSource,
    /// `Citation` title (filename or page title).
    pub title: String,
    /// Excerpt.
    pub snippet: String,
    /// URL for web sources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Internal attachment identifier for file sources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    /// Character span in the response text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TextSpan>,
    /// Relevance score (never populated in P1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// `citations` event payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CitationsData {
    /// Citations.
    pub items: Vec<Citation>,
}

/// Token usage counters.
#[derive(Debug, Clone, Copy)]
#[toolkit_macros::api_dto(response)]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: i64,
    /// Output tokens.
    pub output_tokens: i64,
}

/// Quota decision reported in `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub enum QuotaDecisionKind {
    /// The turn used the selected model.
    Allow,
    /// A quota-driven downgrade occurred.
    Downgrade,
}

/// `done` event payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct DoneData {
    /// Token usage (always present).
    pub usage: Usage,
    /// Model actually used for this turn.
    pub effective_model: String,
    /// Model chosen at chat creation.
    pub selected_model: String,
    /// Quota decision.
    pub quota_decision: QuotaDecisionKind,
    /// Model downgraded from (only when `quota_decision=downgrade`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    /// Why the downgrade occurred (only when `quota_decision=downgrade`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    /// Per-tier quota status (CAS-winning completed/incomplete turns only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

/// `error` event payload.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ErrorData {
    /// Stable streaming error code.
    pub code: String,
    /// Human-readable, sanitized description.
    pub message: String,
}

/// One SSE event of the streaming endpoints (documentation type).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
#[serde(tag = "event", content = "data")]
pub enum MiniChatSseEvent {
    /// Stream header.
    #[serde(rename = "stream_started")]
    StreamStarted(StreamStartedData),
    /// Keepalive.
    #[serde(rename = "ping")]
    Ping(PingData),
    /// Incremental output.
    #[serde(rename = "delta")]
    Delta(DeltaData),
    /// Tool activity.
    #[serde(rename = "tool")]
    Tool(ToolData),
    /// Citations.
    #[serde(rename = "citations")]
    Citations(CitationsData),
    /// Successful completion.
    #[serde(rename = "done")]
    Done(DoneData),
    /// Terminal error.
    #[serde(rename = "error")]
    Error(ErrorData),
}
