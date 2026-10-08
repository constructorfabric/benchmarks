//! REST / SSE DTOs.

use base64::Engine as _;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::service::chats::ChatView;
use crate::domain::service::messages::{AttachmentSummary, MessageView, ReactionView};
use crate::domain::service::quota::TierStatus;
use crate::domain::service::stream::{
    Citation as DomainCitation, DoneInfo, QuotaWarning as DomainWarning,
};
use crate::domain::service::turns::TurnStatus;
use crate::infra::storage::entity::attachment;

/// RFC 3339 with six fractional digits (serde helper).
pub mod ts {
    use serde::Serializer;
    use time::OffsetDateTime;

    /// Serialize a timestamp.
    ///
    /// # Errors
    /// Serializer errors.
    pub fn serialize<S: Serializer>(ts: &OffsetDateTime, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&crate::domain::clock::format_api(*ts))
    }

    /// Optional variant.
    pub mod option {
        use serde::Serializer;
        use time::OffsetDateTime;

        /// Serialize an optional timestamp.
        ///
        /// # Errors
        /// Serializer errors.
        #[allow(clippy::ref_option)]
        pub fn serialize<S: Serializer>(
            ts: &Option<OffsetDateTime>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match ts {
                Some(t) => s.serialize_str(&crate::domain::clock::format_api(*t)),
                None => s.serialize_none(),
            }
        }
    }
}

// ---------------------------------------------------------------- chats --

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
    #[serde(serialize_with = "ts::serialize")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
    #[serde(serialize_with = "ts::serialize")]
    #[schema(value_type = String, format = DateTime)]
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

// ------------------------------------------------------------ messages --

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

fn kind_dto(s: &str) -> AttachmentKindDto {
    if s == "image" {
        AttachmentKindDto::Image
    } else {
        AttachmentKindDto::Document
    }
}

fn status_dto(s: &str) -> AttachmentStatusDto {
    match s {
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

fn thumb_dto(t: (Vec<u8>, i32, i32)) -> ImgThumbnailDto {
    ImgThumbnailDto {
        content_type: "image/webp".to_owned(),
        width: t.1,
        height: t.2,
        data_base64: base64::engine::general_purpose::STANDARD.encode(t.0),
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

impl From<AttachmentSummary> for AttachmentSummaryDto {
    fn from(a: AttachmentSummary) -> Self {
        Self {
            attachment_id: a.attachment_id,
            kind: kind_dto(&a.kind),
            filename: a.filename,
            status: status_dto(&a.status),
            img_thumbnail: a.thumbnail.map(thumb_dto),
        }
    }
}

/// Response DTO for a message in the list endpoint.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatMessageDto {
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
    #[serde(serialize_with = "ts::serialize")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<MessageView> for MiniChatMessageDto {
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
            attachments: v.attachments.into_iter().map(Into::into).collect(),
            my_reaction: v.my_reaction.as_deref().and_then(ReactionKindDto::parse),
            model: if role == MessageRoleDto::Assistant {
                m.model
            } else {
                None
            },
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
    pub reaction: ReactionKindDto,
    #[serde(serialize_with = "ts::serialize")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<ReactionView> for MiniChatReactionDto {
    fn from(r: ReactionView) -> Self {
        Self {
            message_id: r.message_id,
            reaction: ReactionKindDto::parse(&r.reaction).unwrap_or(ReactionKindDto::Like),
            created_at: r.created_at,
        }
    }
}

// --------------------------------------------------------- attachments --

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
        serialize_with = "ts::option::serialize"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub summary_updated_at: Option<OffsetDateTime>,
    #[serde(serialize_with = "ts::serialize")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<attachment::Model> for AttachmentDetailDto {
    fn from(a: attachment::Model) -> Self {
        let summary = crate::domain::service::messages::attachment_summary(&a);
        Self {
            id: a.id,
            filename: a.filename,
            content_type: a.content_type,
            size_bytes: a.size_bytes,
            status: status_dto(&a.status),
            kind: kind_dto(&a.attachment_kind),
            error_code: if a.status == "failed" {
                a.error_code
            } else {
                None
            },
            doc_summary: None,
            img_thumbnail: summary.thumbnail.map(thumb_dto),
            summary_updated_at: None,
            created_at: a.created_at,
        }
    }
}

// --------------------------------------------------------------- models --

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

impl From<mini_chat_sdk::ModelCatalogEntry> for ModelDto {
    fn from(m: mini_chat_sdk::ModelCatalogEntry) -> Self {
        Self {
            model_id: m.id,
            display_name: m.display_name,
            tier: match m.tier {
                mini_chat_sdk::ModelTier::Premium => ModelTierDto::Premium,
                mini_chat_sdk::ModelTier::Standard => ModelTierDto::Standard,
            },
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

// ---------------------------------------------------------------- quota --

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
    if p == "daily" {
        QuotaPeriod::Daily
    } else {
        QuotaPeriod::Monthly
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
    #[serde(serialize_with = "ts::serialize")]
    #[schema(value_type = String, format = DateTime)]
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
    pub fn new(rows: Vec<TierStatus>, threshold: u8) -> Self {
        Self {
            tiers: rows
                .into_iter()
                .map(|t| QuotaTierStatus {
                    tier: tier_dto(t.tier),
                    periods: t
                        .periods
                        .into_iter()
                        .map(|p| QuotaPeriodStatus {
                            period: period_dto(p.period),
                            limit_credits_micro: p.limit,
                            used_credits_micro: p.used,
                            remaining_credits_micro: p.remaining,
                            remaining_percentage: u32::from(p.remaining_percentage),
                            next_reset: p.next_reset,
                            warning: p.warning,
                            exhausted: p.exhausted,
                        })
                        .collect(),
                })
                .collect(),
            warning_threshold_pct: u32::from(threshold),
        }
    }
}

// ---------------------------------------------------------------- turns --

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
    #[serde(serialize_with = "ts::serialize")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: OffsetDateTime,
}

impl From<TurnStatus> for TurnStatusResponse {
    fn from(t: TurnStatus) -> Self {
        Self {
            request_id: t.request_id,
            state: match t.state {
                "running" => TurnStatusState::Running,
                "done" => TurnStatusState::Done,
                "error" => TurnStatusState::Error,
                _ => TurnStatusState::Cancelled,
            },
            error_code: t.error_code,
            assistant_message_id: t.assistant_message_id,
            updated_at: t.updated_at,
        }
    }
}

/// Request DTO for `PATCH /chats/{id}/turns/{request_id}` (edit).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct EditTurnRequest {
    pub content: String,
}

// --------------------------------------------------------------- stream --

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

/// Metadata about a thread summary applied to the current turn's context.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ThreadSummaryInfo {
    /// Estimated token cost of the summary in the context window.
    pub token_estimate: u32,
}

/// Stream header event.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

/// Kind of a `delta` chunk.
#[derive(Debug, Clone, Copy)]
#[toolkit_macros::api_dto(response)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

/// Delta text chunk.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct DeltaData {
    #[serde(rename = "type")]
    pub kind: DeltaKind,
    pub content: String,
}

/// Lifecycle phase of a tool invocation within a stream.
#[derive(Debug, Clone, Copy)]
#[toolkit_macros::api_dto(response)]
pub enum ToolPhase {
    Start,
    Done,
}

/// Tool lifecycle event.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ToolData {
    pub phase: ToolPhase,
    pub name: String,
    #[schema(value_type = Object)]
    pub details: serde_json::Value,
}

/// Whether a citation came from a file or web search.
#[derive(Debug, Clone, Copy)]
#[toolkit_macros::api_dto(response)]
pub enum CitationSource {
    File,
    Web,
}

/// A character span within response text.
#[derive(Debug, Clone, Copy)]
#[toolkit_macros::api_dto(response)]
pub struct TextSpan {
    pub start: usize,
    pub end: usize,
}

/// A citation extracted from provider annotations.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

impl From<DomainCitation> for Citation {
    fn from(c: DomainCitation) -> Self {
        Self {
            source: if c.web {
                CitationSource::Web
            } else {
                CitationSource::File
            },
            title: c.title,
            url: c.url,
            attachment_id: c.attachment_id,
            snippet: c.snippet,
            span: c.span.map(|(start, end)| TextSpan { start, end }),
            score: None,
        }
    }
}

/// Citations from provider annotations.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct CitationsData {
    pub items: Vec<Citation>,
}

/// Token usage counters.
#[derive(Debug, Clone, Copy)]
#[toolkit_macros::api_dto(response)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Quota decision reported in `done`.
#[derive(Debug, Clone, Copy)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaDecisionKind {
    Allow,
    Downgrade,
}

/// Per-tier, per-period quota warning entry in the SSE `done` event.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaWarning {
    pub tier: QuotaTier,
    pub period: QuotaPeriod,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    /// RFC 3339 timestamp of the next quota-period reset.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "ts::option::serialize"
    )]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub next_reset: Option<OffsetDateTime>,
}

impl From<DomainWarning> for QuotaWarning {
    fn from(w: DomainWarning) -> Self {
        Self {
            tier: tier_dto(w.tier),
            period: period_dto(w.period),
            remaining_percentage: u32::from(w.remaining_percentage),
            warning: w.warning,
            exhausted: w.exhausted,
            next_reset: w.next_reset,
        }
    }
}

/// Successful stream completion.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
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

impl From<DoneInfo> for DoneData {
    fn from(d: DoneInfo) -> Self {
        Self {
            usage: Usage {
                input_tokens: d.input_tokens,
                output_tokens: d.output_tokens,
            },
            effective_model: d.effective_model,
            selected_model: d.selected_model,
            quota_decision: if d.downgrade {
                QuotaDecisionKind::Downgrade
            } else {
                QuotaDecisionKind::Allow
            },
            downgrade_from: d.downgrade_from,
            downgrade_reason: d.downgrade_reason,
            quota_warnings: d
                .quota_warnings
                .map(|w| w.into_iter().map(Into::into).collect()),
        }
    }
}

/// Stream error (terminal).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
}

/// Payload of the `ping` event: an empty JSON object.
#[derive(Debug, Clone, Default)]
#[toolkit_macros::api_dto(response)]
#[allow(clippy::empty_structs_with_brackets)]
pub struct PingData {}

/// `OpenAPI` description of one SSE event of the `messages:stream`, retry
/// and edit responses: `event: <name>` and `data: <payload>`.
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

#[cfg(test)]
#[path = "dto_tests.rs"]
mod dto_tests;
