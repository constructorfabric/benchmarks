//! REST DTOs (the generated `OpenAPI` `docs/api/api.json` is the reference).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Request DTO for creating a new chat.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone, Default)]
#[schema(as = CreateChatReq)]
pub struct CreateChatReq {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

/// Request DTO for updating a chat title.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct UpdateChatReq {
    pub title: String,
}

/// Response DTO for chat details.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
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

/// Full attachment details returned by the GET attachment endpoint.
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
        with = "time::serde::rfc3339::option"
    )]
    pub summary_updated_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Message author role.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRoleDto {
    User,
    Assistant,
    System,
}

/// Reaction value.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionKindDto {
    Like,
    Dislike,
}

/// Response DTO for a message in the list endpoint.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
#[schema(as = MiniChatMessageDto)]
pub struct MessageDto {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: MessageRoleDto,
    pub content: String,
    pub attachments: Vec<AttachmentSummaryDto>,
    /// The caller's reaction to this message; `null` when there is none.
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

/// Request DTO for setting a reaction.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct SetReactionReq {
    pub reaction: String,
}

/// Response DTO for a reaction.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
#[schema(as = MiniChatReactionDto)]
pub struct ReactionDto {
    pub message_id: Uuid,
    pub reaction: ReactionKindDto,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

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

/// Quota tier classification.
#[toolkit_macros::api_dto(request, response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaTier {
    Premium,
    Total,
}

/// Quota period classification.
#[toolkit_macros::api_dto(request, response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaPeriod {
    Daily,
    Monthly,
}

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

#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaTierStatus {
    pub tier: QuotaTier,
    pub periods: Vec<QuotaPeriodStatus>,
}

#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u32,
}

/// Web search toggle.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone, Copy, Default)]
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

/// Request DTO for `PATCH /chats/{id}/turns/{request_id}` (edit).
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct EditTurnRequest {
    pub content: String,
}

// ───────────────────────────── SSE payloads ─────────────────────────────

/// Metadata about a thread summary applied to the current turn's context.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ThreadSummaryInfo {
    /// Estimated token cost of the summary in the context window.
    pub token_estimate: u32,
}

/// Stream header event carrying the stream request ID and server-generated assistant message ID.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    /// `true` for a live generation; `false` for an idempotent replay.
    pub is_new_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

/// Payload of the `ping` event: an empty JSON object.
// Braces are load-bearing: serde serializes a unit struct as `null`, but the wire contract is `{}`.
#[allow(clippy::empty_structs_with_brackets)]
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PingData {}

/// Kind of a `delta` chunk.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// Delta text chunk.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DeltaData {
    #[serde(rename = "type")]
    pub kind: DeltaKind,
    pub content: String,
}

/// Lifecycle phase of a tool invocation within a stream.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolPhase {
    Start,
    Done,
}

/// Tool lifecycle event.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ToolData {
    pub phase: ToolPhase,
    pub name: String,
    #[schema(value_type = Object)]
    pub details: serde_json::Value,
}

/// Whether a citation came from a file or web search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CitationSource {
    File,
    Web,
}

/// A character span within response text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct TextSpan {
    pub start: usize,
    pub end: usize,
}

/// A citation extracted from provider annotations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CitationsData {
    pub items: Vec<Citation>,
}

/// Token usage counters.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Quota decision reported in `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuotaDecisionKind {
    Allow,
    Downgrade,
}

/// Per-tier, per-period quota warning entry in the SSE `done` event.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct QuotaWarning {
    pub tier: QuotaTier,
    pub period: QuotaPeriod,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    /// RFC 3339 timestamp of the next quota-period reset (only with `warning` or `exhausted`).
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option",
        default
    )]
    pub next_reset: Option<OffsetDateTime>,
}

/// Successful stream completion.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
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

/// Stream error (terminal).
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
}

/// `OpenAPI` description of one SSE event of the `messages:stream`, retry and edit responses.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum MiniChatSseEvent {
    StreamStarted(StreamStartedData),
    Ping(PingData),
    Delta(DeltaData),
    Tool(ToolData),
    Citations(CitationsData),
    Done(DoneData),
    Error(ErrorData),
}

impl toolkit::api::api_dto::ResponseApiDto for MiniChatSseEvent {}
