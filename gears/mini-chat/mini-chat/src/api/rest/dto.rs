//! REST DTOs of the Models API and of the chat CRUD feature.

use base64::Engine as _;
use mini_chat_sdk::ModelTier;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::service::attachments::model::{
    AttachmentDetail, AttachmentKind, AttachmentStatus, Thumbnail,
};
use crate::domain::service::chats::ChatDetail;
use crate::domain::service::messages::{
    AttachmentSummary, ImgThumbnail, MessageView, Reaction, ReactionView,
};
use crate::domain::service::models::ModelView;
use crate::domain::service::quota::status::{
    QuotaPeriodView, QuotaStatusView, QuotaTierView, QuotaWarningView,
};
use crate::domain::service::quota::types::{
    QuotaPeriod as DomainQuotaPeriod, QuotaTier as DomainQuotaTier,
};

/// Rate-limit tier of a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ModelTierDto {
    /// Premium tier.
    Premium,
    /// Standard tier.
    Standard,
}

/// One model of the catalog as seen by a user.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ModelDto {
    /// Stable model identifier.
    pub model_id: String,
    /// User-facing name.
    pub display_name: String,
    /// Rate-limit tier.
    pub tier: ModelTierDto,
    /// Informational multiplier label.
    pub multiplier_display: String,
    /// Help text; omitted when empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Capability flags.
    pub multimodal_capabilities: Vec<String>,
    /// Maximum context window in tokens.
    pub context_window: u32,
}

/// Response of `GET /v1/models`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ModelListDto {
    /// Enabled models in catalog order.
    pub items: Vec<ModelDto>,
}

impl From<ModelView> for ModelDto {
    fn from(view: ModelView) -> Self {
        Self {
            model_id: view.model_id,
            display_name: view.display_name,
            tier: match view.tier {
                ModelTier::Premium => ModelTierDto::Premium,
                ModelTier::Standard => ModelTierDto::Standard,
            },
            multiplier_display: view.multiplier_display,
            description: view.description,
            multimodal_capabilities: view.multimodal_capabilities,
            context_window: view.context_window,
        }
    }
}

/// Body of `POST /v1/chats`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct CreateChatReq {
    /// Chat title; absent or `null` creates an untitled chat.
    #[serde(default)]
    pub title: Option<String>,
    /// Model id; absent or `null` selects the default model.
    #[serde(default)]
    pub model: Option<String>,
}

/// Body of `PATCH /v1/chats/{id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct UpdateChatReq {
    /// The new title.
    pub title: String,
}

/// Body of `PUT /v1/chats/{id}/messages/{msg_id}/reaction`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct SetReactionReq {
    /// `like` or `dislike`.
    pub reaction: String,
}

/// A chat with its message count.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ChatDetailDto {
    /// Chat id.
    pub id: Uuid,
    /// Model of the chat.
    pub model: String,
    /// Title, omitted when the chat has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Always `false` in P1.
    pub is_temporary: bool,
    /// Non-deleted messages.
    pub message_count: u64,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Last activity time.
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<ChatDetail> for ChatDetailDto {
    fn from(detail: ChatDetail) -> Self {
        Self {
            id: detail.id,
            model: detail.model,
            title: detail.title,
            is_temporary: detail.is_temporary,
            message_count: detail.message_count,
            created_at: detail.created_at,
            updated_at: detail.updated_at,
        }
    }
}

/// Thumbnail of a ready image.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ImgThumbnailDto {
    /// Always `image/webp`.
    pub content_type: String,
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// Standard base64 of the WebP bytes.
    pub data_base64: String,
}

impl From<ImgThumbnail> for ImgThumbnailDto {
    fn from(thumbnail: ImgThumbnail) -> Self {
        Self {
            content_type: "image/webp".to_owned(),
            width: thumbnail.width,
            height: thumbnail.height,
            data_base64: base64::engine::general_purpose::STANDARD.encode(thumbnail.data),
        }
    }
}

/// Kind of an attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum AttachmentKindDto {
    /// A document.
    Document,
    /// An image.
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

/// Lifecycle status of an attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum AttachmentStatusDto {
    /// Row inserted, provider upload not stored yet.
    Pending,
    /// The provider file id is stored, processing continues.
    Uploaded,
    /// Usable in a message.
    Ready,
    /// Terminal failure.
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

/// Full details of an attachment (`POST` 201 and `GET`). Optional keys are
/// omitted, never `null`; `doc_summary` and `summary_updated_at` are part of the
/// schema but are never sent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentDetailDto {
    /// Attachment id.
    pub id: Uuid,
    /// File name.
    pub filename: String,
    /// Stored normalized MIME type.
    pub content_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
    /// Lifecycle status.
    pub status: AttachmentStatusDto,
    /// Kind.
    pub kind: AttachmentKindDto,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Only with `status` `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Only for a ready image with a stored thumbnail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
    /// Never present (document summaries are not implemented).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_summary: Option<String>,
    /// Never present.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub summary_updated_at: Option<OffsetDateTime>,
}

impl From<AttachmentDetail> for AttachmentDetailDto {
    fn from(detail: AttachmentDetail) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-project-detail:p1:inst-non-exposure
        Self {
            id: detail.id,
            filename: detail.filename,
            content_type: detail.content_type,
            size_bytes: detail.size_bytes,
            status: detail.status.into(),
            kind: detail.kind.into(),
            created_at: detail.created_at,
            error_code: detail.error_code,
            img_thumbnail: detail
                .thumbnail
                .map(|thumbnail: Thumbnail| ImgThumbnailDto {
                    content_type: "image/webp".to_owned(),
                    width: i32::try_from(thumbnail.width).unwrap_or(i32::MAX),
                    height: i32::try_from(thumbnail.height).unwrap_or(i32::MAX),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(thumbnail.bytes),
                }),
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-project-detail:p1:inst-omit-optional
            doc_summary: None,
            summary_updated_at: None,
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-project-detail:p1:inst-omit-optional
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-project-detail:p1:inst-non-exposure
    }
}

/// Attachment of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentSummaryDto {
    /// Attachment id.
    pub attachment_id: Uuid,
    /// `document` or `image`.
    pub kind: String,
    /// File name.
    pub filename: String,
    /// Upload status.
    pub status: String,
    /// Only for a ready image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

impl From<AttachmentSummary> for AttachmentSummaryDto {
    fn from(summary: AttachmentSummary) -> Self {
        Self {
            attachment_id: summary.attachment_id,
            kind: summary.kind,
            filename: summary.filename,
            status: summary.status,
            img_thumbnail: summary.img_thumbnail.map(ImgThumbnailDto::from),
        }
    }
}

/// A message of the history.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatMessageDto {
    /// Message id.
    pub id: Uuid,
    /// Request id.
    pub request_id: Uuid,
    /// `user`, `assistant` or `system`.
    pub role: String,
    /// Content.
    pub content: String,
    /// Attachments, `[]` when none.
    pub attachments: Vec<AttachmentSummaryDto>,
    /// The caller's reaction, `null` when none.
    pub my_reaction: Option<String>,
    /// Creation time.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Effective model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Input tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    /// Output tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
}

impl From<MessageView> for MiniChatMessageDto {
    fn from(view: MessageView) -> Self {
        Self {
            id: view.id,
            request_id: view.request_id,
            role: view.role,
            content: view.content,
            attachments: view
                .attachments
                .into_iter()
                .map(AttachmentSummaryDto::from)
                .collect(),
            my_reaction: view.my_reaction.map(|r: Reaction| r.as_str().to_owned()),
            created_at: view.created_at,
            model: view.model,
            input_tokens: view.input_tokens,
            output_tokens: view.output_tokens,
        }
    }
}

/// The stored reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatReactionDto {
    /// Reacted message.
    pub message_id: Uuid,
    /// `like` or `dislike`.
    pub reaction: String,
    /// Time of the `PUT`.
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<ReactionView> for MiniChatReactionDto {
    fn from(view: ReactionView) -> Self {
        Self {
            message_id: view.message_id,
            reaction: view.reaction.as_str().to_owned(),
            created_at: view.created_at,
        }
    }
}

/// Tier of a quota entry: `premium` sub-cap or the `total` cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaTier {
    /// Premium sub-cap.
    Premium,
    /// Overall cap across all tiers.
    Total,
}

/// Period of a quota entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaPeriod {
    /// UTC calendar day.
    Daily,
    /// UTC calendar month.
    Monthly,
}

impl From<DomainQuotaTier> for QuotaTier {
    fn from(tier: DomainQuotaTier) -> Self {
        match tier {
            DomainQuotaTier::Premium => Self::Premium,
            DomainQuotaTier::Total => Self::Total,
        }
    }
}

impl From<DomainQuotaPeriod> for QuotaPeriod {
    fn from(period: DomainQuotaPeriod) -> Self {
        match period {
            DomainQuotaPeriod::Daily => Self::Daily,
            DomainQuotaPeriod::Monthly => Self::Monthly,
        }
    }
}

/// One period of one tier of the quota status.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaPeriodStatus {
    /// `daily` or `monthly`.
    pub period: QuotaPeriod,
    /// Limit in micro-credits.
    pub limit_credits_micro: i64,
    /// Spent plus reserved in micro-credits (may exceed the limit).
    pub used_credits_micro: i64,
    /// Limit minus used, at least 0.
    pub remaining_credits_micro: i64,
    /// Remaining share of the limit, floored, 0 to 100.
    pub remaining_percentage: u8,
    /// Next reset (RFC 3339 UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub next_reset: OffsetDateTime,
    /// The remaining share reached the warning threshold.
    pub warning: bool,
    /// Less than 1% of the limit remains.
    pub exhausted: bool,
}

impl From<QuotaPeriodView> for QuotaPeriodStatus {
    fn from(view: QuotaPeriodView) -> Self {
        Self {
            period: view.period.into(),
            limit_credits_micro: view.limit_credits_micro,
            used_credits_micro: view.used_credits_micro,
            remaining_credits_micro: view.remaining_credits_micro,
            remaining_percentage: view.remaining_percentage,
            next_reset: view.next_reset,
            warning: view.warning,
            exhausted: view.exhausted,
        }
    }
}

/// One tier of the quota status.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatus {
    /// `premium` or `total`.
    pub tier: QuotaTier,
    /// Daily then monthly; periods without a positive limit are omitted.
    pub periods: Vec<QuotaPeriodStatus>,
}

impl From<QuotaTierView> for QuotaTierStatus {
    fn from(view: QuotaTierView) -> Self {
        Self {
            tier: view.tier.into(),
            periods: view
                .periods
                .into_iter()
                .map(QuotaPeriodStatus::from)
                .collect(),
        }
    }
}

/// Response of `GET /v1/quota/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponse {
    /// `premium` then `total`.
    pub tiers: Vec<QuotaTierStatus>,
    /// The configured `quota.warning_threshold_pct`.
    pub warning_threshold_pct: u8,
}

impl From<QuotaStatusView> for QuotaStatusResponse {
    fn from(view: QuotaStatusView) -> Self {
        Self {
            tiers: view.tiers.into_iter().map(QuotaTierStatus::from).collect(),
            warning_threshold_pct: view.warning_threshold_pct,
        }
    }
}

/// One entry of `quota_warnings` of the SSE `done` event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaWarning {
    /// `premium` or `total`.
    pub tier: QuotaTier,
    /// `daily` or `monthly`.
    pub period: QuotaPeriod,
    /// Remaining share of the limit, floored, 0 to 100.
    pub remaining_percentage: u8,
    /// The remaining share reached the warning threshold.
    pub warning: bool,
    /// Less than 1% of the limit remains.
    pub exhausted: bool,
    /// Next reset (RFC 3339 UTC); present only when `warning` or `exhausted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_reset: Option<OffsetDateTime>,
}

impl From<QuotaWarningView> for QuotaWarning {
    fn from(view: QuotaWarningView) -> Self {
        Self {
            tier: view.tier.into(),
            period: view.period.into(),
            remaining_percentage: view.remaining_percentage,
            warning: view.warning,
            exhausted: view.exhausted,
            next_reset: view.next_reset,
        }
    }
}

// ---- streaming turns ----------------------------------------------------------------------------

/// `web_search` option of the send request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct WebSearchConfig {
    /// Enable web search for this message.
    pub enabled: bool,
}

/// Body of `PATCH /v1/chats/{id}/turns/{request_id}` (edit). The new text is
/// stored as sent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct EditTurnRequest {
    /// The replacement user message text.
    pub content: String,
}

/// Body of `POST /v1/chats/{id}/messages:stream`. There is no `model` field:
/// the turn runs on the model of the chat.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct StreamMessageRequest {
    /// Message text; sent and stored as given.
    pub content: String,
    /// Idempotency key (any UUID version); generated when absent.
    #[serde(default)]
    pub request_id: Option<Uuid>,
    /// Attachments of the message; empty equals absent.
    #[serde(default)]
    pub attachment_ids: Option<Vec<Uuid>>,
    /// Web search option; absent or `null` means disabled.
    #[serde(default)]
    pub web_search: Option<WebSearchConfig>,
}

/// `thread_summary_applied` of `stream_started`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ThreadSummaryInfo {
    /// Token estimate of the kept summary.
    pub token_estimate: u32,
}

/// `data` of the `stream_started` event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct StreamStartedData {
    /// Request id of the turn.
    pub request_id: Uuid,
    /// Assistant message id (the persisted one on replay).
    pub message_id: Uuid,
    /// `false` on replay.
    pub is_new_turn: bool,
    /// Present when a thread summary was kept in the context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

/// `data` of the `ping` event: `{}`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
#[allow(clippy::empty_structs_with_brackets)] // serializes as `{}`, a unit struct would be `null`
pub struct PingData {}

/// `type` of a `delta` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum DeltaType {
    /// Assistant text.
    Text,
    /// Model reasoning (`<think>` text of the vLLM adapter).
    Reasoning,
}

/// `data` of the `delta` event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct DeltaData {
    /// `text` or `reasoning`.
    #[serde(rename = "type")]
    pub kind: DeltaType,
    /// The fragment.
    pub content: String,
}

/// `phase` of a `tool` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ToolPhase {
    /// The tool started.
    Start,
    /// The tool finished.
    Done,
}

/// `data` of the `tool` event.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct ToolData {
    /// `start` or `done`.
    pub phase: ToolPhase,
    /// `file_search`, `web_search`, `code_interpreter`, `function_call`,
    /// `search_knowledge`, `load_files` or `unknown_tool`.
    pub name: String,
    /// Tool details object.
    pub details: serde_json::Value,
}

/// `[start, end]` character span of a citation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct TextSpan {
    /// Start offset.
    pub start: u64,
    /// End offset.
    pub end: u64,
}

/// `source` of a citation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum CitationSourceDto {
    /// A chat attachment.
    File,
    /// A web page.
    Web,
}

/// One citation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct Citation {
    /// `file` or `web`.
    pub source: CitationSourceDto,
    /// Title.
    pub title: String,
    /// Snippet.
    pub snippet: String,
    /// Web URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Attachment id of a file citation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    /// Character span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TextSpan>,
}

/// `data` of the `citations` event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct CitationsData {
    /// The citations.
    pub items: Vec<Citation>,
}

/// Token usage of the `done` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
}

/// `quota_decision` of the `done` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaDecisionDto {
    /// The selected model ran.
    Allow,
    /// Another model ran.
    Downgrade,
}

/// `data` of the `done` event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct DoneData {
    /// Provider usage.
    pub usage: Usage,
    /// Model that ran.
    pub effective_model: String,
    /// Model of the chat.
    pub selected_model: String,
    /// `allow` or `downgrade`.
    pub quota_decision: QuotaDecisionDto,
    /// The selected model, when downgraded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    /// Why, when downgraded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    /// Quota warnings of a completed turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarning>>,
}

/// `data` of the `error` event.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ErrorData {
    /// Streaming error code.
    pub code: String,
    /// Sanitized text (not part of the contract).
    pub message: String,
}

/// Schema of the `data` of any SSE event of the send endpoint.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
#[serde(untagged)]
pub enum MiniChatSseEvent {
    /// `stream_started`
    StreamStarted(StreamStartedData),
    /// `ping`
    Ping(PingData),
    /// `delta`
    Delta(DeltaData),
    /// `tool`
    Tool(ToolData),
    /// `citations`
    Citations(CitationsData),
    /// `done`
    Done(DoneData),
    /// `error`
    Error(ErrorData),
}

/// Response of `GET /v1/chats/{id}/turns/{request_id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct TurnStatusResponse {
    /// Request id.
    pub request_id: Uuid,
    /// `running`, `done`, `error` or `cancelled`.
    pub state: String,
    /// Present only for `error` with a stored code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Present for `done`, and for `cancelled` with a persisted partial message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    /// Last update (RFC 3339 UTC).
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

impl From<crate::domain::service::stream::status::TurnStatus> for TurnStatusResponse {
    fn from(status: crate::domain::service::stream::status::TurnStatus) -> Self {
        Self {
            request_id: status.request_id,
            state: status.state.api_state().to_owned(),
            error_code: status.error_code,
            assistant_message_id: status.assistant_message_id,
            updated_at: status.updated_at,
        }
    }
}

#[cfg(test)]
#[path = "dto_attachments_tests.rs"]
mod attachments_tests;
