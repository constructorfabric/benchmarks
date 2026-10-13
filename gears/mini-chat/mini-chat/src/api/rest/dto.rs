//! REST DTOs.

use base64::Engine as _;
use mini_chat_sdk::ModelCatalogEntry;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::attachment::AttachmentDetail;
use crate::domain::chat::ChatDetail;
use crate::domain::message::{AttachmentSummary, MessageView, ReactionView, Thumbnail};
use crate::domain::quota::{PeriodBreakdown, QuotaStatus, TierStatus};
use crate::domain::turn::TurnStatus;

/// Request body of `POST /v1/chats`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct CreateChatRequest {
    /// Optional title: trimmed, 1 to 255 characters. Absent or `null` creates an untitled chat.
    #[serde(default)]
    pub title: Option<String>,
    /// Optional model id; defaults to the first enabled `is_default` model, else the first
    /// enabled model.
    #[serde(default)]
    pub model: Option<String>,
}

/// Request body of `PATCH /v1/chats/{id}`. Unknown fields (such as `model`) are ignored.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct UpdateChatRequest {
    /// The new title: trimmed, 1 to 255 characters.
    pub title: String,
}

/// A chat as returned by the API. Identity (`user_id`, `tenant_id`) is never exposed; `title` is
/// omitted when the chat has none.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ChatDetailDto {
    /// Chat identifier.
    pub id: Uuid,
    /// The selected model, locked for the lifetime of the chat.
    pub model: String,
    /// Chat title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Temporary-chat flag (reserved).
    pub is_temporary: bool,
    /// Number of messages in the chat.
    pub message_count: u64,
    /// Creation time.
    #[serde(with = "mini_chat_sdk::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
    /// Last activity time.
    #[serde(with = "mini_chat_sdk::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: OffsetDateTime,
}

impl From<ChatDetail> for ChatDetailDto {
    fn from(detail: ChatDetail) -> Self {
        Self {
            id: detail.chat.id,
            model: detail.chat.model,
            title: detail.chat.title,
            is_temporary: detail.chat.is_temporary,
            message_count: detail.message_count,
            created_at: detail.chat.created_at,
            updated_at: detail.chat.updated_at,
        }
    }
}

/// The public projection of a catalog model: no provider routing, credit multipliers, token limits
/// other than the context window, default flag or estimation budgets.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ModelDto {
    /// Stable internal model identifier.
    pub model_id: String,
    /// User-facing name.
    pub display_name: String,
    /// Rate-limit tier: `standard` or `premium`.
    pub tier: String,
    /// Human-readable credit multiplier.
    pub multiplier_display: String,
    /// User-facing help text; omitted when none is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Capability flags; clients ignore unknown values.
    pub multimodal_capabilities: Vec<String>,
    /// Maximum context window in tokens.
    pub context_window: u32,
}

impl From<ModelCatalogEntry> for ModelDto {
    fn from(entry: ModelCatalogEntry) -> Self {
        Self {
            model_id: entry.id,
            display_name: entry.display_name,
            tier: entry.tier.as_str().to_owned(),
            multiplier_display: entry.multiplier_display,
            description: Some(entry.description).filter(|text| !text.is_empty()),
            multimodal_capabilities: entry.multimodal_capabilities,
            context_window: entry.context_window,
        }
    }
}

/// Response of `GET /v1/models`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ModelListResponse {
    /// The globally enabled models.
    pub items: Vec<ModelDto>,
}

/// Web search control of a send request.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct WebSearchOptions {
    /// Include the `web_search` tool in the provider request.
    #[serde(default)]
    pub enabled: bool,
}

/// Request body of `POST /v1/chats/{id}/messages:stream`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct SendMessageRequest {
    /// The user message; empty or whitespace-only content is rejected.
    pub content: String,
    /// Idempotency key (any UUID version); the server generates a UUID v4 when omitted.
    #[serde(default)]
    pub request_id: Option<Uuid>,
    /// Attachments referenced by the message.
    #[serde(default)]
    pub attachment_ids: Option<Vec<Uuid>>,
    /// Web search control; defaults to `{ "enabled": false }`.
    #[serde(default)]
    pub web_search: Option<WebSearchOptions>,
}

/// Request body of `PATCH /v1/chats/{id}/turns/{request_id}`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct EditTurnRequest {
    /// The new user message; empty or whitespace-only content is rejected.
    pub content: String,
}

/// Documentation of the events of the `messages:stream` response: `stream_started`, `delta`,
/// `tool`, `citations`, `done`, `error` and `ping`, each with its own JSON data.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct StreamEventDoc {
    /// The event name.
    pub event: String,
    /// The event data (see the SSE event definitions of the contract).
    pub data: serde_json::Value,
}

/// A server-generated preview of an image attachment.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ThumbnailDto {
    /// Always `image/webp`.
    pub content_type: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// The encoded preview, base64.
    pub data_base64: String,
}

/// An attachment of a message.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentSummaryDto {
    /// The attachment id.
    pub attachment_id: Uuid,
    /// `document` or `image`.
    pub kind: String,
    /// The filename.
    pub filename: String,
    /// `pending`, `uploaded`, `ready` or `failed`.
    pub status: String,
    /// The preview of a ready image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ThumbnailDto>,
}

impl From<Thumbnail> for ThumbnailDto {
    fn from(thumbnail: Thumbnail) -> Self {
        Self {
            content_type: "image/webp".to_owned(),
            width: thumbnail.width,
            height: thumbnail.height,
            data_base64: base64::engine::general_purpose::STANDARD.encode(thumbnail.data),
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
            img_thumbnail: summary.thumbnail.map(ThumbnailDto::from),
        }
    }
}

/// An attachment as `GET` and the upload return it. Null members are omitted; `doc_summary` and
/// `summary_updated_at` are never populated (ADR-0007) and therefore never present.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct AttachmentDetailDto {
    /// The attachment id.
    pub id: Uuid,
    /// The filename.
    pub filename: String,
    /// The MIME type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    /// The size in bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<i64>,
    /// `pending`, `uploaded`, `ready` or `failed`.
    pub status: String,
    /// `document` or `image`.
    pub kind: String,
    /// The stable failure reason; present only when `status` is `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// The preview of a ready image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ThumbnailDto>,
    /// Upload time.
    #[serde(with = "mini_chat_sdk::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<AttachmentDetail> for AttachmentDetailDto {
    fn from(detail: AttachmentDetail) -> Self {
        Self {
            id: detail.id,
            filename: detail.filename,
            content_type: detail.content_type,
            size_bytes: detail.size_bytes,
            status: detail.status,
            kind: detail.kind.as_str().to_owned(),
            error_code: detail.error_code,
            img_thumbnail: detail.thumbnail.map(ThumbnailDto::from),
            created_at: detail.created_at,
        }
    }
}

/// A message of a chat.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct MessageDto {
    /// Message id.
    pub id: Uuid,
    /// The turn correlation key; user and assistant message of a turn share it.
    pub request_id: Uuid,
    /// `user`, `assistant` or `system`.
    pub role: String,
    /// The text.
    pub content: String,
    /// The model that produced an assistant message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider-reported input tokens; omitted when 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    /// Provider-reported output tokens; omitted when 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    /// Non-deleted attachments; always present.
    pub attachments: Vec<AttachmentSummaryDto>,
    /// The caller's reaction (`like`, `dislike`) or `null`; always present.
    pub my_reaction: Option<String>,
    /// Creation time.
    #[serde(with = "mini_chat_sdk::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<MessageView> for MessageDto {
    fn from(message: MessageView) -> Self {
        Self {
            id: message.id,
            request_id: message.request_id,
            role: message.role.as_str().to_owned(),
            content: message.content,
            model: message.model,
            input_tokens: Some(message.input_tokens).filter(|tokens| *tokens != 0),
            output_tokens: Some(message.output_tokens).filter(|tokens| *tokens != 0),
            attachments: message
                .attachments
                .into_iter()
                .map(AttachmentSummaryDto::from)
                .collect(),
            my_reaction: message
                .my_reaction
                .map(|reaction| reaction.as_str().to_owned()),
            created_at: message.created_at,
        }
    }
}

/// Request body of `PUT /v1/chats/{id}/messages/{msg_id}/reaction`.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct SetReactionRequest {
    /// `like` or `dislike`.
    pub reaction: String,
}

/// A stored reaction.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct ReactionDto {
    /// The reacted message.
    pub message_id: Uuid,
    /// `like` or `dislike`.
    pub reaction: String,
    /// When the reaction was set.
    #[serde(with = "mini_chat_sdk::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<ReactionView> for ReactionDto {
    fn from(view: ReactionView) -> Self {
        Self {
            message_id: view.message_id,
            reaction: view.reaction.as_str().to_owned(),
            created_at: view.created_at,
        }
    }
}

/// The authoritative state of a turn.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct TurnStatusDto {
    /// The turn's request id.
    pub request_id: Uuid,
    /// `running`, `done`, `error` or `cancelled`.
    pub state: String,
    /// The terminal error code of a failed turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// The persisted assistant message (`done`, or `cancelled` with partial content).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    /// Last update.
    #[serde(with = "mini_chat_sdk::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: OffsetDateTime,
}

impl From<TurnStatus> for TurnStatusDto {
    fn from(status: TurnStatus) -> Self {
        Self {
            request_id: status.request_id,
            state: status.state.to_owned(),
            error_code: status.error_code,
            assistant_message_id: status.assistant_message_id,
            updated_at: status.updated_at,
        }
    }
}

/// One period of a tier in the quota status.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaPeriodDto {
    /// `daily` or `monthly`.
    pub period: String,
    /// The limit of the tier in the period, in micro-credits.
    pub limit_credits_micro: i64,
    /// Spent plus reserved credits (conservative).
    pub used_credits_micro: i64,
    /// The limit minus the used credits, never negative.
    pub remaining_credits_micro: i64,
    /// Floored remaining percentage of the limit (0..=100).
    pub remaining_percentage: i64,
    /// Start of the next period (midnight UTC tomorrow, or the 1st of the next month).
    #[serde(with = "mini_chat_sdk::time_serde")]
    #[schema(value_type = String, format = DateTime)]
    pub next_reset: OffsetDateTime,
    /// The remaining percentage reached the warning threshold.
    pub warning: bool,
    /// Nothing (less than 1 percent) remains.
    pub exhausted: bool,
}

impl From<PeriodBreakdown> for QuotaPeriodDto {
    fn from(period: PeriodBreakdown) -> Self {
        Self {
            period: period.period.to_owned(),
            limit_credits_micro: period.limit_credits_micro,
            used_credits_micro: period.used_credits_micro,
            remaining_credits_micro: period.remaining_credits_micro,
            remaining_percentage: period.remaining_percentage,
            next_reset: period.next_reset,
            warning: period.warning,
            exhausted: period.exhausted,
        }
    }
}

/// The periods of one tier in the quota status.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaTierDto {
    /// `premium` or `total`.
    pub tier: String,
    /// The periods with a positive limit.
    pub periods: Vec<QuotaPeriodDto>,
}

impl From<TierStatus> for QuotaTierDto {
    fn from(tier: TierStatus) -> Self {
        Self {
            tier: tier.tier.to_owned(),
            periods: tier.periods.into_iter().map(QuotaPeriodDto::from).collect(),
        }
    }
}

/// `GET /v1/quota/status`: the caller's remaining quota per tier and period.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct QuotaStatusDto {
    /// `premium` and `total`.
    pub tiers: Vec<QuotaTierDto>,
    /// `quota.warning_threshold_pct`.
    pub warning_threshold_pct: u8,
}

impl From<QuotaStatus> for QuotaStatusDto {
    fn from(status: QuotaStatus) -> Self {
        Self {
            tiers: status.tiers.into_iter().map(QuotaTierDto::from).collect(),
            warning_threshold_pct: status.warning_threshold_pct,
        }
    }
}
