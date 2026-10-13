//! REST DTOs (`OpenAPI` subset). Optional response fields are omitted when absent.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use mini_chat_sdk::{ModelCatalogEntry, ModelTier};
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{PeriodType, QuotaDecision, TurnState};
use crate::domain::services::{
    ChatView, MessageView, PeriodStatus, QuotaStatus, QuotaTier as DomainQuotaTier, TierStatus,
};
use crate::infra::db::entities::{attachment, chat_turn, message_reaction};
use crate::infra::db::repos::attachment::{
    AttachmentSummary, ImgThumbnail, THUMBNAIL_CONTENT_TYPE,
};

/// Response DTO for chat details.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ChatDetailDto {
    pub id: Uuid,
    pub model: String,
    /// Omitted when the chat has no title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<ChatView> for ChatDetailDto {
    fn from(v: ChatView) -> Self {
        let c = v.chat;
        Self {
            id: c.id,
            model: c.model.unwrap_or_default(),
            title: c.title,
            is_temporary: c.is_temporary,
            message_count: v.message_count,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }
}

/// Request DTO for creating a new chat.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone, Default)]
pub struct CreateChatReq {
    /// Optional; trimmed, 1–255 characters.
    #[serde(default)]
    pub title: Option<String>,
    /// Optional; defaults to the catalog's default model.
    #[serde(default)]
    pub model: Option<String>,
}

/// Request DTO for updating a chat title. Unknown fields are ignored.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct UpdateChatReq {
    pub title: String,
}

/// `$filter` / `$orderby` fields of `GET /v1/chats` (`OpenAPI` documentation only;
/// the query itself is compiled by `infra::db::repos::chat`).
#[derive(toolkit_odata_macros::ODataFilterable)]
#[allow(dead_code)]
pub struct ChatQuery {
    #[odata(filter(kind = "DateTimeUtc"))]
    pub updated_at: DateTime<Utc>,
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    #[odata(filter(kind = "String"))]
    pub title: String,
}

/// `$filter` / `$orderby` fields of `GET /v1/chats/{id}/messages` (`OpenAPI`
/// documentation only; the query itself is compiled by `infra::db::repos::message`).
#[derive(toolkit_odata_macros::ODataFilterable)]
#[allow(dead_code)]
pub struct MessageQuery {
    #[odata(filter(kind = "DateTimeUtc"))]
    pub created_at: DateTime<Utc>,
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    #[odata(filter(kind = "String"))]
    pub role: String,
}

/// Declares a response enum whose wire values are the `OpenAPI` string enum values
/// (`api_dto` renders variants in `snake_case`), with a fallible conversion from
/// the persisted string (an unknown stored value is an internal error).
macro_rules! string_dto_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[toolkit_macros::api_dto(response)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name {
            $($variant),+
        }

        impl TryFrom<&str> for $name {
            type Error = DomainError;

            fn try_from(s: &str) -> Result<Self, Self::Error> {
                match s {
                    $($s => Ok(Self::$variant),)+
                    other => Err(DomainError::internal(format!(
                        "unknown {} value `{other}`",
                        stringify!($name)
                    ))),
                }
            }
        }
    };
}

string_dto_enum! {
    /// Message author role.
    MessageRoleDto { User => "user", Assistant => "assistant", System => "system" }
}

string_dto_enum! {
    /// Attachment kind.
    AttachmentKindDto { Document => "document", Image => "image" }
}

string_dto_enum! {
    /// Attachment lifecycle status.
    AttachmentStatusDto {
        Pending => "pending",
        Uploaded => "uploaded",
        Ready => "ready",
        Failed => "failed",
    }
}

string_dto_enum! {
    /// Reaction value.
    ReactionKindDto { Like => "like", Dislike => "dislike" }
}

/// Model tier.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Quota period classification.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaPeriod {
    Daily,
    Monthly,
}

impl From<PeriodType> for QuotaPeriod {
    fn from(p: PeriodType) -> Self {
        match p {
            PeriodType::Daily => Self::Daily,
            PeriodType::Monthly => Self::Monthly,
        }
    }
}

/// Quota tier classification.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaTier {
    Premium,
    Total,
}

impl From<DomainQuotaTier> for QuotaTier {
    fn from(t: DomainQuotaTier) -> Self {
        match t {
            DomainQuotaTier::Premium => Self::Premium,
            DomainQuotaTier::Total => Self::Total,
        }
    }
}

/// Server-generated preview thumbnail of an image attachment.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ImgThumbnailDto {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data_base64: String,
}

impl From<ImgThumbnail> for ImgThumbnailDto {
    fn from(t: ImgThumbnail) -> Self {
        Self {
            content_type: THUMBNAIL_CONTENT_TYPE.to_owned(),
            width: t.width,
            height: t.height,
            data_base64: STANDARD.encode(t.data),
        }
    }
}

/// Lightweight attachment metadata embedded in message responses.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct AttachmentSummaryDto {
    pub attachment_id: Uuid,
    pub kind: AttachmentKindDto,
    pub filename: String,
    pub status: AttachmentStatusDto,
    /// Only for images with `status = ready` that have a thumbnail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

impl TryFrom<AttachmentSummary> for AttachmentSummaryDto {
    type Error = DomainError;

    fn try_from(a: AttachmentSummary) -> Result<Self, Self::Error> {
        Ok(Self {
            attachment_id: a.attachment_id,
            kind: a.kind.as_str().try_into()?,
            filename: a.filename,
            status: a.status.as_str().try_into()?,
            img_thumbnail: a.img_thumbnail.map(Into::into),
        })
    }
}

/// Full attachment details (`GET` / upload response). Optional fields are
/// omitted when null: `error_code` only for `failed`, `img_thumbnail` only for a
/// ready image with a thumbnail; `doc_summary` / `summary_updated_at` are never
/// populated (ADR-0007).
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_updated_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

impl TryFrom<attachment::Model> for AttachmentDetailDto {
    type Error = DomainError;

    fn try_from(a: attachment::Model) -> Result<Self, Self::Error> {
        let status = AttachmentStatusDto::try_from(a.status.as_str())?;
        let kind = AttachmentKindDto::try_from(a.attachment_kind.as_str())?;
        let img_thumbnail = match (
            status == AttachmentStatusDto::Ready && kind == AttachmentKindDto::Image,
            a.img_thumbnail,
            a.img_thumbnail_width,
            a.img_thumbnail_height,
        ) {
            (true, Some(data), Some(width), Some(height)) => Some(
                ImgThumbnail {
                    width,
                    height,
                    data,
                }
                .into(),
            ),
            _ => None,
        };
        Ok(Self {
            id: a.id,
            filename: a.filename,
            content_type: a.content_type.unwrap_or_default(),
            size_bytes: a.size_bytes.unwrap_or(0),
            status,
            kind,
            error_code: a
                .error_code
                .filter(|_| status == AttachmentStatusDto::Failed),
            doc_summary: None,
            img_thumbnail,
            summary_updated_at: None,
            created_at: a.created_at,
        })
    }
}

/// Response DTO for a message in the list endpoint (`MiniChatMessageDto`).
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct MiniChatMessageDto {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: MessageRoleDto,
    pub content: String,
    /// Absent for user messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Omitted when 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    /// Omitted when 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    /// Always present.
    pub attachments: Vec<AttachmentSummaryDto>,
    /// Always present, `null` when the caller has no reaction.
    pub my_reaction: Option<ReactionKindDto>,
    pub created_at: DateTime<Utc>,
}

impl TryFrom<MessageView> for MiniChatMessageDto {
    type Error = DomainError;

    /// # Errors
    /// `Internal` for a stored message without `request_id` (never serialized as a
    /// nil UUID) or with an unknown role, attachment kind/status or reaction.
    fn try_from(v: MessageView) -> Result<Self, Self::Error> {
        let m = v.message;
        let request_id = m
            .request_id
            .ok_or_else(|| DomainError::internal(format!("message {} has no request_id", m.id)))?;
        let non_zero = |n: i64| (n != 0).then_some(n);
        Ok(Self {
            id: m.id,
            request_id,
            role: m.role.as_str().try_into()?,
            content: m.content,
            model: m.model,
            input_tokens: non_zero(m.input_tokens),
            output_tokens: non_zero(m.output_tokens),
            attachments: v
                .attachments
                .into_iter()
                .map(AttachmentSummaryDto::try_from)
                .collect::<Result<_, _>>()?,
            my_reaction: v
                .my_reaction
                .as_deref()
                .map(ReactionKindDto::try_from)
                .transpose()?,
            created_at: m.created_at,
        })
    }
}

/// Request DTO for setting a reaction (`like` | `dislike`, validated by the service).
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
    pub created_at: DateTime<Utc>,
}

impl TryFrom<message_reaction::Model> for MiniChatReactionDto {
    type Error = DomainError;

    fn try_from(r: message_reaction::Model) -> Result<Self, Self::Error> {
        Ok(Self {
            message_id: r.message_id,
            reaction: r.reaction.as_str().try_into()?,
            created_at: r.created_at,
        })
    }
}

/// Response DTO for a single model; no provider or credit internals.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ModelDto {
    pub model_id: String,
    pub display_name: String,
    pub tier: ModelTierDto,
    pub multiplier_display: String,
    /// Omitted when no description is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub multimodal_capabilities: Vec<String>,
    pub context_window: u32,
}

impl From<ModelCatalogEntry> for ModelDto {
    fn from(m: ModelCatalogEntry) -> Self {
        Self {
            model_id: m.id,
            display_name: m.display_name,
            tier: m.tier.into(),
            multiplier_display: m.multiplier_display,
            description: Some(m.description).filter(|d| !d.is_empty()),
            multimodal_capabilities: m.multimodal_capabilities,
            context_window: m.context_window,
        }
    }
}

/// Response DTO for the model list endpoint.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct ModelListDto {
    pub items: Vec<ModelDto>,
}

/// Quota of one period (credits in micro-credits).
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaPeriodStatus {
    pub period: QuotaPeriod,
    pub limit_credits_micro: i64,
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u8,
    pub next_reset: DateTime<Utc>,
    pub warning: bool,
    pub exhausted: bool,
}

impl From<PeriodStatus> for QuotaPeriodStatus {
    fn from(p: PeriodStatus) -> Self {
        Self {
            period: p.period.into(),
            limit_credits_micro: p.limit,
            used_credits_micro: p.used,
            remaining_credits_micro: p.remaining,
            remaining_percentage: p.remaining_percentage,
            next_reset: p.next_reset,
            warning: p.warning,
            exhausted: p.exhausted,
        }
    }
}

/// Quota periods of one tier.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaTierStatus {
    pub tier: QuotaTier,
    pub periods: Vec<QuotaPeriodStatus>,
}

impl From<TierStatus> for QuotaTierStatus {
    fn from(t: TierStatus) -> Self {
        Self {
            tier: t.tier.into(),
            periods: t.periods.into_iter().map(Into::into).collect(),
        }
    }
}

/// Response DTO of `GET /v1/quota/status`.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u8,
}

impl From<QuotaStatus> for QuotaStatusResponse {
    fn from(s: QuotaStatus) -> Self {
        Self {
            tiers: s.tiers.into_iter().map(Into::into).collect(),
            warning_threshold_pct: s.warning_threshold_pct,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Streaming (`messages:stream`) and turn status
// ---------------------------------------------------------------------------------------------

/// Web search toggle of a send request.
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone, Copy, Default)]
pub struct WebSearchConfig {
    pub enabled: bool,
}

/// Request body of `POST /v1/chats/{id}/messages:stream` (unknown fields ignored).
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct StreamMessageRequest {
    /// Message content (must be non-empty after trimming).
    pub content: String,
    /// Idempotency key (any UUID); generated by the server when omitted.
    #[serde(default)]
    pub request_id: Option<Uuid>,
    /// Attachments referenced by the message.
    #[serde(default)]
    pub attachment_ids: Vec<Uuid>,
    /// Web search configuration (`{enabled: false}` when omitted).
    #[serde(default)]
    pub web_search: Option<WebSearchConfig>,
}

/// Request DTO for `PATCH /chats/{id}/turns/{request_id}` (edit).
#[toolkit_macros::api_dto(request)]
#[derive(Debug, Clone)]
pub struct EditTurnRequest {
    /// Replacement user message (must be non-empty after trimming).
    pub content: String,
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

impl From<TurnState> for TurnStatusState {
    fn from(s: TurnState) -> Self {
        match s {
            TurnState::Running => Self::Running,
            TurnState::Completed => Self::Done,
            TurnState::Failed => Self::Error,
            TurnState::Cancelled => Self::Cancelled,
        }
    }
}

/// Response DTO of `GET /v1/chats/{id}/turns/{request_id}`.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone)]
pub struct TurnStatusResponse {
    pub request_id: Uuid,
    pub state: TurnStatusState,
    /// Terminal error code; only for `error`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Persisted assistant message; `done`, and `cancelled` with partial content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: DateTime<Utc>,
}

impl TryFrom<chat_turn::Model> for TurnStatusResponse {
    type Error = DomainError;

    fn try_from(t: chat_turn::Model) -> Result<Self, Self::Error> {
        let state = TurnState::parse(&t.state)
            .ok_or_else(|| DomainError::internal(format!("unknown turn state `{}`", t.state)))?;
        Ok(Self {
            request_id: t.request_id,
            state: state.into(),
            error_code: t.error_code.filter(|_| state == TurnState::Failed),
            assistant_message_id: t
                .assistant_message_id
                .filter(|_| matches!(state, TurnState::Completed | TurnState::Cancelled)),
            updated_at: t.updated_at.unwrap_or(t.started_at),
        })
    }
}

/// Metadata about a thread summary applied to the turn's context.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadSummaryInfo {
    /// Estimated token cost of the summary in the context window.
    pub token_estimate: u32,
}

/// `stream_started`: first event of every stream.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    /// `false` for an idempotent replay of a completed turn.
    pub is_new_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

/// Payload of `ping`: an empty JSON object (a unit struct would serialize as
/// `null`).
#[allow(
    clippy::empty_structs_with_brackets,
    reason = "serializes as `{}`, documented as an object"
)]
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PingData {}

/// Kind of a `delta` chunk.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// `delta`: incremental assistant output.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// `tool`: tool activity.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, PartialEq)]
pub struct ToolData {
    pub phase: ToolPhase,
    pub name: String,
    #[schema(value_type = Object)]
    pub details: serde_json::Value,
}

/// Whether a citation came from a file or web search.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CitationSource {
    File,
    Web,
}

/// A character span within the response text.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSpan {
    pub start: u32,
    pub end: u32,
}

/// A citation extracted from provider annotations.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, PartialEq)]
pub struct Citation {
    pub source: CitationSource,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The attachment id (never the provider file id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TextSpan>,
    /// Not populated in P1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// `citations`: source references, sent once before `done`.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, PartialEq)]
pub struct CitationsData {
    pub items: Vec<Citation>,
}

/// Token usage counters of `done`.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

impl From<QuotaDecision> for QuotaDecisionKind {
    fn from(d: QuotaDecision) -> Self {
        match d {
            QuotaDecision::Allow => Self::Allow,
            QuotaDecision::Downgrade => Self::Downgrade,
        }
    }
}

/// Per-tier, per-period quota entry of `done`.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaWarning {
    pub tier: QuotaTier,
    pub period: QuotaPeriod,
    pub remaining_percentage: u8,
    pub warning: bool,
    pub exhausted: bool,
    /// Present only when `warning` or `exhausted`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_reset: Option<DateTime<Utc>>,
}

impl QuotaWarning {
    /// One entry per period of each tier.
    #[must_use]
    pub fn from_status(tiers: Vec<TierStatus>) -> Vec<Self> {
        tiers
            .into_iter()
            .flat_map(|t| {
                let tier = QuotaTier::from(t.tier);
                t.periods.into_iter().map(move |p| Self {
                    tier,
                    period: p.period.into(),
                    remaining_percentage: p.remaining_percentage,
                    warning: p.warning,
                    exhausted: p.exhausted,
                    next_reset: (p.warning || p.exhausted).then_some(p.next_reset),
                })
            })
            .collect()
    }
}

/// `done`: successful completion.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoneData {
    pub usage: Usage,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: QuotaDecisionKind,
    /// Equals `selected_model`; only on a downgrade.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    /// Only on a live downgrade (omitted on replay).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    /// Present on CAS-winning completed turns only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

/// `error`: terminal stream error.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
}

/// One SSE event of the `messages:stream`, retry and edit responses: sent as
/// `event: <name>` and `data: <payload JSON>` (see `api::rest::sse`).
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, PartialEq)]
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
