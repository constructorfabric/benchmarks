//! REST DTOs (wire shapes of `docs/api/api.json`).

use mini_chat_sdk::{ModelCatalogEntry, ModelTier};
use time::OffsetDateTime;
use toolkit_odata_macros::ODataFilterable;
use uuid::Uuid;

use crate::domain::models::ChatDetail;
use crate::domain::services::quota_service::{self, Bucket, Period, PeriodStatus, QuotaStatus};

/// Model tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ModelTierDto {
    Standard,
    Premium,
}

impl From<ModelTier> for ModelTierDto {
    fn from(t: ModelTier) -> Self {
        match t {
            ModelTier::Standard => Self::Standard,
            ModelTier::Premium => Self::Premium,
        }
    }
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
            tier: m.tier.into(),
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

/// Response DTO for chat details.
///
/// `OData` list fields: `updated_at`, `id`, `title` (`$filter` / `$orderby`).
#[derive(Debug, Clone, ODataFilterable)]
#[toolkit_macros::api_dto(response)]
pub struct ChatDetailDto {
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    pub model: String,
    /// Omitted when the chat has no title.
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

impl From<ChatDetail> for ChatDetailDto {
    fn from(c: ChatDetail) -> Self {
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

/// Request DTO for creating a new chat.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct CreateChatReq {
    pub title: Option<String>,
    pub model: Option<String>,
}

/// Request DTO for updating a chat title.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct UpdateChatReq {
    pub title: String,
}

/// Quota tier of the status / warnings (`premium`, `total`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaTier {
    Premium,
    Total,
}

impl From<Bucket> for QuotaTier {
    fn from(b: Bucket) -> Self {
        match b {
            Bucket::Premium => Self::Premium,
            Bucket::Total => Self::Total,
        }
    }
}

/// Quota period (`daily`, `monthly`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaPeriod {
    Daily,
    Monthly,
}

impl From<Period> for QuotaPeriod {
    fn from(p: Period) -> Self {
        match p {
            Period::Daily => Self::Daily,
            Period::Monthly => Self::Monthly,
        }
    }
}

/// One period of a tier in the quota status.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaPeriodStatus {
    pub period: QuotaPeriod,
    pub limit_credits_micro: i64,
    /// `spent + reserved`.
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

impl From<PeriodStatus> for QuotaPeriodStatus {
    fn from(p: PeriodStatus) -> Self {
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

/// Quota status of one tier.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatus {
    pub tier: QuotaTier,
    pub periods: Vec<QuotaPeriodStatus>,
}

/// `GET /v1/quota/status` response.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u32,
}

impl From<QuotaStatus> for QuotaStatusResponse {
    fn from(s: QuotaStatus) -> Self {
        Self {
            tiers: s
                .tiers
                .into_iter()
                .map(|t| QuotaTierStatus {
                    tier: t.tier.into(),
                    periods: t.periods.into_iter().map(Into::into).collect(),
                })
                .collect(),
            warning_threshold_pct: u32::from(s.warning_threshold_pct),
        }
    }
}

/// Per-tier, per-period quota warning entry of the SSE `done` event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaWarning {
    pub tier: QuotaTier,
    pub period: QuotaPeriod,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    /// Present only when `warning` or `exhausted`.
    #[serde(
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub next_reset: Option<OffsetDateTime>,
}

impl From<quota_service::QuotaWarning> for QuotaWarning {
    fn from(w: quota_service::QuotaWarning) -> Self {
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

// ---------------------------------------------------------------------------
// Streaming (`POST /chats/{id}/messages:stream`) and its SSE events
// ---------------------------------------------------------------------------

/// Web search toggle.
#[derive(Debug, Clone)]
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
    pub request_id: Option<Uuid>,
    /// Attachment IDs to include.
    #[serde(default)]
    pub attachment_ids: Vec<Uuid>,
    /// Web search configuration.
    pub web_search: Option<WebSearchConfig>,
}

/// Request DTO for `PATCH /chats/{id}/turns/{request_id}` (edit).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct EditTurnRequest {
    /// Replacement user message (must be non-empty after trim).
    pub content: String,
}

/// Metadata about a thread summary applied to the current turn's context.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ThreadSummaryInfo {
    /// Estimated token cost of the summary in the context window.
    pub token_estimate: u32,
}

/// First event of every SSE stream (new generations and replays).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    /// Pre-allocated (new turn) or persisted (replay) assistant message id.
    pub message_id: Uuid,
    /// `false` when the stream replays an already completed turn.
    pub is_new_turn: bool,
    /// Present when a thread summary is included in the context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

/// Payload of the `ping` event: an empty JSON object (a unit struct would
/// be documented as `null`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
#[allow(
    clippy::empty_structs_with_brackets,
    reason = "serializes as `{}`, a unit struct would be `null`"
)]
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
#[derive(Debug, Clone, PartialEq)]
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

/// A citation extracted from provider annotations (provider ids never
/// appear; file citations carry the attachment id and filename).
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
    /// Not populated in P1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TextSpan>,
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

/// Successful stream completion.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct DoneData {
    pub usage: Usage,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: QuotaDecisionKind,
    /// Equals `selected_model`; only on a downgrade.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    /// Only on a live downgraded turn (never on replay).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    /// Only on CAS-winning completed turns.
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
/// type only documents the wire format; `api::rest::sse` writes it.
#[derive(Debug, Clone)]
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

// ---------------------------------------------------------------------------
// Messages list
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
#[toolkit_macros::api_dto(request, response)]
pub enum ReactionKindDto {
    Like,
    Dislike,
}

/// Request DTO for `PUT /chats/{id}/messages/{msg_id}/reaction`. The value is
/// a plain string: an unsupported value is 400 `INVALID_REACTION` (checked
/// before authorization), only a missing / non-string field is 422.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct SetReactionReq {
    /// `like` or `dislike`.
    pub reaction: String,
}

/// Response DTO for a reaction.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatReactionDto {
    pub message_id: Uuid,
    pub reaction: ReactionKindDto,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

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

/// Server-generated preview thumbnail for an image attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ImgThumbnailDto {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data_base64: String,
}

/// Lightweight attachment metadata embedded in Message responses.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentSummaryDto {
    pub attachment_id: Uuid,
    pub kind: AttachmentKindDto,
    pub filename: String,
    pub status: AttachmentStatusDto,
    /// Only for ready images.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
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
    /// Only for `failed` attachments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Never populated (ADR-0007).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_summary: Option<String>,
    /// Only for ready images.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
    /// Never populated (ADR-0007).
    #[serde(
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub summary_updated_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// Response DTO for a message in the list endpoint (`MiniChatMessageDto`:
/// another gear exposes a `MessageDto` in the same `OpenAPI` registry).
///
/// `OData` list fields: `created_at`, `id`, `role`.
#[derive(Debug, Clone, ODataFilterable)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatMessageDto {
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    pub request_id: Uuid,
    #[odata(filter(kind = "String"))]
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
    #[odata(filter(kind = "DateTimeUtc"))]
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------
// Turn status
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

fn attachment_kind_dto(kind: &str) -> AttachmentKindDto {
    if kind == "image" {
        AttachmentKindDto::Image
    } else {
        AttachmentKindDto::Document
    }
}

fn attachment_status_dto(status: &str) -> AttachmentStatusDto {
    match status {
        "pending" => AttachmentStatusDto::Pending,
        "uploaded" => AttachmentStatusDto::Uploaded,
        "ready" => AttachmentStatusDto::Ready,
        _ => AttachmentStatusDto::Failed,
    }
}

fn thumbnail_dto((bytes, width, height): (Vec<u8>, i32, i32)) -> ImgThumbnailDto {
    use base64::Engine as _;
    ImgThumbnailDto {
        content_type: "image/webp".to_owned(),
        width,
        height,
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    }
}

impl From<crate::domain::models::AttachmentSummary> for AttachmentSummaryDto {
    fn from(a: crate::domain::models::AttachmentSummary) -> Self {
        Self {
            attachment_id: a.attachment_id,
            kind: attachment_kind_dto(&a.kind),
            filename: a.filename,
            status: attachment_status_dto(&a.status),
            img_thumbnail: a.thumbnail.map(thumbnail_dto),
        }
    }
}

impl From<crate::domain::models::AttachmentDetail> for AttachmentDetailDto {
    fn from(a: crate::domain::models::AttachmentDetail) -> Self {
        Self {
            id: a.id,
            filename: a.filename,
            content_type: a.content_type,
            size_bytes: a.size_bytes,
            status: attachment_status_dto(&a.status),
            kind: attachment_kind_dto(&a.kind),
            error_code: a.error_code,
            doc_summary: None,
            img_thumbnail: a.thumbnail.map(thumbnail_dto),
            summary_updated_at: None,
            created_at: a.created_at,
        }
    }
}

fn reaction_kind_dto(reaction: &str) -> Option<ReactionKindDto> {
    match reaction {
        "like" => Some(ReactionKindDto::Like),
        "dislike" => Some(ReactionKindDto::Dislike),
        _ => None,
    }
}

impl TryFrom<crate::domain::models::ReactionView> for MiniChatReactionDto {
    type Error = crate::domain::error::DomainError;

    fn try_from(r: crate::domain::models::ReactionView) -> Result<Self, Self::Error> {
        let reaction = reaction_kind_dto(&r.reaction).ok_or_else(|| {
            crate::domain::error::DomainError::Internal(format!(
                "stored reaction `{}` is not like/dislike",
                r.reaction
            ))
        })?;
        Ok(Self {
            message_id: r.message_id,
            reaction,
            created_at: r.created_at,
        })
    }
}

impl From<crate::domain::models::MessageView> for MiniChatMessageDto {
    fn from(m: crate::domain::models::MessageView) -> Self {
        let nonzero = |v: i64| (v != 0).then_some(v);
        Self {
            id: m.id,
            request_id: m.request_id,
            role: match m.role.as_str() {
                "assistant" => MessageRoleDto::Assistant,
                "system" => MessageRoleDto::System,
                _ => MessageRoleDto::User,
            },
            content: m.content,
            attachments: m.attachments.into_iter().map(Into::into).collect(),
            my_reaction: m.my_reaction.as_deref().and_then(reaction_kind_dto),
            model: m.model,
            input_tokens: nonzero(m.input_tokens),
            output_tokens: nonzero(m.output_tokens),
            created_at: m.created_at,
        }
    }
}

impl From<crate::domain::models::TurnStatusView> for TurnStatusResponse {
    fn from(t: crate::domain::models::TurnStatusView) -> Self {
        use crate::domain::models::TurnStatusState as S;
        Self {
            request_id: t.request_id,
            state: match t.state {
                S::Running => TurnStatusState::Running,
                S::Done => TurnStatusState::Done,
                S::Error => TurnStatusState::Error,
                S::Cancelled => TurnStatusState::Cancelled,
            },
            error_code: t.error_code,
            assistant_message_id: t.assistant_message_id,
            updated_at: t.updated_at,
        }
    }
}
