//! REST / SSE DTOs (shapes from the generated `OpenAPI`, `contracts/openapi-mini-chat.json`).

use base64::Engine as _;
use serde::Serialize;
use time::OffsetDateTime;
use toolkit_macros::api_dto;
use uuid::Uuid;

use crate::domain::service::chats::ChatView;
use crate::domain::service::messages::{AttachmentSummaryView, MessageView, Thumbnail};
use crate::domain::service::quota::PeriodStatus;
use crate::domain::service::reactions::ReactionView;
use crate::domain::service::turns::TurnStatusView;

#[api_dto(request)]
#[derive(Debug, Clone)]
pub struct CreateChatReq {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[api_dto(request)]
#[derive(Debug, Clone)]
pub struct UpdateChatReq {
    pub title: String,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct ChatDetailDto {
    pub id: Uuid,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub is_temporary: bool,
    pub message_count: i64,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: OffsetDateTime,
}

impl From<ChatView> for ChatDetailDto {
    fn from(c: ChatView) -> Self {
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

#[api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRoleDto {
    User,
    Assistant,
    System,
}

#[api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKindDto {
    Document,
    Image,
}

#[api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentStatusDto {
    Pending,
    Uploaded,
    Ready,
    Failed,
}

#[api_dto(request, response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionKindDto {
    Like,
    Dislike,
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
        "uploaded" => AttachmentStatusDto::Uploaded,
        "ready" => AttachmentStatusDto::Ready,
        "failed" => AttachmentStatusDto::Failed,
        _ => AttachmentStatusDto::Pending,
    }
}

fn reaction_dto(s: &str) -> Option<ReactionKindDto> {
    match s {
        "like" => Some(ReactionKindDto::Like),
        "dislike" => Some(ReactionKindDto::Dislike),
        _ => None,
    }
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct ImgThumbnailDto {
    pub content_type: String,
    pub width: i32,
    pub height: i32,
    pub data_base64: String,
}

impl From<Thumbnail> for ImgThumbnailDto {
    fn from(t: Thumbnail) -> Self {
        Self {
            content_type: t.content_type,
            width: t.width,
            height: t.height,
            data_base64: base64::engine::general_purpose::STANDARD.encode(t.data),
        }
    }
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct AttachmentSummaryDto {
    pub attachment_id: Uuid,
    pub kind: AttachmentKindDto,
    pub filename: String,
    pub status: AttachmentStatusDto,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub img_thumbnail: Option<ImgThumbnailDto>,
}

impl From<AttachmentSummaryView> for AttachmentSummaryDto {
    fn from(a: AttachmentSummaryView) -> Self {
        Self {
            attachment_id: a.attachment_id,
            kind: kind_dto(&a.kind),
            filename: a.filename,
            status: status_dto(&a.status),
            img_thumbnail: a.img_thumbnail.map(Into::into),
        }
    }
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct MiniChatMessageDto {
    pub id: Uuid,
    pub request_id: Uuid,
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
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<MessageView> for MiniChatMessageDto {
    fn from(m: MessageView) -> Self {
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
            my_reaction: m.my_reaction.as_deref().and_then(reaction_dto),
            model: m.model,
            input_tokens: (m.input_tokens > 0).then_some(m.input_tokens),
            output_tokens: (m.output_tokens > 0).then_some(m.output_tokens),
            created_at: m.created_at,
        }
    }
}

#[api_dto(request)]
#[derive(Debug, Clone)]
pub struct SetReactionReq {
    pub reaction: String,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct MiniChatReactionDto {
    pub message_id: Uuid,
    pub reaction: ReactionKindDto,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
}

impl From<ReactionView> for MiniChatReactionDto {
    fn from(r: ReactionView) -> Self {
        Self {
            message_id: r.message_id,
            reaction: reaction_dto(&r.reaction).unwrap_or(ReactionKindDto::Like),
            created_at: r.created_at,
        }
    }
}

#[api_dto(request)]
#[derive(Debug, Clone)]
pub struct WebSearchConfig {
    pub enabled: bool,
}

#[api_dto(request)]
#[derive(Debug, Clone)]
pub struct StreamMessageRequest {
    pub content: String,
    #[serde(default)]
    pub request_id: Option<Uuid>,
    #[serde(default)]
    pub attachment_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    pub web_search: Option<WebSearchConfig>,
}

#[api_dto(request)]
#[derive(Debug, Clone)]
pub struct EditTurnRequest {
    pub content: String,
}

#[api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatusState {
    Running,
    Done,
    Error,
    Cancelled,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct TurnStatusResponse {
    pub request_id: Uuid,
    pub state: TurnStatusState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assistant_message_id: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: OffsetDateTime,
}

impl From<TurnStatusView> for TurnStatusResponse {
    fn from(t: TurnStatusView) -> Self {
        Self {
            request_id: t.request_id,
            state: match t.state {
                "done" => TurnStatusState::Done,
                "error" => TurnStatusState::Error,
                "cancelled" => TurnStatusState::Cancelled,
                _ => TurnStatusState::Running,
            },
            error_code: t.error_code,
            assistant_message_id: t.assistant_message_id,
            updated_at: t.updated_at,
        }
    }
}

#[api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTierDto {
    Standard,
    Premium,
}

#[api_dto(response)]
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

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct ModelListDto {
    pub items: Vec<ModelDto>,
}

#[api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaTier {
    Premium,
    Total,
}

#[api_dto(response)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaPeriod {
    Daily,
    Monthly,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaPeriodStatus {
    pub period: QuotaPeriod,
    pub limit_credits_micro: i64,
    pub used_credits_micro: i64,
    pub remaining_credits_micro: i64,
    pub remaining_percentage: u32,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaTierStatus {
    pub tier: QuotaTier,
    pub periods: Vec<QuotaPeriodStatus>,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaStatusResponse {
    pub tiers: Vec<QuotaTierStatus>,
    pub warning_threshold_pct: u32,
}

impl QuotaStatusResponse {
    #[must_use]
    pub fn from_periods(periods: Vec<PeriodStatus>, warning_threshold_pct: u8) -> Self {
        let mut tiers: Vec<QuotaTierStatus> = Vec::new();
        for p in periods {
            let tier = if p.tier == "premium" {
                QuotaTier::Premium
            } else {
                QuotaTier::Total
            };
            let entry = QuotaPeriodStatus {
                period: if p.period.as_str() == "daily" {
                    QuotaPeriod::Daily
                } else {
                    QuotaPeriod::Monthly
                },
                limit_credits_micro: p.limit,
                used_credits_micro: p.used,
                remaining_credits_micro: p.remaining,
                remaining_percentage: p.remaining_percentage,
                next_reset: p.next_reset,
                warning: p.warning,
                exhausted: p.exhausted,
            };
            if let Some(t) = tiers.iter_mut().find(|t| t.tier == tier) {
                t.periods.push(entry);
            } else {
                tiers.push(QuotaTierStatus {
                    tier,
                    periods: vec![entry],
                });
            }
        }
        Self {
            tiers,
            warning_threshold_pct: u32::from(warning_threshold_pct),
        }
    }
}

// ----- SSE event schemas (OpenAPI only; events are written as JSON values) -----

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct ThreadSummaryInfo {
    pub token_estimate: u32,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct StreamStartedData {
    pub request_id: Uuid,
    pub message_id: Uuid,
    pub is_new_turn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_summary_applied: Option<ThreadSummaryInfo>,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
#[allow(
    clippy::empty_structs_with_brackets,
    reason = "must serialize as an empty JSON object `{}`, not `null`"
)]
pub struct PingData {}

#[api_dto(response)]
#[derive(Debug, Clone, Copy)]
pub enum DeltaKind {
    Text,
    Reasoning,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct DeltaData {
    #[serde(rename = "type")]
    pub kind: DeltaKind,
    pub content: String,
}

#[api_dto(response)]
#[derive(Debug, Clone, Copy)]
pub enum ToolPhase {
    Start,
    Done,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct ToolData {
    pub phase: ToolPhase,
    pub name: String,
    #[schema(value_type = Object)]
    pub details: serde_json::Value,
}

#[api_dto(response)]
#[derive(Debug, Clone, Copy)]
pub enum CitationSource {
    File,
    Web,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct TextSpan {
    pub start: u64,
    pub end: u64,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct Citation {
    pub source: CitationSource,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<TextSpan>,
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct CitationsData {
    pub items: Vec<Citation>,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[api_dto(response)]
#[derive(Debug, Clone, Copy)]
pub enum QuotaDecisionKind {
    Allow,
    Downgrade,
}

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct QuotaWarning {
    pub tier: QuotaTier,
    pub period: QuotaPeriod,
    pub remaining_percentage: u32,
    pub warning: bool,
    pub exhausted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub next_reset: Option<String>,
}

#[api_dto(response)]
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

#[api_dto(response)]
#[derive(Debug, Clone)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
}

/// One SSE event (`event:` name + JSON `data:`).
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
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

impl From<TurnStatusView> for TurnStatusState {
    fn from(t: TurnStatusView) -> Self {
        TurnStatusResponse::from(t).state
    }
}
