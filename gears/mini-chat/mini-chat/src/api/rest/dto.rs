//! REST DTOs of the Models API and the chat endpoints (the target `OpenAPI` subset: `ModelDto`,
//! `ModelListDto`, `CreateChatReq`, `UpdateChatReq`, `ChatDetailDto`).

use base64::Engine;
use mini_chat_sdk::{ModelTier, QuotaDecision};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::attachments::{AttachmentKind, AttachmentRecord, AttachmentStatus};
use crate::domain::chats::ChatDetail;
use crate::domain::messages::{AttachmentSummary, MessageView};
use crate::domain::model_visibility::VisibleModel;
use crate::domain::quota::{
    PeriodStatus, PeriodType, QuotaStatus, QuotaTier, QuotaWarning, TierStatus,
};
use crate::domain::streaming::{
    CitationItem, CitationSource, CitationsPayload, Delta, DeltaKind as DomainDeltaKind, Done,
    RawSendBody, StreamStarted, TextSpan, ToolEvent, ToolPhase,
};
use crate::domain::turns::TurnStatusView;

/// Rate-limit tier on the wire: the lowercase string `standard` or `premium`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ModelTierDto {
    Standard,
    Premium,
}

/// One model of the Models API: exactly the seven public fields.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ModelDto {
    /// Stable internal model id, the same value used in `POST /v1/chats`.
    pub model_id: String,
    pub display_name: String,
    pub tier: ModelTierDto,
    /// Human-readable credit multiplier, for example `1x`.
    pub multiplier_display: String,
    /// Omitted when no description is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub multimodal_capabilities: Vec<String>,
    pub context_window: u32,
}

/// Response of `GET /v1/models`: a single `items` array and no pagination fields.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ModelListDto {
    pub items: Vec<ModelDto>,
}

impl From<ModelTier> for ModelTierDto {
    fn from(tier: ModelTier) -> Self {
        match tier {
            ModelTier::Standard => Self::Standard,
            ModelTier::Premium => Self::Premium,
        }
    }
}

impl From<VisibleModel> for ModelDto {
    fn from(model: VisibleModel) -> Self {
        Self {
            model_id: model.model_id,
            display_name: model.display_name,
            tier: model.tier.into(),
            multiplier_display: model.multiplier_display,
            description: model.description,
            multimodal_capabilities: model.multimodal_capabilities,
            context_window: model.context_window,
        }
    }
}

/// Body of `POST /v1/chats`: both fields optional, `null` allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct CreateChatReq {
    pub title: Option<String>,
    pub model: Option<String>,
}

/// Body of `PATCH /v1/chats/{id}`: `title` is required and must be a string; unknown fields
/// are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct UpdateChatReq {
    pub title: String,
}

/// The chat as the create, get, update and list operations return it: the required fields
/// `id`, `model`, `is_temporary`, `message_count`, `created_at` and `updated_at` (RFC 3339), and
/// `title` only for a titled chat (omitted, never `null`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ChatDetailDto {
    pub id: Uuid,
    pub model: String,
    // @cpt-begin:cpt-cf-mini-chat-algo-chat-management-chat-detail-projection:p1:inst-projection-title
    // omitted from the JSON when the chat is untitled, never sent as `null`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    // @cpt-end:cpt-cf-mini-chat-algo-chat-management-chat-detail-projection:p1:inst-projection-title
    pub is_temporary: bool,
    pub message_count: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

/// The filterable and orderable fields of `GET /v1/chats` (`x-odata-filter`, `x-odata-orderby`
/// of the reference `OpenAPI`), in the order of the reference. It only declares the field set;
/// the list query itself runs over the same fields in the persistence layer.
#[derive(toolkit_odata_macros::ODataFilterable)]
#[toolkit_macros::api_dto(response)]
pub struct ChatDto {
    #[odata(filter(kind = "DateTimeUtc"))]
    pub updated_at: OffsetDateTime,
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    #[odata(filter(kind = "String"))]
    pub title: String,
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

/// The tier of a quota status entry: `premium` or `total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaTierDto {
    Premium,
    Total,
}

/// The period of a quota status entry: `daily` or `monthly`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaPeriodDto {
    Daily,
    Monthly,
}

/// One period of one tier (`QuotaPeriodStatus` of the reference `OpenAPI`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaPeriodStatusDto {
    pub period: QuotaPeriodDto,
    pub limit_credits_micro: i64,
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: i32,
    #[serde(with = "time::serde::rfc3339")]
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// One tier with its periods (`QuotaTierStatus`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierStatusDto {
    pub tier: QuotaTierDto,
    pub periods: Vec<QuotaPeriodStatusDto>,
}

/// Response of `GET /v1/quota/status` (`QuotaStatusResponse`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusResponseDto {
    pub tiers: Vec<QuotaTierStatusDto>,
    pub warning_threshold_pct: i32,
}

impl From<QuotaTier> for QuotaTierDto {
    fn from(tier: QuotaTier) -> Self {
        match tier {
            QuotaTier::Premium => Self::Premium,
            QuotaTier::Total => Self::Total,
        }
    }
}

impl From<PeriodType> for QuotaPeriodDto {
    fn from(period: PeriodType) -> Self {
        match period {
            PeriodType::Daily => Self::Daily,
            PeriodType::Monthly => Self::Monthly,
        }
    }
}

impl From<PeriodStatus> for QuotaPeriodStatusDto {
    fn from(status: PeriodStatus) -> Self {
        Self {
            period: status.period.into(),
            limit_credits_micro: status.limit_credits_micro,
            used_credits_micro: status.used_credits_micro,
            remaining_credits_micro: status.remaining_credits_micro,
            remaining_percentage: i32::from(status.remaining_percentage),
            next_reset: status.next_reset,
            warning: status.warning,
            exhausted: status.exhausted,
        }
    }
}

impl From<TierStatus> for QuotaTierStatusDto {
    fn from(status: TierStatus) -> Self {
        Self {
            tier: status.tier.into(),
            periods: status
                .periods
                .into_iter()
                .map(QuotaPeriodStatusDto::from)
                .collect(),
        }
    }
}

impl From<QuotaStatus> for QuotaStatusResponseDto {
    fn from(status: QuotaStatus) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-return
        // `tiers` and `warning_threshold_pct` only: billing outcomes, settlement details and
        // the web search and code interpreter counters are not part of the response
        Self {
            tiers: status
                .tiers
                .into_iter()
                .map(QuotaTierStatusDto::from)
                .collect(),
            warning_threshold_pct: i32::from(status.warning_threshold_pct),
        }
        // @cpt-end:cpt-cf-mini-chat-flow-quota-and-billing-quota-status:p1:inst-status-return
    }
}

// ── messages list, turn status and send (feature message-streaming, slice S1) ──────────────

/// The thumbnail of an image attachment summary (`ImgThumbnailDto`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ImgThumbnailDto {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data_base64: String,
}

/// Lightweight attachment metadata embedded in a message (`AttachmentSummaryDto`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentSummaryDto {
    pub attachment_id: Uuid,
    pub kind: String,
    pub filename: String,
    pub status: String,
    /// Only for an image with status `ready` and a stored thumbnail (omitted otherwise).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

/// `AttachmentKindDto`: the attachment kind on the wire, `document` or `image`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum AttachmentKindDto {
    Document,
    Image,
}

/// `AttachmentStatusDto`: the attachment lifecycle status on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum AttachmentStatusDto {
    Pending,
    Uploaded,
    Ready,
    Failed,
}

impl From<AttachmentKind> for AttachmentKindDto {
    fn from(kind: AttachmentKind) -> Self {
        match kind {
            AttachmentKind::Document => Self::Document,
            AttachmentKind::Image => Self::Image,
        }
    }
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

/// The attachment detail of `POST /v1/chats/{id}/attachments` (201) and
/// `GET /v1/chats/{id}/attachments/{attachment_id}` (`AttachmentDetailDto`): `id`, `filename`,
/// `content_type`, `size_bytes`, `status`, `kind` and `created_at` always; `error_code` only for a
/// `failed` row; `img_thumbnail` only for a `ready` image with a stored thumbnail; never
/// `doc_summary`, `summary_updated_at` or any internal or provider field.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentDetailDto {
    pub id: Uuid,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub status: AttachmentStatusDto,
    pub kind: AttachmentKindDto,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
    /// Declared by the contract and never populated (ADR-0007): always omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_summary: Option<String>,
    /// Declared by the contract and never populated (ADR-0007): always omitted.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub summary_updated_at: Option<OffsetDateTime>,
}

// @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection:p1:inst-proj-required
impl From<AttachmentRecord> for AttachmentDetailDto {
    fn from(record: AttachmentRecord) -> Self {
        let ready_image =
            record.status == AttachmentStatus::Ready && record.kind == AttachmentKind::Image;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection:p1:inst-proj-error-code
        // the `error_code` only for a failed row
        let error_code = if record.status == AttachmentStatus::Failed {
            record.error_code
        } else {
            None
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection:p1:inst-proj-error-code
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection:p1:inst-proj-thumbnail
        // only for a ready image with stored thumbnail bytes
        let img_thumbnail = if ready_image {
            record.thumbnail.map(|thumbnail| ImgThumbnailDto {
                content_type: "image/webp".to_owned(),
                width: thumbnail.width,
                height: thumbnail.height,
                data_base64: base64::engine::general_purpose::STANDARD.encode(thumbnail.bytes),
            })
        } else {
            None
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection:p1:inst-proj-thumbnail
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection:p1:inst-proj-omit-null
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection:p1:inst-proj-no-internal
        // `provider_file_id`, `secondary_file_id`, `storage_backend`, the purpose flags, the
        // cleanup columns, `uploaded_by_user_id` and every vector store identifier are not
        // copied; `doc_summary` and `summary_updated_at` stay null and are omitted
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection:p1:inst-proj-return
        Self {
            id: record.id,
            filename: record.filename,
            content_type: record.content_type,
            size_bytes: record.size_bytes,
            status: record.status.into(),
            kind: record.kind.into(),
            created_at: record.created_at,
            error_code,
            img_thumbnail,
            doc_summary: None,
            summary_updated_at: None,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection:p1:inst-proj-return
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection:p1:inst-proj-no-internal
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection:p1:inst-proj-omit-null
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection:p1:inst-proj-required

/// One message of `GET /v1/chats/{id}/messages` (`MiniChatMessageDto`): `id`, `request_id`,
/// `role`, `content`, `attachments` (always an array), `my_reaction` (always present, possibly
/// `null`) and `created_at`; `model`, `input_tokens` and `output_tokens` are omitted when absent.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatMessageDto {
    pub id: Uuid,
    pub request_id: Uuid,
    pub role: String,
    pub content: String,
    pub attachments: Vec<AttachmentSummaryDto>,
    /// `like`, `dislike` or `null`; always present.
    #[schema(required = true)]
    pub my_reaction: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
}

/// The filterable and orderable fields of `GET /v1/chats/{id}/messages` (`x-odata-filter`,
/// `x-odata-orderby` of the reference `OpenAPI`): `created_at`, `id` and `role`.
#[derive(toolkit_odata_macros::ODataFilterable)]
#[toolkit_macros::api_dto(response)]
pub struct MessageDto {
    #[odata(filter(kind = "DateTimeUtc"))]
    pub created_at: OffsetDateTime,
    #[odata(filter(kind = "Uuid"))]
    pub id: Uuid,
    #[odata(filter(kind = "String"))]
    pub role: String,
}

/// Response of `GET /v1/chats/{id}/turns/{request_id}` (`TurnStatusResponse`): no `chat_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct TurnStatusResponse {
    pub request_id: Uuid,
    /// `running`, `done`, `error` or `cancelled`.
    pub state: String,
    /// Only for `error` (omitted otherwise).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// For `done` and for `cancelled` with a persisted partial message (omitted otherwise).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
}

// @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-request
/// `web_search` of the send body.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct WebSearchConfig {
    pub enabled: bool,
}

/// Body of `POST /v1/chats/{id}/messages:stream`; unknown fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct StreamMessageRequest {
    pub content: String,
    pub request_id: Option<Uuid>,
    pub attachment_ids: Option<Vec<Uuid>>,
    pub web_search: Option<WebSearchConfig>,
}
// @cpt-end:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-request

// @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-content-only
/// Body of `PATCH /v1/chats/{id}/turns/{request_id}`: only `content`; unknown fields are ignored,
/// so the client chooses neither the new `request_id` nor the attachments of the replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct EditTurnRequest {
    pub content: String,
}
// @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation:p1:inst-ev-content-only

/// Body of `PUT /v1/chats/{id}/messages/{msg_id}/reaction`: `like` or `dislike`; the value is
/// checked by the domain (a wrong value is 400 `INVALID_REACTION`, a missing field is 422).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct SetReactionReq {
    pub reaction: String,
}

/// The reaction kind on the wire: `like` or `dislike`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ReactionKindDto {
    Like,
    Dislike,
}

/// Response of `PUT /v1/chats/{id}/messages/{msg_id}/reaction` (`MiniChatReactionDto`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct MiniChatReactionDto {
    pub message_id: Uuid,
    pub reaction: ReactionKindDto,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

impl From<crate::domain::mutations::StoredReaction> for MiniChatReactionDto {
    fn from(stored: crate::domain::mutations::StoredReaction) -> Self {
        Self {
            message_id: stored.message_id,
            reaction: match stored.reaction {
                crate::domain::messages::Reaction::Like => ReactionKindDto::Like,
                crate::domain::messages::Reaction::Dislike => ReactionKindDto::Dislike,
            },
            created_at: stored.created_at,
        }
    }
}

/// `data` of the SSE event `stream_started`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    /// `{"token_estimate": n}`, only for a new turn that used a thread summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

/// `thread_summary_applied` of `stream_started`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct ThreadSummaryInfo {
    pub token_estimate: i32,
}

/// `data` of the SSE event `delta`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct DeltaData {
    /// `text` or `reasoning`.
    #[serde(rename = "type")]
    pub kind: DeltaKindDto,
    pub content: String,
}

/// Kind of a `delta` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum DeltaKindDto {
    Text,
    Reasoning,
}

/// Token usage counters of `done`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct UsageDto {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Quota decision reported in `done`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum QuotaDecisionKindDto {
    Allow,
    Downgrade,
}

/// Per-tier, per-period quota warning entry of `done`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaWarningDto {
    pub tier: QuotaTierDto,
    pub period: QuotaPeriodDto,
    pub remaining_percentage: i32,
    pub warning: bool,
    pub exhausted: bool,
    /// Present when `warning` or `exhausted` is true.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub next_reset: Option<OffsetDateTime>,
}

/// `data` of the SSE event `done`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct DoneData {
    pub usage: UsageDto,
    pub effective_model: String,
    pub selected_model: String,
    pub quota_decision: QuotaDecisionKindDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downgrade_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quota_warnings: Option<Vec<QuotaWarningDto>>,
}

/// Phase of a `tool` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum ToolPhaseDto {
    Start,
    Done,
}

/// `data` of the SSE event `tool`.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct ToolData {
    pub phase: ToolPhaseDto,
    pub name: String,
    pub details: serde_json::Value,
}

/// Source of a citation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub enum CitationSourceDto {
    File,
    Web,
}

/// A character span `{start, end}` of the answer text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct TextSpanDto {
    pub start: usize,
    pub end: usize,
}

/// One item of `citations`: no score and no provider identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct CitationDto {
    pub source: CitationSourceDto,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TextSpanDto>,
}

/// `data` of the SSE event `citations`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct CitationsData {
    pub items: Vec<CitationDto>,
}

impl From<ToolEvent> for ToolData {
    fn from(event: ToolEvent) -> Self {
        Self {
            phase: match event.phase {
                ToolPhase::Start => ToolPhaseDto::Start,
                ToolPhase::Done => ToolPhaseDto::Done,
            },
            name: event.name,
            details: event.details,
        }
    }
}

impl From<CitationItem> for CitationDto {
    fn from(item: CitationItem) -> Self {
        Self {
            source: match item.source {
                CitationSource::File => CitationSourceDto::File,
                CitationSource::Web => CitationSourceDto::Web,
            },
            title: item.title,
            url: item.url,
            attachment_id: item.attachment_id,
            snippet: item.snippet,
            span: item
                .span
                .map(|TextSpan { start, end }| TextSpanDto { start, end }),
        }
    }
}

impl From<CitationsPayload> for CitationsData {
    fn from(payload: CitationsPayload) -> Self {
        Self {
            items: payload.items.into_iter().map(CitationDto::from).collect(),
        }
    }
}

impl From<MessageView> for MiniChatMessageDto {
    fn from(view: MessageView) -> Self {
        // the DTO has no tenant, chat, deletion, compression, provider or estimate field
        Self {
            id: view.id,
            request_id: view.request_id,
            role: view.role.as_str().to_owned(),
            content: view.content,
            attachments: view
                .attachments
                .into_iter()
                .map(AttachmentSummaryDto::from)
                .collect(),
            my_reaction: view
                .my_reaction
                .map(|reaction| reaction.as_str().to_owned()),
            created_at: view.created_at,
            model: view.model,
            input_tokens: view.input_tokens,
            output_tokens: view.output_tokens,
        }
    }
}

impl From<AttachmentSummary> for AttachmentSummaryDto {
    fn from(summary: AttachmentSummary) -> Self {
        Self {
            attachment_id: summary.attachment_id,
            kind: summary.kind,
            filename: summary.filename,
            status: summary.status,
            img_thumbnail: summary.img_thumbnail.map(|thumbnail| ImgThumbnailDto {
                content_type: thumbnail.content_type,
                width: thumbnail.width,
                height: thumbnail.height,
                data_base64: base64::engine::general_purpose::STANDARD.encode(thumbnail.data),
            }),
        }
    }
}

impl From<TurnStatusView> for TurnStatusResponse {
    fn from(view: TurnStatusView) -> Self {
        // absent optional fields are omitted by the serializer, never sent as `null`
        Self {
            request_id: view.request_id,
            state: view.state.as_str().to_owned(),
            error_code: view.error_code,
            assistant_message_id: view.assistant_message_id,
            updated_at: view.updated_at,
        }
    }
}

impl From<StreamMessageRequest> for RawSendBody {
    fn from(body: StreamMessageRequest) -> Self {
        // an absent or `null` `web_search` is not requested, unknown body fields were ignored
        // by the extractor
        Self {
            content: body.content,
            request_id: body.request_id,
            attachment_ids: body.attachment_ids,
            web_search_enabled: body.web_search.map(|web_search| web_search.enabled),
        }
    }
}

impl From<StreamStarted> for StreamStartedData {
    fn from(started: StreamStarted) -> Self {
        Self {
            request_id: started.request_id,
            message_id: started.message_id,
            is_new_turn: started.is_new_turn,
            thread_summary_applied: started
                .thread_summary_token_estimate
                .map(|token_estimate| ThreadSummaryInfo { token_estimate }),
        }
    }
}

impl From<Delta> for DeltaData {
    fn from(delta: Delta) -> Self {
        Self {
            kind: match delta.kind {
                DomainDeltaKind::Text => DeltaKindDto::Text,
                DomainDeltaKind::Reasoning => DeltaKindDto::Reasoning,
            },
            content: delta.content,
        }
    }
}

impl From<QuotaWarning> for QuotaWarningDto {
    fn from(warning: QuotaWarning) -> Self {
        Self {
            tier: warning.tier.into(),
            period: warning.period.into(),
            remaining_percentage: i32::from(warning.remaining_percentage),
            warning: warning.warning,
            exhausted: warning.exhausted,
            next_reset: warning.next_reset,
        }
    }
}

impl From<Done> for DoneData {
    fn from(done: Done) -> Self {
        Self {
            usage: UsageDto {
                input_tokens: done.input_tokens,
                output_tokens: done.output_tokens,
            },
            effective_model: done.effective_model,
            selected_model: done.selected_model,
            quota_decision: match done.quota_decision {
                // `unknown` exists only on the orphan audit path and never reaches `done`
                QuotaDecision::Allow | QuotaDecision::Unknown => QuotaDecisionKindDto::Allow,
                QuotaDecision::Downgrade => QuotaDecisionKindDto::Downgrade,
            },
            downgrade_from: done.downgrade_from,
            downgrade_reason: done.downgrade_reason,
            quota_warnings: done
                .quota_warnings
                .map(|warnings| warnings.into_iter().map(QuotaWarningDto::from).collect()),
        }
    }
}

#[cfg(test)]
#[path = "dto_tests.rs"]
mod dto_tests;
